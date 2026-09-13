//! loglume CLI: filter syslog files with SQL WHERE clauses, powered by
//! `db-core`'s `StreamEngine`.
//!
//! Usage:
//!     loglume "severity >= WARN" app.log
//!     loglume "select * from log where severity >= 'WARN' and facility = 'kern'" app.log
//!     cat app.log | loglume "severity = ERROR"

use clap::Parser;
use loglume::{
    Cell, Engine, EngineError, Facility, QueryResult, ScopeReport, Severity, StreamEngine,
};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(name = "loglume")]
#[command(about = "Fast CLI log viewer with SQL filtering")]
#[command(version)]
struct Args {
    /// Filter: either a short form (e.g. "severity >= WARN") or full SQL
    /// (e.g. "select * from log where severity >= 'WARN'")
    #[arg(required = true)]
    filter: String,

    /// Log file to read (reads stdin if omitted)
    #[arg()]
    file: Option<PathBuf>,

    /// Maximum lines to process (0 = unlimited)
    #[arg(short = 'n', long, default_value = "0")]
    max_lines: usize,

    /// Show only the last N matching lines (like tail)
    #[arg(short = 't', long, default_value = "0")]
    tail: usize,

    /// Follow file for new lines (like tail -f)
    #[arg(short = 'f', long)]
    follow: bool,

    /// Scope of history to query, e.g. "1h", "1d", "100000 lines", "256mb"
    #[arg(long)]
    scope: Option<String>,
}

fn main() {
    let args = Args::parse();

    let result = run(&args);

    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn run(args: &Args) -> io::Result<()> {
    let sql = build_sql(&args.filter, args.scope.as_deref(), args.max_lines)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;

    match &args.file {
        Some(path) => {
            if args.follow {
                run_follow(path, &sql, args.tail)
            } else {
                run_once(path, &sql, args.tail)
            }
        }
        None => {
            if args.follow {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "--follow requires a file argument (stdin can't be followed)",
                ));
            }
            run_stdin(&sql, args.tail)
        }
    }
}

fn engine_err(e: EngineError) -> io::Error {
    io::Error::other(e.to_string())
}

fn open_engine(path: &Path) -> io::Result<StreamEngine> {
    StreamEngine::open(path).map_err(engine_err)
}

fn run_once(path: &Path, sql: &str, tail: usize) -> io::Result<()> {
    let mut engine = open_engine(path)?;
    let result = engine.run_query(sql).map_err(engine_err)?;
    print_result(&result, tail);
    Ok(())
}

fn run_stdin(sql: &str, tail: usize) -> io::Result<()> {
    use std::io::Read;

    let mut buffer = Vec::new();
    io::stdin().lock().read_to_end(&mut buffer)?;

    let tmp_path = std::env::temp_dir().join(format!("loglume-stdin-{}.log", std::process::id()));
    std::fs::write(&tmp_path, &buffer)?;
    let result = run_once(&tmp_path, sql, tail);
    let _ = std::fs::remove_file(&tmp_path);
    result
}

