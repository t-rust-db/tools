//! loglume CLI: filter syslog files with SQL WHERE clauses.
//!
//! Usage:
//!     loglume "severity >= WARN" app.log
//!     cat app.log | loglume "severity = ERROR"

use clap::Parser;
use loglume::{LogBatch, Severity, Source, SourceKind, SyslogParser};
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

    let source = Source::new(
        SourceKind::File,
        path.to_str().unwrap_or("unknown"),
    );

    let parser = SyslogParser::with_year(2024); // TODO: detect from file or system
    let filter_fn = parse_filter(filter);

    // Find start offset for tail mode
    let start_offset = if tail > 0 {
        find_tail_offset(&mmap, tail)
    } else {
        0
    };

    let mut offset = start_offset;
    let mut total_lines = 0;
    let limit = if max_lines == 0 { usize::MAX } else { max_lines };

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
    use std::thread;
    use std::time::Duration;

    let filter_fn = parse_filter(filter);
    let parser = SyslogParser::with_year(2024);
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

    // Poll for new content
    loop {
        thread::sleep(Duration::from_millis(100));

        let metadata = std::fs::metadata(path)?;
        let new_len = metadata.len() as usize;

        if new_len > pos {
            let mut file = File::open(path)?;
            use std::io::Seek;
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
        }
    }
}

fn process_stdin(filter: &str, max_lines: usize) -> io::Result<()> {
    use std::io::BufRead;

    let stdin = io::stdin();
    let source = Source::new(SourceKind::Stdin, "stdin");
    let parser = SyslogParser::with_year(2024);
    let filter_fn = parse_filter(filter);

    let limit = if max_lines == 0 { usize::MAX } else { max_lines };
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

/// Parse a simple filter expression.
///
/// Supports:
/// - `severity >= WARN`
/// - `severity = ERROR`
/// - `severity > INFO`
///
/// TODO: integrate with db-core parser for full SQL WHERE support.
fn parse_filter(filter: &str) -> Box<dyn Fn(&LogBatch<'_>, usize) -> bool> {
    let filter = filter.trim();

    // Try parsing "severity <op> <level>"
    if let Some(rest) = filter.strip_prefix("severity") {
        let rest = rest.trim();

        if let Some(level_str) = rest.strip_prefix(">=") {
            let level_str = level_str.trim();
            if let Some(level) = Severity::parse(level_str) {
                return Box::new(move |batch, i| {
                    batch.severity.get(i).and_then(|s| *s).map_or(false, |s| s >= level)
                });
            }
        }

        if let Some(level_str) = rest.strip_prefix(">") {
            let level_str = level_str.trim();
            if let Some(level) = Severity::parse(level_str) {
                return Box::new(move |batch, i| {
                    batch.severity.get(i).and_then(|s| *s).map_or(false, |s| s > level)
                });
            }
        }

        if let Some(level_str) = rest.strip_prefix("<=") {
            let level_str = level_str.trim();
            if let Some(level) = Severity::parse(level_str) {
                return Box::new(move |batch, i| {
                    batch.severity.get(i).and_then(|s| *s).map_or(false, |s| s <= level)
                });
            }
        }

        if let Some(level_str) = rest.strip_prefix("<") {
            let level_str = level_str.trim();
            if let Some(level) = Severity::parse(level_str) {
                return Box::new(move |batch, i| {
                    batch.severity.get(i).and_then(|s| *s).map_or(false, |s| s < level)
                });
            }
        }

        if let Some(level_str) = rest.strip_prefix("=") {
            let level_str = level_str.trim();
            if let Some(level) = Severity::parse(level_str) {
                return Box::new(move |batch, i| {
                    batch.severity.get(i).and_then(|s| *s).map_or(false, |s| s == level)
                });
            }
        }
    }

    // Fallback: match all (TODO: proper error handling)
    eprintln!("warning: unrecognized filter '{filter}', matching all lines");
    Box::new(|_, _| true)
}
