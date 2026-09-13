//! loglume CLI: filter syslog files with SQL WHERE clauses.
//!
//! Usage:
//!     loglume "severity >= WARN" app.log
//!     cat app.log | loglume "severity = ERROR"

use clap::Parser;
use loglume::{Facility, LogBatch, Severity, Source, SourceKind, SyslogParser};
use memmap2::Mmap;
use std::fs::File;
use std::io::{self, Read};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "loglume")]
#[command(about = "Fast CLI log viewer with SQL filtering")]
#[command(version)]
struct Args {
    /// SQL WHERE clause (e.g., "severity >= WARN")
    #[arg(required = true)]
    filter: String,

    /// Log file to read (reads stdin if omitted)
    #[arg()]
    file: Option<PathBuf>,

    /// Maximum lines to process (0 = unlimited)
    #[arg(short = 'n', long, default_value = "0")]
    max_lines: usize,

    /// Show last N lines (like tail)
    #[arg(short = 't', long, default_value = "0")]
    tail: usize,

    /// Follow file for new lines (like tail -f)
    #[arg(short = 'f', long)]
    follow: bool,
}

fn main() {
    let args = Args::parse();

    let result = if let Some(path) = &args.file {
        if args.follow {
            process_file_follow(path, &args.filter, args.tail)
        } else {
            process_file(path, &args.filter, args.max_lines, args.tail)
        }
    } else {
        process_stdin(&args.filter, args.max_lines)
    };

    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn process_file(path: &PathBuf, filter: &str, max_lines: usize, tail: usize) -> io::Result<()> {
    let file = File::open(path)?;
    // Safety: we only read the file, and it's opened read-only
    #[allow(unsafe_code)]
    let mmap = unsafe { Mmap::map(&file)? };

    let source = Source::new(SourceKind::File, path.to_str().unwrap_or("unknown"));

    let parser = SyslogParser::new();
    let filter_fn =
        parse_filter(filter).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;

    // Find start offset for tail mode
    let start_offset = if tail > 0 {
        find_tail_offset(&mmap, tail)
    } else {
        0
    };

    let mut offset = start_offset;
    let mut total_lines = 0;
    let limit = if max_lines == 0 {
        usize::MAX
    } else {
        max_lines
    };

    while offset < mmap.len() && total_lines < limit {
        let remaining = mmap.get(offset..).unwrap_or(&[]);
        let (batch, consumed) = parser.parse_batch(source.clone(), remaining, 256);

        for i in 0..batch.len() {
            if filter_fn(&batch, i) {
                let raw = batch.raw_line(i);
                if let Ok(line) = std::str::from_utf8(raw) {
                    println!("{line}");
                }
            }
            total_lines = total_lines.saturating_add(1);
            if total_lines >= limit {
                break;
            }
        }

        if consumed == 0 {
            break;
        }
        offset = offset.saturating_add(consumed);
    }

    Ok(())
}

fn process_file_follow(path: &PathBuf, filter: &str, tail: usize) -> io::Result<()> {
    use notify::{RecursiveMode, Watcher};
    use std::io::Seek;
    use std::sync::mpsc;

    let filter_fn =
        parse_filter(filter).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    let parser = SyslogParser::new();
    let source = Source::new(SourceKind::File, path.to_str().unwrap_or("unknown"));

    // Initial read with tail
    let mut file = File::open(path)?;
    let mut buffer = Vec::new();
    file.read_to_end(&mut buffer)?;

    let start_offset = if tail > 0 {
        find_tail_offset(&buffer, tail)
    } else {
        0
    };

    // Process initial content
    let initial = buffer.get(start_offset..).unwrap_or(&[]);
    let (batch, _) = parser.parse_batch(source.clone(), initial, usize::MAX);
    for i in 0..batch.len() {
        if filter_fn(&batch, i) {
            if let Ok(line) = std::str::from_utf8(batch.raw_line(i)) {
                println!("{line}");
            }
        }
    }

    let mut pos = buffer.len();

    // Watch for filesystem writes instead of polling on a fixed interval.
    let (tx, rx) = mpsc::channel();
    let mut watcher = notify::recommended_watcher(move |res| {
        // The receiving end may have gone away if we're shutting down; ignore send errors.
        let _ = tx.send(res);
    })
    .map_err(|e| io::Error::other(e.to_string()))?;
    watcher
        .watch(path, RecursiveMode::NonRecursive)
        .map_err(|e| io::Error::other(e.to_string()))?;

    for res in rx {
        let event = res.map_err(|e| io::Error::other(e.to_string()))?;
        if !event.kind.is_modify() && !event.kind.is_create() {
            continue;
        }

        let metadata = std::fs::metadata(path)?;
        let new_len = metadata.len() as usize;

        if new_len > pos {
            let mut file = File::open(path)?;
            file.seek(std::io::SeekFrom::Start(pos as u64))?;

            let mut new_data = Vec::new();
            file.read_to_end(&mut new_data)?;

            if !new_data.is_empty() {
                let (batch, consumed) = parser.parse_batch(source.clone(), &new_data, usize::MAX);
                for i in 0..batch.len() {
                    if filter_fn(&batch, i) {
                        if let Ok(line) = std::str::from_utf8(batch.raw_line(i)) {
                            println!("{line}");
                        }
                    }
                }
                pos = pos.saturating_add(consumed);
            }
        } else if new_len < pos {
            // File was truncated/replaced (log rotation): restart from the top.
            pos = 0;
        }
    }

    Ok(())
}

fn process_stdin(filter: &str, max_lines: usize) -> io::Result<()> {
    use std::io::BufRead;

    let stdin = io::stdin();
    let source = Source::new(SourceKind::Stdin, "stdin");
    let parser = SyslogParser::new();
    let filter_fn =
        parse_filter(filter).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;

    let limit = if max_lines == 0 {
        usize::MAX
    } else {
        max_lines
    };
    let mut lines_processed: usize = 0;

    // Read line-by-line for streaming support
    for line_result in stdin.lock().lines() {
        if lines_processed >= limit {
            break;
        }

        let line = line_result?;
        let line_bytes = line.as_bytes();
        let mut line_with_newline = line_bytes.to_vec();
        line_with_newline.push(b'\n');

        let (batch, _) = parser.parse_batch(source.clone(), &line_with_newline, 1);

        for i in 0..batch.len() {
            if filter_fn(&batch, i) {
                println!("{line}");
            }
        }

        lines_processed = lines_processed.saturating_add(1);
    }

    Ok(())
}

/// Find the byte offset to start reading the last N lines.
fn find_tail_offset(data: &[u8], n: usize) -> usize {
    if n == 0 || data.is_empty() {
        return 0;
    }

    // Scan backwards counting newlines
    let mut newlines_found = 0;
    let mut pos = data.len();

    // Skip trailing newline if present
    if pos > 0 && data.get(pos.saturating_sub(1)) == Some(&b'\n') {
        pos = pos.saturating_sub(1);
    }

    while pos > 0 && newlines_found < n {
        pos = pos.saturating_sub(1);
        if data.get(pos) == Some(&b'\n') {
            newlines_found = newlines_found.saturating_add(1);
        }
    }

    // If we found enough newlines, skip past the last one found
    if newlines_found >= n && pos > 0 {
        pos.saturating_add(1)
    } else {
        0 // Not enough lines, return start
    }
}

/// Parse a filter expression.
///
/// Supports:
/// - `severity >= WARN`
/// - `severity = ERROR`
/// - `facility = kern`
/// - `severity >= WARN AND facility = auth`
///
/// TODO: integrate with db-core parser for full SQL WHERE support.
type FilterPredicate = Box<dyn Fn(&LogBatch<'_>, usize) -> bool>;

fn parse_filter(filter: &str) -> Result<FilterPredicate, String> {
    let filter = filter.trim();

    // Handle AND combinator
    if let Some((left, right)) = filter.split_once(" AND ") {
        let left_fn = parse_single_filter(left.trim())?;
        let right_fn = parse_single_filter(right.trim())?;
        return Ok(Box::new(move |batch, i| {
            left_fn(batch, i) && right_fn(batch, i)
        }));
    }
    if let Some((left, right)) = filter.split_once(" and ") {
        let left_fn = parse_single_filter(left.trim())?;
        let right_fn = parse_single_filter(right.trim())?;
        return Ok(Box::new(move |batch, i| {
            left_fn(batch, i) && right_fn(batch, i)
        }));
    }

    parse_single_filter(filter)
}

/// Parse a single filter clause.
fn parse_single_filter(filter: &str) -> Result<FilterPredicate, String> {
    let filter = filter.trim();

    // Try parsing "severity <op> <level>"
    if let Some(rest) = filter.strip_prefix("severity") {
        let rest = rest.trim();

        if let Some(level_str) = rest.strip_prefix(">=") {
            let level_str = level_str.trim();
            let level = Severity::parse(level_str)
                .ok_or_else(|| format!("unrecognized severity level '{level_str}'"))?;
            return Ok(Box::new(move |batch, i| {
                batch
                    .severity
                    .get(i)
                    .and_then(|s| *s)
                    .is_some_and(|s| s >= level)
            }));
        }

        if let Some(level_str) = rest.strip_prefix(">") {
            let level_str = level_str.trim();
            let level = Severity::parse(level_str)
                .ok_or_else(|| format!("unrecognized severity level '{level_str}'"))?;
            return Ok(Box::new(move |batch, i| {
                batch
                    .severity
                    .get(i)
                    .and_then(|s| *s)
                    .is_some_and(|s| s > level)
            }));
        }

        if let Some(level_str) = rest.strip_prefix("<=") {
            let level_str = level_str.trim();
            let level = Severity::parse(level_str)
                .ok_or_else(|| format!("unrecognized severity level '{level_str}'"))?;
            return Ok(Box::new(move |batch, i| {
                batch
                    .severity
                    .get(i)
                    .and_then(|s| *s)
                    .is_some_and(|s| s <= level)
            }));
        }

        if let Some(level_str) = rest.strip_prefix("<") {
            let level_str = level_str.trim();
            let level = Severity::parse(level_str)
                .ok_or_else(|| format!("unrecognized severity level '{level_str}'"))?;
            return Ok(Box::new(move |batch, i| {
                batch
                    .severity
                    .get(i)
                    .and_then(|s| *s)
                    .is_some_and(|s| s < level)
            }));
        }

        if let Some(level_str) = rest.strip_prefix("=") {
            let level_str = level_str.trim();
            let level = Severity::parse(level_str)
                .ok_or_else(|| format!("unrecognized severity level '{level_str}'"))?;
            return Ok(Box::new(move |batch, i| {
                batch.severity.get(i).and_then(|s| *s) == Some(level)
            }));
        }

        return Err(format!("unrecognized severity operator in '{filter}'"));
    }

    // Try parsing "facility = <name>"
    if let Some(rest) = filter.strip_prefix("facility") {
        let rest = rest.trim();
        if let Some(name) = rest.strip_prefix("=") {
            let name = name.trim();
            let fac = parse_facility_name(name)
                .ok_or_else(|| format!("unrecognized facility name '{name}'"))?;
            return Ok(Box::new(move |batch, i| {
                batch.facility.get(i).and_then(|f| *f) == Some(fac)
            }));
        }
        return Err(format!("unrecognized facility operator in '{filter}'"));
    }

    Err(format!("unrecognized filter expression '{filter}'"))
}

/// Parse facility name to enum.
fn parse_facility_name(name: &str) -> Option<Facility> {
    match name.to_ascii_lowercase().as_str() {
        "kern" | "kernel" => Some(Facility::Kern),
        "user" => Some(Facility::User),
        "mail" => Some(Facility::Mail),
        "daemon" => Some(Facility::Daemon),
        "auth" | "security" => Some(Facility::Auth),
        "syslog" => Some(Facility::Syslog),
        "lpr" => Some(Facility::Lpr),
        "news" => Some(Facility::News),
        "uucp" => Some(Facility::Uucp),
        "cron" => Some(Facility::Cron),
        "authpriv" => Some(Facility::AuthPriv),
        "ftp" => Some(Facility::Ftp),
        "local0" => Some(Facility::Local0),
        "local1" => Some(Facility::Local1),
        "local2" => Some(Facility::Local2),
        "local3" => Some(Facility::Local3),
        "local4" => Some(Facility::Local4),
        "local5" => Some(Facility::Local5),
        "local6" => Some(Facility::Local6),
        "local7" => Some(Facility::Local7),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse a single raw syslog line into a batch of one entry.
    fn batch_for(line: &str) -> (loglume::LogBatch<'static>, ()) {
        let raw: &'static str = Box::leak(format!("{line}\n").into_boxed_str());
        let parser = SyslogParser::new();
        let source = Source::new(SourceKind::File, "test");
        let (batch, _consumed) = parser.parse_batch(source, raw.as_bytes(), 1);
        (batch, ())
    }

    // facility=kern(0), severity=emerg(0) -> pri 0
    const KERN_EMERG: &str = "<0>Sep 9 08:00:00 host kernel[1]: panic";
    // facility=auth(4), severity=warning(4) -> pri 36
    const AUTH_WARNING: &str = "<36>Sep 9 08:00:00 host sshd[1]: bad login";
    // facility=daemon(3), severity=err(3) -> pri 27
    const DAEMON_ERR: &str = "<27>Sep 9 08:00:00 host nginx[1]: 500";
    // facility=user(1), severity=notice(5) -> pri 13 (less urgent than warning)
    const USER_NOTICE: &str = "<13>Sep 9 08:00:00 host app[1]: heads up";

    #[test]
    fn severity_ge_matches_equal_and_above() {
        let filter = parse_filter("severity >= WARN").expect("valid filter");
        let (batch, _) = batch_for(AUTH_WARNING);
        assert!(filter(&batch, 0));
        let (batch, _) = batch_for(KERN_EMERG);
        assert!(filter(&batch, 0));
    }

    #[test]
    fn severity_ge_rejects_below() {
        let filter = parse_filter("severity >= ERR").expect("valid filter");
        let (batch, _) = batch_for(AUTH_WARNING);
        assert!(!filter(&batch, 0));
    }

    // Severity's Ord follows urgency, not raw syslog numeric codes: EMERG is
    // the "greatest" severity, DEBUG the "least" (e.g. EMERG > ERR > WARNING).

    #[test]
    fn severity_gt() {
        let filter = parse_filter("severity > ERR").expect("valid filter");
        let (batch, _) = batch_for(KERN_EMERG);
        assert!(filter(&batch, 0));
        let (batch, _) = batch_for(DAEMON_ERR);
        assert!(!filter(&batch, 0));
    }

    #[test]
    fn severity_le() {
        let filter = parse_filter("severity <= ERR").expect("valid filter");
        let (batch, _) = batch_for(DAEMON_ERR);
        assert!(filter(&batch, 0));
        let (batch, _) = batch_for(KERN_EMERG);
        assert!(!filter(&batch, 0));
    }

    #[test]
    fn severity_lt() {
        let filter = parse_filter("severity < WARN").expect("valid filter");
        let (batch, _) = batch_for(USER_NOTICE);
        assert!(filter(&batch, 0));
        let (batch, _) = batch_for(AUTH_WARNING);
        assert!(!filter(&batch, 0));
    }

    #[test]
    fn severity_eq() {
        let filter = parse_filter("severity = WARN").expect("valid filter");
        let (batch, _) = batch_for(AUTH_WARNING);
        assert!(filter(&batch, 0));
        let (batch, _) = batch_for(DAEMON_ERR);
        assert!(!filter(&batch, 0));
    }

    #[test]
    fn facility_eq_matches_by_name() {
        let filter = parse_filter("facility = auth").expect("valid filter");
        let (batch, _) = batch_for(AUTH_WARNING);
        assert!(filter(&batch, 0));
        let (batch, _) = batch_for(KERN_EMERG);
        assert!(!filter(&batch, 0));
    }

    #[test]
    fn facility_eq_matches_alias() {
        let filter = parse_filter("facility = kernel").expect("valid filter");
        let (batch, _) = batch_for(KERN_EMERG);
        assert!(filter(&batch, 0));
    }

    #[test]
    fn and_combinator_uppercase() {
        let filter = parse_filter("severity >= WARN AND facility = auth").expect("valid filter");
        let (batch, _) = batch_for(AUTH_WARNING);
        assert!(filter(&batch, 0));
        let (batch, _) = batch_for(KERN_EMERG);
        assert!(!filter(&batch, 0));
    }

    #[test]
    fn and_combinator_lowercase() {
        let filter = parse_filter("severity >= WARN and facility = auth").expect("valid filter");
        let (batch, _) = batch_for(AUTH_WARNING);
        assert!(filter(&batch, 0));
    }

    #[test]
    fn unrecognized_severity_level_is_an_error() {
        let err = match parse_filter("severity >= BOGUS") {
            Err(e) => e,
            Ok(_) => panic!("expected error"),
        };
        assert!(err.contains("BOGUS"));
    }

    #[test]
    fn unrecognized_facility_name_is_an_error() {
        let err = match parse_filter("facility = nope") {
            Err(e) => e,
            Ok(_) => panic!("expected error"),
        };
        assert!(err.contains("nope"));
    }

    #[test]
    fn unrecognized_filter_expression_is_an_error() {
        let err = match parse_filter("bogus filter") {
            Err(e) => e,
            Ok(_) => panic!("expected error"),
        };
        assert!(err.contains("bogus filter"));
    }

    #[test]
    fn find_tail_offset_basic() {
        let data = b"a\nb\nc\nd\n";
        // Last 2 lines are "c\n" and "d\n" -> offset points at 'c'
        assert_eq!(find_tail_offset(data, 2), 4);
        assert_eq!(find_tail_offset(data, 0), 0);
        assert_eq!(find_tail_offset(data, 100), 0);
    }
}