fn run_follow(path: &Path, sql: &str, tail: usize) -> io::Result<()> {
    use notify::{RecursiveMode, Watcher};
    use std::sync::mpsc;

    let mut engine = open_engine(path)?;
    let result = engine.run_query(sql).map_err(engine_err)?;
    print_result(&result, tail);
    let mut printed = result.rows.len();

    let (tx, rx) = mpsc::channel();
    let mut watcher = notify::recommended_watcher(move |res| {
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

        engine.refresh().map_err(engine_err)?;
        let result = engine.run_query(sql).map_err(engine_err)?;

        if result.rows.len() < printed {
            // File was truncated/rotated: engine.refresh() already reloaded
            // from scratch, so start printing from the top again.
            printed = 0;
        }

        if result.rows.len() == printed {
            // Nothing new (e.g. a duplicate filesystem event for one write).
            continue;
        }

        print_rows(&result, printed);
        print_footer(&result);
        printed = result.rows.len();
    }

    Ok(())
}

fn print_result(result: &QueryResult, tail: usize) {
    let start = if tail > 0 {
        result.rows.len().saturating_sub(tail)
    } else {
        0
    };
    print_rows(result, start);
    print_footer(result);
}

/// Print rows starting at `start`, using the "raw" column if present.
fn print_rows(result: &QueryResult, start: usize) {
    let raw_idx = result.columns.iter().position(|c| c == "raw");
    let stdout = io::stdout();
    let mut out = stdout.lock();

    for row in result.rows.iter().skip(start) {
        let line = raw_idx
            .and_then(|idx| row.get(idx))
            .and_then(|cell| match cell {
                Cell::Text(s) => Some(s.clone()),
                _ => None,
            });
        match line {
            Some(line) => {
                let _ = writeln!(out, "{line}");
            }
            None => {
                // Not a `SELECT *`-shaped result (e.g. an aggregate query);
                // fall back to printing every cell in the row.
                let cells: Vec<String> = row.iter().map(format_cell).collect();
                let _ = writeln!(out, "{}", cells.join("\t"));
            }
        }
    }
}

fn format_cell(cell: &Cell) -> String {
    match cell {
        Cell::Null => String::new(),
        Cell::Int(i) => i.to_string(),
        Cell::Real(r) => r.to_string(),
        Cell::Bool(b) => b.to_string(),
        Cell::Text(s) => s.clone(),
        Cell::Blob(b) => format!("<{} bytes>", b.len()),
    }
}

/// Print the effective-range footer to stderr, so stdout stays pipeable.
fn print_footer(result: &QueryResult) {
    let Some(report) = &result.scope_report else {
        return;
    };
    eprintln!("-- {}", format_scope_report(report));
}

fn format_scope_report(report: &ScopeReport) -> String {
    let range = match (report.first_ts, report.last_ts) {
        (Some(first), Some(last)) => {
            format!("{} -> {}", format_ts_ns(first), format_ts_ns(last))
        }
        _ => "no timestamps".to_string(),
    };
    let capped = if report.capped { ", capped" } else { "" };
    format!("{} lines, {range}{capped}", report.lines)
}

/// Format nanoseconds since the Unix epoch as an ISO-8601 UTC timestamp.
fn format_ts_ns(ns: i64) -> String {
    let secs = ns.div_euclid(1_000_000_000);
    let days = secs.div_euclid(86400);
    let secs_of_day = secs.rem_euclid(86400);
    let (year, month, day) = civil_from_days(days);
    let hh = secs_of_day / 3600;
    let mm = (secs_of_day % 3600) / 60;
    let ss = secs_of_day % 60;
    format!("{year:04}-{month:02}-{day:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

/// Days-since-epoch to (year, month, day), Howard Hinnant's `civil_from_days`.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

/// Rewrite loglume's short-form filter syntax, or pass raw SQL through.
///
/// Supports:
/// - `severity >= WARN`
/// - `severity = ERROR`
/// - `facility = kern`
/// - `severity >= WARN AND facility = auth`
/// - any full SQL `SELECT ...` statement, passed through untouched
fn build_sql(filter: &str, scope: Option<&str>, max_lines: usize) -> Result<String, String> {
    let mut sql = rewrite_filter_to_sql(filter)?;
    if let Some(scope) = scope {
        if scope.trim().is_empty() {
            return Err("--scope must not be empty".to_string());
        }
        sql.push_str(&format!(" SINCE {scope}"));
    }
    if max_lines > 0 {
        sql.push_str(&format!(" LIMIT {max_lines}"));
    }
    Ok(sql)
}

fn rewrite_filter_to_sql(filter: &str) -> Result<String, String> {
    let trimmed = filter.trim();
    if trimmed.len() >= 6 && trimmed[..6].eq_ignore_ascii_case("select") {
        return Ok(trimmed.to_string());
    }

    let where_clause = parse_where_clause(trimmed)?;
    Ok(format!("SELECT * FROM log WHERE {where_clause}"))
}

fn parse_where_clause(filter: &str) -> Result<String, String> {
    let filter = filter.trim();

    if let Some((left, right)) = filter.split_once(" AND ") {
        let left_sql = parse_single_clause(left.trim())?;
        let right_sql = parse_single_clause(right.trim())?;
        return Ok(format!("{left_sql} AND {right_sql}"));
    }
    if let Some((left, right)) = filter.split_once(" and ") {
        let left_sql = parse_single_clause(left.trim())?;
        let right_sql = parse_single_clause(right.trim())?;
        return Ok(format!("{left_sql} AND {right_sql}"));
    }

    parse_single_clause(filter)
}

/// Parse a single filter clause into a SQL WHERE fragment.
fn parse_single_clause(filter: &str) -> Result<String, String> {
    let filter = filter.trim();

    if let Some(rest) = filter.strip_prefix("severity") {
        let rest = rest.trim();

        for op in [">=", ">", "<=", "<", "="] {
            if let Some(level_str) = rest.strip_prefix(op) {
                let level_str = level_str.trim();
                Severity::parse(level_str)
                    .ok_or_else(|| format!("unrecognized severity level '{level_str}'"))?;
                return Ok(format!("severity {op} '{}'", level_str.to_uppercase()));
            }
        }

        return Err(format!("unrecognized severity operator in '{filter}'"));
    }

    if let Some(rest) = filter.strip_prefix("facility") {
        let rest = rest.trim();
        if let Some(name) = rest.strip_prefix("=") {
            let name = name.trim();
            let fac = parse_facility_name(name)
                .ok_or_else(|| format!("unrecognized facility name '{name}'"))?;
            return Ok(format!("facility = '{}'", facility_keyword(fac)));
        }
        return Err(format!("unrecognized facility operator in '{filter}'"));
    }

    Err(format!("unrecognized filter expression '{filter}'"))
}

/// Parse facility name (including common aliases) to the enum.
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

/// The canonical syslog keyword for a facility, as understood by db-core's
/// own SQL literal parsing (not loglume's own aliases like "kernel").
fn facility_keyword(fac: Facility) -> &'static str {
    match fac {
        Facility::Kern => "kern",
        Facility::User => "user",
        Facility::Mail => "mail",
        Facility::Daemon => "daemon",
        Facility::Auth => "auth",
        Facility::Syslog => "syslog",
        Facility::Lpr => "lpr",
        Facility::News => "news",
        Facility::Uucp => "uucp",
        Facility::Cron => "cron",
        Facility::AuthPriv => "authpriv",
        Facility::Ftp => "ftp",
        Facility::Local0 => "local0",
        Facility::Local1 => "local1",
        Facility::Local2 => "local2",
        Facility::Local3 => "local3",
        Facility::Local4 => "local4",
        Facility::Local5 => "local5",
        Facility::Local6 => "local6",
        Facility::Local7 => "local7",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn severity_ge_rewrites_to_sql() {
        let sql = rewrite_filter_to_sql("severity >= WARN").expect("valid filter");
        assert_eq!(sql, "SELECT * FROM log WHERE severity >= 'WARN'");
    }

    #[test]
    fn severity_gt_rewrites_to_sql() {
        let sql = rewrite_filter_to_sql("severity > ERR").expect("valid filter");
        assert_eq!(sql, "SELECT * FROM log WHERE severity > 'ERR'");
    }

    #[test]
    fn severity_le_rewrites_to_sql() {
        let sql = rewrite_filter_to_sql("severity <= ERR").expect("valid filter");
        assert_eq!(sql, "SELECT * FROM log WHERE severity <= 'ERR'");
    }

    #[test]
    fn severity_lt_rewrites_to_sql() {
        let sql = rewrite_filter_to_sql("severity < WARN").expect("valid filter");
        assert_eq!(sql, "SELECT * FROM log WHERE severity < 'WARN'");
    }

    #[test]
    fn severity_eq_rewrites_to_sql() {
        let sql = rewrite_filter_to_sql("severity = WARN").expect("valid filter");
        assert_eq!(sql, "SELECT * FROM log WHERE severity = 'WARN'");
    }

    #[test]
    fn facility_eq_rewrites_to_sql() {
        let sql = rewrite_filter_to_sql("facility = auth").expect("valid filter");
        assert_eq!(sql, "SELECT * FROM log WHERE facility = 'auth'");
    }

    #[test]
    fn facility_alias_rewrites_to_canonical_keyword() {
        let sql = rewrite_filter_to_sql("facility = kernel").expect("valid filter");
        assert_eq!(sql, "SELECT * FROM log WHERE facility = 'kern'");
    }

    #[test]
    fn and_combinator_uppercase_rewrites_to_sql() {
        let sql =
            rewrite_filter_to_sql("severity >= WARN AND facility = auth").expect("valid filter");
        assert_eq!(
            sql,
            "SELECT * FROM log WHERE severity >= 'WARN' AND facility = 'auth'"
        );
    }

    #[test]
    fn and_combinator_lowercase_rewrites_to_sql() {
        let sql =
            rewrite_filter_to_sql("severity >= WARN and facility = auth").expect("valid filter");
        assert_eq!(
            sql,
            "SELECT * FROM log WHERE severity >= 'WARN' AND facility = 'auth'"
        );
    }

    #[test]
    fn raw_sql_passes_through_untouched() {
        let sql = rewrite_filter_to_sql("select * from log where severity >= 'WARN'")
            .expect("valid filter");
        assert_eq!(sql, "select * from log where severity >= 'WARN'");
    }

    #[test]
    fn raw_sql_uppercase_select_passes_through() {
        let sql = rewrite_filter_to_sql("SELECT count(*) FROM log").expect("valid filter");
        assert_eq!(sql, "SELECT count(*) FROM log");
    }

    #[test]
    fn unrecognized_severity_level_is_an_error() {
        let err = match rewrite_filter_to_sql("severity >= BOGUS") {
            Err(e) => e,
            Ok(_) => panic!("expected error"),
        };
        assert!(err.contains("BOGUS"));
    }

    #[test]
    fn unrecognized_facility_name_is_an_error() {
        let err = match rewrite_filter_to_sql("facility = nope") {
            Err(e) => e,
            Ok(_) => panic!("expected error"),
        };
        assert!(err.contains("nope"));
    }

    #[test]
    fn unrecognized_filter_expression_is_an_error() {
        let err = match rewrite_filter_to_sql("bogus filter") {
            Err(e) => e,
            Ok(_) => panic!("expected error"),
        };
        assert!(err.contains("bogus filter"));
    }

    #[test]
    fn build_sql_appends_scope_and_limit() {
        let sql = build_sql("severity >= WARN", Some("1h"), 10).expect("valid");
        assert_eq!(
            sql,
            "SELECT * FROM log WHERE severity >= 'WARN' SINCE 1h LIMIT 10"
        );
    }

    #[test]
    fn build_sql_empty_scope_is_an_error() {
        let err = build_sql("severity >= WARN", Some("  "), 0).unwrap_err();
        assert!(err.contains("--scope"));
    }

    #[test]
    fn format_ts_ns_epoch() {
        assert_eq!(format_ts_ns(0), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn format_ts_ns_known_date() {
        // 2024-01-01T00:00:00Z = 1704067200 seconds since epoch.
        assert_eq!(
            format_ts_ns(1_704_067_200_000_000_000),
            "2024-01-01T00:00:00Z"
        );
    }
}
