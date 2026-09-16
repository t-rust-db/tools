//! loglume CLI: filter syslog files with SQL WHERE clauses, powered by
//! `db-core`'s `StreamEngine`.
//!
//! Usage:
//!     loglume "severity >= WARN" app.log
//!     loglume "select * from log where severity >= 'WARN' and facility = 'kern'" app.log
//!     cat app.log | loglume "severity = ERROR"
//!     loglume "severity >= WARN" --tui a.log b.log   # side-by-side panes

use clap::Parser;
use loglume::{
    BinaryOp, Cell, CompiledPredicate, EmitMode, Engine, EngineError, Facility, QueryResult,
    ScopeReport, Severity, StandingQuery, StreamEngine,
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

    /// Log file(s) to read (reads stdin if omitted). Multiple files require
    /// --tui and open as side-by-side panes.
    #[arg()]
    files: Vec<PathBuf>,

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

    /// Open an interactive TUI viewer instead of printing to stdout
    #[arg(long)]
    tui: bool,

    /// Save the resolved filter/SQL under this name for later use as "@name"
    #[arg(long)]
    save_filter: Option<String>,

    /// Mark matching lines distinctly (bold/highlighted) without hiding
    /// non-matching ones -- restrict (--filter) narrows, this annotates.
    /// Accepts loglume's short forms or any db-core boolean expression.
    #[arg(long)]
    highlight: Option<String>,

    /// Run as a standing query on the resolved filter/SQL: fires on a
    /// transition (or a threshold crossing, with --alert-op/--alert-threshold)
    /// instead of printing results once. Requires --exec.
    #[arg(long)]
    alert: bool,

    /// Poll cadence for --alert (and its threshold hold duration), e.g.
    /// "1m", "30s", "1h". Default: 1m.
    #[arg(long, default_value = "1m")]
    window: String,

    /// Shell command to run when --alert fires; the fired rows are piped
    /// to its stdin, one per line.
    #[arg(long)]
    exec: Option<String>,

    /// Comparison operator for --alert's EmitMode::Threshold (e.g. ">=");
    /// requires --alert-threshold. Without this pair, --alert uses
    /// EmitMode::OnChange. Threshold mode requires the alert SQL to be a
    /// range-vector query (e.g. "... RANGE 10 seconds ...").
    #[arg(long)]
    alert_op: Option<String>,

    /// Threshold value for --alert's EmitMode::Threshold; requires
    /// --alert-op.
    #[arg(long)]
    alert_threshold: Option<f64>,
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
    // "loglume config" (no file args) is special-cased as a subcommand
    // rather than a real filter -- "config" was never a valid filter
    // expression before, so this isn't a behavior change for anyone.
    if args.filter == "config" && args.files.is_empty() {
        return config::print_resolved();
    }

    let cfg = config::Config::load()?;

    let filter = if let Some(name) = args.filter.strip_prefix('@') {
        cfg.filters.get(name).cloned().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("no saved filter named '{name}' (see `loglume config`)"),
            )
        })?
    } else {
        args.filter.clone()
    };

    let scope = args.scope.clone().or_else(|| cfg.tui.default_scope.clone());
    let sql = build_sql(&filter, scope.as_deref(), args.max_lines)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;

    if let Some(name) = &args.save_filter {
        let mut cfg = cfg.clone();
        cfg.filters.insert(name.clone(), sql.clone());
        cfg.save()?;
    }

    let highlight = args.highlight.as_deref().map(resolve_highlight_expr);

    if args.alert {
        let exec = args.exec.as_deref().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "--alert requires --exec")
        })?;
        let window = parse_duration_str(&args.window)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        let mode = resolve_emit_mode(args.alert_op.as_deref(), args.alert_threshold)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        let path = match args.files.as_slice() {
            [path] => path,
            [] => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "--alert requires a file argument (stdin can't be a standing query)",
                ))
            }
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "--alert supports exactly one file",
                ))
            }
        };
        return run_alert(path, &sql, window, mode, exec);
    }

    if args.tui {
        if args.files.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "--tui requires at least one file argument (stdin isn't supported)",
            ));
        }
        return tui::run(&args.files, sql, &cfg.tui.theme, &cfg.tui.color_rules);
    }

    match args.files.as_slice() {
        [] => {
            if args.follow {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "--follow requires a file argument (stdin can't be followed)",
                ));
            }
            run_stdin(&sql, args.tail, highlight.as_deref())
        }
        [path] => {
            if args.follow {
                run_follow(path, &sql, args.tail, highlight.as_deref())
            } else {
                run_once(path, &sql, args.tail, highlight.as_deref())
            }
        }
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "multiple files require --tui (side-by-side panes)",
        )),
    }
}

/// Resolve a `--highlight` expression: try loglume's short-form parser
/// first (reusing the exact same fragment loglume's own filters use), and
/// fall back to passing the expression straight to `compile_predicate` as
/// a raw boolean expression (e.g. referencing `message`/`raw`/tier-3
/// fields the short forms don't cover) -- db-core validates it either way.
fn resolve_highlight_expr(expr: &str) -> String {
    parse_where_clause(expr).unwrap_or_else(|_| normalize_double_quoted_literals(expr))
}

/// Rewrites `"..."` spans in `expr` to `'...'` (#39). db-core's SQL grammar
/// parses double quotes as a quoted *identifier* reference, not a string
/// literal, which is a common footgun since most users expect `"..."` to
/// mean a string literal (as it does in most other languages, JSON
/// included) -- `tag = "kernel"` is silently read as "column `tag` equals
/// column `kernel`", failing with a confusing "unknown column" error.
/// Existing single-quoted spans are copied through untouched.
fn normalize_double_quoted_literals(expr: &str) -> String {
    let mut out = String::with_capacity(expr.len());
    let mut chars = expr.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                out.push('\'');
                for inner in chars.by_ref() {
                    out.push(inner);
                    if inner == '\'' {
                        break;
                    }
                }
            }
            '"' => {
                out.push('\'');
                for inner in chars.by_ref() {
                    if inner == '"' {
                        break;
                    }
                    if inner == '\'' {
                        out.push('\''); // escape embedded ' for SQL ('' inside '...')
                    }
                    out.push(inner);
                }
                out.push('\'');
            }
            _ => out.push(c),
        }
    }
    out
}

pub(crate) fn engine_err(e: EngineError) -> io::Error {
    io::Error::other(e.to_string())
}

pub(crate) fn open_engine(path: &Path) -> io::Result<StreamEngine> {
    StreamEngine::open(path).map_err(engine_err)
}

fn compile_highlight(
    engine: &impl Engine,
    highlight: Option<&str>,
) -> io::Result<Option<CompiledPredicate>> {
    highlight
        .map(|expr| engine.compile_predicate(expr))
        .transpose()
        .map_err(engine_err)
}

fn run_once(path: &Path, sql: &str, tail: usize, highlight: Option<&str>) -> io::Result<()> {
    let mut engine = open_engine(path)?;
    let predicate = compile_highlight(&engine, highlight)?;
    let result = engine.run_query(sql).map_err(engine_err)?;
    print_result(&result, tail, predicate.as_ref());
    Ok(())
}

fn run_stdin(sql: &str, tail: usize, highlight: Option<&str>) -> io::Result<()> {
    use std::io::Read;

    let mut buffer = Vec::new();
    io::stdin().lock().read_to_end(&mut buffer)?;

    let tmp_path = std::env::temp_dir().join(format!("loglume-stdin-{}.log", std::process::id()));
    std::fs::write(&tmp_path, &buffer)?;
    let result = run_once(&tmp_path, sql, tail, highlight);
    let _ = std::fs::remove_file(&tmp_path);
    result
}

/// How often `watch_file` stats the log file.
pub(crate) const WATCH_POLL: std::time::Duration = std::time::Duration::from_millis(100);

/// Signal every change to `path`, one `()` per observed change.
///
/// This polls the file's size and mtime rather than using filesystem
/// notifications: on macOS, FSEvents reports *nothing* while a producer
/// appends through a long-held file descriptor (which is how syslogd,
/// docker and most application loggers write), so the previous
/// notify-based watcher only woke up when some other process happened to
/// close the file -- the "erratic, barely updating" live view. See
/// `tests/spikes/notify_probe.rs` for the measurements: FSEvents 0
/// events, notify's mtime polling ~1/s (clock granularity), size polling
/// every append, for one `stat` per tick.
///
/// A shrinking size (truncation/rotation) is a change like any other;
/// `StreamEngine::refresh` handles the reload.
pub(crate) fn watch_file(path: &Path) -> std::sync::mpsc::Receiver<()> {
    let (tx, rx) = std::sync::mpsc::channel();
    let path = path.to_path_buf();
    std::thread::spawn(move || {
        let stat = |p: &Path| {
            std::fs::metadata(p)
                .ok()
                .map(|m| (m.len(), m.modified().ok()))
        };
        let mut last = stat(&path);
        loop {
            std::thread::sleep(WATCH_POLL);
            let now = stat(&path);
            if now != last {
                last = now;
                // A send error means the receiver is gone: stop polling.
                if tx.send(()).is_err() {
                    return;
                }
            }
        }
    });
    rx
}

fn run_follow(path: &Path, sql: &str, tail: usize, highlight: Option<&str>) -> io::Result<()> {
    let mut engine = open_engine(path)?;
    let predicate = compile_highlight(&engine, highlight)?;
    let result = engine.run_query(sql).map_err(engine_err)?;
    print_result(&result, tail, predicate.as_ref());
    let mut printed = result.rows.len();

    for () in watch_file(path) {
        // Only newly appended lines enter the ring here, so the query
        // below applies the filter to those last lines; rows already
        // shown are skipped via `printed`.
        if engine.refresh().map_err(engine_err)? == 0 {
            continue;
        }
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

        print_rows(&result, printed, predicate.as_ref());
        print_footer(&result);
        printed = result.rows.len();
    }

    Ok(())
}

/// Parse a duration like "1m", "30s", "1h", "2d" (or a bare number of
/// seconds) -- `std::time::Duration` has no string parser of its own, and
/// this is a small, dependency-free case rather than pulling in a crate
/// for four suffixes.
fn parse_duration_str(s: &str) -> Result<std::time::Duration, String> {
    let s = s.trim();
    let (num, unit): (&str, &str) = match s.find(|c: char| !c.is_ascii_digit()) {
        Some(idx) => s.split_at(idx),
        None => (s, "s"),
    };
    let amount: u64 = num
        .parse()
        .map_err(|_| format!("invalid duration '{s}' (expected e.g. '1m', '30s', '1h')"))?;
    let secs = match unit.trim().to_ascii_lowercase().as_str() {
        "s" | "sec" | "secs" | "second" | "seconds" => amount,
        "m" | "min" | "mins" | "minute" | "minutes" => amount.saturating_mul(60),
        "h" | "hr" | "hrs" | "hour" | "hours" => amount.saturating_mul(3600),
        "d" | "day" | "days" => amount.saturating_mul(86400),
        other => return Err(format!("unrecognized duration unit '{other}' in '{s}'")),
    };
    Ok(std::time::Duration::from_secs(secs))
}

/// Parse a comparison operator string into `db_core`'s `BinaryOp`, for
/// `--alert-op`.
fn parse_binary_op(s: &str) -> Result<BinaryOp, String> {
    match s.trim() {
        ">" => Ok(BinaryOp::Gt),
        ">=" => Ok(BinaryOp::Ge),
        "<" => Ok(BinaryOp::Lt),
        "<=" => Ok(BinaryOp::Le),
        "=" | "==" => Ok(BinaryOp::Eq),
        "!=" | "<>" => Ok(BinaryOp::Ne),
        other => Err(format!(
            "unrecognized --alert-op '{other}' (expected one of > >= < <= = !=)"
        )),
    }
}

/// Resolve `--alert`'s `EmitMode` from the optional `--alert-op`/
/// `--alert-threshold` pair: both present selects `Threshold`, both
/// absent selects `OnChange`, one without the other is an error.
fn resolve_emit_mode(op: Option<&str>, threshold: Option<f64>) -> Result<EmitMode, String> {
    match (op, threshold) {
        (Some(op), Some(threshold)) => Ok(EmitMode::Threshold {
            op: parse_binary_op(op)?,
            threshold,
        }),
        (None, None) => Ok(EmitMode::OnChange),
        _ => Err("--alert-op and --alert-threshold must be given together".to_string()),
    }
}

/// Run `sql` as a standing query against `path`, executing `exec` on each
/// fire. Reuses the same `watch_file` polling pattern as `--follow`/the
/// TUI: on each observed file change, `engine.refresh()` then
/// `standing_query.poll()` -- db-core owns no scheduler, the caller (this
/// loop) decides when to poll.
fn run_alert(
    path: &Path,
    sql: &str,
    window: std::time::Duration,
    mode: EmitMode,
    exec: &str,
) -> io::Result<()> {
    let mut engine = open_engine(path)?;
    let mut standing_query =
        StandingQuery::new(&engine, sql, mode, window, window).map_err(engine_err)?;

    if let Some(event) = standing_query.poll(&mut engine).map_err(engine_err)? {
        exec_alert(exec, &event.result)?;
    }

    for () in watch_file(path) {
        if engine.refresh().map_err(engine_err)? == 0 {
            continue;
        }
        if let Some(event) = standing_query.poll(&mut engine).map_err(engine_err)? {
            exec_alert(exec, &event.result)?;
        }
    }

    Ok(())
}

/// Run `exec` as a shell command, piping `result`'s rows to its stdin
/// (one formatted line each, reusing `format_row`/`print_rows`'s shape).
fn exec_alert(exec: &str, result: &QueryResult) -> io::Result<()> {
    use std::io::Write as _;
    use std::process::{Command, Stdio};

    let raw_idx = result.columns.iter().position(|c| c == "raw");
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(exec)
        .stdin(Stdio::piped())
        .spawn()?;

    if let Some(mut stdin) = child.stdin.take() {
        for row in &result.rows {
            let _ = writeln!(stdin, "{}", format_row(row, raw_idx));
        }
    }

    child.wait()?;
    Ok(())
}

fn print_result(result: &QueryResult, tail: usize, highlight: Option<&CompiledPredicate>) {
    let start = if tail > 0 {
        result.rows.len().saturating_sub(tail)
    } else {
        0
    };
    print_rows(result, start, highlight);
    print_footer(result);
}

/// ANSI bold + yellow background, and reset. Always emitted when
/// `--highlight` matches a row -- restrict (`--filter`) narrows what's
/// shown, this only marks it, same as `grep --color` layered on top.
const HIGHLIGHT_ON: &str = "\x1b[1;43m";
const HIGHLIGHT_OFF: &str = "\x1b[0m";

/// Print rows starting at `start`, using the "raw" column if present.
/// Rows matching `highlight` (if any) are marked, not hidden.
fn print_rows(result: &QueryResult, start: usize, highlight: Option<&CompiledPredicate>) {
    let raw_idx = result.columns.iter().position(|c| c == "raw");
    let stdout = io::stdout();
    let mut out = stdout.lock();

    for row in result.rows.iter().skip(start) {
        let line = format_row(row, raw_idx);
        let is_match = highlight.is_some_and(|p| p.eval(row, &result.columns).unwrap_or(false));
        if is_match {
            let _ = writeln!(out, "{HIGHLIGHT_ON}{line}{HIGHLIGHT_OFF}");
        } else {
            let _ = writeln!(out, "{line}");
        }
    }
}

/// Render one result row as a single display line: the "raw" column's text
/// if present (typical `SELECT *` shape), else every cell tab-joined (e.g.
/// an aggregate query). Shared by the plain CLI output and the TUI list.
pub(crate) fn format_row(row: &[Cell], raw_idx: Option<usize>) -> String {
    let line = raw_idx
        .and_then(|idx| row.get(idx))
        .and_then(|cell| match cell {
            Cell::Text(s) => Some(s.clone()),
            _ => None,
        });
    match line {
        Some(line) => line,
        None => row.iter().map(format_cell).collect::<Vec<_>>().join("\t"),
    }
}

pub(crate) fn format_cell(cell: &Cell) -> String {
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

pub(crate) fn format_scope_report(report: &ScopeReport) -> String {
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

pub(crate) fn rewrite_filter_to_sql(filter: &str) -> Result<String, String> {
    let trimmed = filter.trim();
    if trimmed.len() >= 6 && trimmed[..6].eq_ignore_ascii_case("select") {
        return Ok(normalize_double_quoted_literals(trimmed));
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
                let level = Severity::parse(level_str)
                    .ok_or_else(|| format!("unrecognized severity level '{level_str}'"))?;
                // Emit the numeric discriminant, not the quoted name: the
                // name->number rewrite (`rewrite_severity_literals`) only
                // runs inside StreamEngine::run_query's own pipeline, not
                // in the generic compile_predicate path used by
                // --highlight -- the numeric form works identically in
                // both, so use it everywhere rather than maintain two
                // fragment builders.
                return Ok(format!("severity {op} {}", level as u8));
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

/// XDG-based configuration (`config.toml`): TUI defaults and saved filters.
mod config {
    use serde::{Deserialize, Serialize};
    use std::collections::BTreeMap;
    use std::io;
    use std::path::PathBuf;

    #[derive(Debug, Default, Clone, Serialize, Deserialize)]
    pub(crate) struct Config {
        #[serde(default)]
        pub(crate) tui: TuiConfig,
        /// Saved filter/query name -> resolved SQL, referenced as `@name`.
        #[serde(default)]
        pub(crate) filters: BTreeMap<String, String>,
    }

    #[derive(Debug, Default, Clone, Serialize, Deserialize)]
    pub(crate) struct TuiConfig {
        /// Fallback for `--scope` when not given on the command line.
        #[serde(default)]
        pub(crate) default_scope: Option<String>,
        /// Reserved for future key-remapping; not yet applied by the TUI.
        #[serde(default)]
        pub(crate) keybindings: BTreeMap<String, String>,
        /// Per-key color overrides (#35); unset keys fall back to the
        /// built-in Catppuccin Mocha defaults.
        #[serde(default)]
        pub(crate) theme: ThemeConfig,
        /// Ordered `(expression, color)` rules (#55): the first whose
        /// expression matches a row colors its text, falling through to
        /// severity-color/default if none match. Uses the same boolean-
        /// expression engine as `--filter`/`--highlight`.
        #[serde(default)]
        pub(crate) color_rules: Vec<ColorRuleConfig>,
    }

    /// One `[[tui.color_rules]]` entry: a boolean expression (loglume
    /// short-form or raw db-core SQL, same grammar `--highlight` accepts)
    /// paired with the hex color to apply when it matches.
    #[derive(Debug, Default, Clone, Serialize, Deserialize)]
    pub(crate) struct ColorRuleConfig {
        pub(crate) expr: String,
        pub(crate) color: String,
    }

    /// `[tui.theme]`: hex-string (`"#rrggbb"`) overrides for the TUI's
    /// color roles. Every field is optional -- an absent or invalid value
    /// falls back to the Catppuccin Mocha default for that role.
    #[derive(Debug, Default, Clone, Serialize, Deserialize)]
    pub(crate) struct ThemeConfig {
        #[serde(default)]
        pub(crate) border_focused: Option<String>,
        #[serde(default)]
        pub(crate) highlight_bg: Option<String>,
        #[serde(default)]
        pub(crate) highlight_fg: Option<String>,
        #[serde(default)]
        pub(crate) detail_dim: Option<String>,
        #[serde(default)]
        pub(crate) status_error: Option<String>,
        #[serde(default)]
        pub(crate) zebra_bg: Option<String>,
        #[serde(default)]
        pub(crate) severity_error: Option<String>,
        #[serde(default)]
        pub(crate) severity_warn: Option<String>,
        #[serde(default)]
        pub(crate) severity_dim: Option<String>,
    }

    impl Config {
        /// Load the config from its resolved path, or defaults if absent.
        pub(crate) fn load() -> io::Result<Self> {
            let path = config_path();
            match std::fs::read_to_string(&path) {
                Ok(contents) => toml::from_str(&contents)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string())),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
                Err(e) => Err(e),
            }
        }

        pub(crate) fn save(&self) -> io::Result<()> {
            let path = config_path();
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let text = toml::to_string_pretty(self)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
            std::fs::write(path, text)
        }
    }

    /// `$XDG_CONFIG_HOME/loglume/config.toml`, falling back to
    /// `$HOME/.config/loglume/config.toml` per the XDG Base Directory Spec
    /// (used verbatim, regardless of platform).
    pub(crate) fn config_path() -> PathBuf {
        config_dir().join("config.toml")
    }

    fn config_dir() -> PathBuf {
        resolve_config_dir(
            std::env::var("XDG_CONFIG_HOME").ok(),
            std::env::var("HOME").ok(),
        )
    }

    /// Pure XDG Base Directory resolution, taking the two relevant env
    /// vars as plain arguments instead of reading the process environment
    /// directly -- keeps this testable without mutating global env state
    /// (`std::env::set_var` in tests is a known thread-safety hazard when
    /// tests run in parallel).
    fn resolve_config_dir(xdg_config_home: Option<String>, home: Option<String>) -> PathBuf {
        if let Some(dir) = xdg_config_home {
            if !dir.is_empty() {
                return PathBuf::from(dir).join("loglume");
            }
        }
        let home = home.unwrap_or_else(|| ".".to_string());
        PathBuf::from(home).join(".config").join("loglume")
    }

    /// `loglume config`: print the resolved config path and its contents.
    pub(crate) fn print_resolved() -> io::Result<()> {
        let path = config_path();
        let cfg = Config::load()?;
        let text = toml::to_string_pretty(&cfg)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        println!("# {}", path.display());
        print!("{text}");
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn xdg_config_home_wins_when_set() {
            let dir = resolve_config_dir(
                Some("/custom/config".to_string()),
                Some("/home/me".to_string()),
            );
            assert_eq!(dir, PathBuf::from("/custom/config/loglume"));
        }

        #[test]
        fn falls_back_to_home_dot_config_when_xdg_unset() {
            let dir = resolve_config_dir(None, Some("/home/me".to_string()));
            assert_eq!(dir, PathBuf::from("/home/me/.config/loglume"));
        }

        #[test]
        fn falls_back_to_home_dot_config_when_xdg_empty() {
            let dir = resolve_config_dir(Some(String::new()), Some("/home/me".to_string()));
            assert_eq!(dir, PathBuf::from("/home/me/.config/loglume"));
        }

        #[test]
        fn default_config_has_no_filters() {
            let cfg = Config::default();
            assert!(cfg.filters.is_empty());
            assert!(cfg.tui.default_scope.is_none());
        }

        #[test]
        fn config_round_trips_through_toml() {
            let mut cfg = Config::default();
            cfg.tui.default_scope = Some("1h".to_string());
            cfg.filters
                .insert("myerr".to_string(), "SELECT * FROM log".to_string());

            let text = toml::to_string_pretty(&cfg).expect("serialize");
            let parsed: Config = toml::from_str(&text).expect("deserialize");

            assert_eq!(parsed.tui.default_scope, Some("1h".to_string()));
            assert_eq!(
                parsed.filters.get("myerr").map(String::as_str),
                Some("SELECT * FROM log")
            );
        }

        #[test]
        fn missing_config_file_loads_as_default() {
            // toml::from_str("") parses to an empty document, which with
            // #[serde(default)] on every field is equivalent to Config::default().
            let parsed: Config = toml::from_str("").expect("empty toml is valid");
            assert!(parsed.filters.is_empty());
            assert!(parsed.tui.default_scope.is_none());
        }
    }
}

/// XDG-based cache: per-field filter/highlight expression history (#43).
/// Separate from `config` above -- history is regenerable/non-essential
/// state, not configuration, so it lives under the cache base dir per the
/// XDG Base Directory Spec rather than alongside `config.toml`.
mod cache {
    use std::collections::HashSet;
    use std::io;
    use std::path::{Path, PathBuf};

    const MAX_HISTORY_ENTRIES: usize = 200;

    /// Most-recent-first, deduplicated history of past filter/highlight
    /// expressions for one field, persisted as one entry per line (oldest
    /// first) at a resolved path.
    #[derive(Debug, Default, Clone)]
    pub(crate) struct History {
        entries: Vec<String>,
    }

    impl History {
        /// Loads history from `path`, or an empty history if absent.
        pub(crate) fn load(path: &Path) -> Self {
            let raw = std::fs::read_to_string(path).unwrap_or_default();
            let mut seen = HashSet::new();
            let entries = raw
                .lines()
                .rev()
                .map(str::to_string)
                .filter(|line| seen.insert(line.clone()))
                .collect();
            Self { entries }
        }

        pub(crate) fn get(&self, pos: usize) -> Option<&str> {
            self.entries.get(pos).map(String::as_str)
        }

        pub(crate) fn len(&self) -> usize {
            self.entries.len()
        }

        pub(crate) fn is_empty(&self) -> bool {
            self.entries.is_empty()
        }

        /// Appends `entry` as the most recent, if non-empty and different
        /// from the current most-recent entry, then persists to `path`.
        pub(crate) fn append(&mut self, path: &Path, entry: &str) -> io::Result<()> {
            if entry.is_empty() || self.entries.first().map(String::as_str) == Some(entry) {
                return Ok(());
            }
            self.entries.retain(|e| e != entry);
            self.entries.insert(0, entry.to_string());
            self.entries.truncate(MAX_HISTORY_ENTRIES);

            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let text: String = self
                .entries
                .iter()
                .rev()
                .map(|e| format!("{e}\n"))
                .collect();
            std::fs::write(path, text)
        }
    }

    /// `$XDG_CACHE_HOME/loglume/filter_history`, falling back to
    /// `$HOME/.cache/loglume/filter_history` per the XDG Base Directory
    /// Spec (used verbatim, regardless of platform).
    pub(crate) fn filter_history_path() -> PathBuf {
        cache_dir().join("filter_history")
    }

    /// The latest TUI view settings (e.g. stacked/side-by-side layout),
    /// persisted as a single overwritten snapshot rather than a history
    /// (#56) -- unlike `History` above, there's only ever one "latest".
    #[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
    pub(crate) struct TuiState {
        #[serde(default = "default_stacked")]
        pub(crate) stacked: bool,
    }

    fn default_stacked() -> bool {
        true
    }

    impl Default for TuiState {
        fn default() -> Self {
            Self { stacked: true }
        }
    }

    impl TuiState {
        /// Loads state from `path`, or the default if absent or unparsable
        /// -- a corrupt or stale cache file must never block startup.
        pub(crate) fn load(path: &Path) -> Self {
            std::fs::read_to_string(path)
                .ok()
                .and_then(|raw| toml::from_str(&raw).ok())
                .unwrap_or_default()
        }

        /// Overwrites `path` with this state (not appended -- only the
        /// latest snapshot is kept).
        pub(crate) fn save(&self, path: &Path) -> io::Result<()> {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let text = toml::to_string_pretty(self)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
            std::fs::write(path, text)
        }
    }

    /// `$XDG_CACHE_HOME/loglume/tui_state`, falling back to
    /// `$HOME/.cache/loglume/tui_state` -- same resolution as
    /// [`filter_history_path`].
    pub(crate) fn tui_state_path() -> PathBuf {
        cache_dir().join("tui_state")
    }

    /// Same resolution as [`filter_history_path`], for `?`-highlight
    /// expressions instead of `/`-filter expressions.
    pub(crate) fn highlight_history_path() -> PathBuf {
        cache_dir().join("highlight_history")
    }

    fn cache_dir() -> PathBuf {
        resolve_cache_dir(
            std::env::var("XDG_CACHE_HOME").ok(),
            std::env::var("HOME").ok(),
        )
    }

    /// Pure XDG Base Directory resolution, mirroring
    /// `config::resolve_config_dir` but for the *cache* base dir.
    fn resolve_cache_dir(xdg_cache_home: Option<String>, home: Option<String>) -> PathBuf {
        if let Some(dir) = xdg_cache_home {
            if !dir.is_empty() {
                return PathBuf::from(dir).join("loglume");
            }
        }
        let home = home.unwrap_or_else(|| ".".to_string());
        PathBuf::from(home).join(".cache").join("loglume")
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn xdg_cache_home_wins_when_set() {
            let dir = resolve_cache_dir(
                Some("/custom/cache".to_string()),
                Some("/home/me".to_string()),
            );
            assert_eq!(dir, PathBuf::from("/custom/cache/loglume"));
        }

        #[test]
        fn falls_back_to_home_dot_cache_when_xdg_unset() {
            let dir = resolve_cache_dir(None, Some("/home/me".to_string()));
            assert_eq!(dir, PathBuf::from("/home/me/.cache/loglume"));
        }

        #[test]
        fn falls_back_to_home_dot_cache_when_xdg_empty() {
            let dir = resolve_cache_dir(Some(String::new()), Some("/home/me".to_string()));
            assert_eq!(dir, PathBuf::from("/home/me/.cache/loglume"));
        }

        fn temp_history_path(tag: &str) -> PathBuf {
            std::env::temp_dir().join(format!(
                "loglume-history-test-{tag}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or_default()
            ))
        }

        #[test]
        fn history_load_of_missing_file_is_empty() {
            let path = temp_history_path("missing");
            let history = History::load(&path);
            assert!(history.is_empty());
        }

        #[test]
        fn history_append_orders_most_recent_first_and_dedupes() {
            let path = temp_history_path("append");
            let mut history = History::load(&path);
            history.append(&path, "a").unwrap();
            history.append(&path, "b").unwrap();
            history.append(&path, "a").unwrap(); // re-promote "a"

            assert_eq!(history.len(), 2);
            assert_eq!(history.get(0), Some("a"));
            assert_eq!(history.get(1), Some("b"));

            let reloaded = History::load(&path);
            assert_eq!(reloaded.get(0), Some("a"));
            assert_eq!(reloaded.get(1), Some("b"));
            std::fs::remove_file(&path).ok();
        }

        #[test]
        fn history_append_ignores_empty_and_immediate_repeat() {
            let path = temp_history_path("repeat");
            let mut history = History::load(&path);
            history.append(&path, "").unwrap();
            assert!(history.is_empty());
            history.append(&path, "x").unwrap();
            history.append(&path, "x").unwrap();
            assert_eq!(history.len(), 1);
            std::fs::remove_file(&path).ok();
        }

        #[test]
        fn history_append_caps_entry_count() {
            let path = temp_history_path("cap");
            let mut history = History::load(&path);
            for i in 0..(MAX_HISTORY_ENTRIES + 10) {
                history.append(&path, &i.to_string()).unwrap();
            }
            assert_eq!(history.len(), MAX_HISTORY_ENTRIES);
            assert_eq!(
                history.get(0),
                Some((MAX_HISTORY_ENTRIES + 9).to_string().as_str())
            );
            std::fs::remove_file(&path).ok();
        }

        fn temp_tui_state_path(tag: &str) -> PathBuf {
            std::env::temp_dir().join(format!(
                "loglume-tui-state-test-{tag}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or_default()
            ))
        }

        #[test]
        fn tui_state_load_of_missing_file_defaults_to_stacked() {
            let path = temp_tui_state_path("missing");
            assert_eq!(TuiState::load(&path), TuiState { stacked: true });
        }

        #[test]
        fn tui_state_load_of_unparsable_file_falls_back_to_default() {
            let path = temp_tui_state_path("garbage");
            std::fs::write(&path, "not valid toml {{{").unwrap();
            assert_eq!(TuiState::load(&path), TuiState::default());
            std::fs::remove_file(&path).ok();
        }

        #[test]
        fn tui_state_save_and_load_round_trips_and_overwrites() {
            let path = temp_tui_state_path("roundtrip");
            TuiState { stacked: false }.save(&path).unwrap();
            assert_eq!(TuiState::load(&path), TuiState { stacked: false });

            // A second save overwrites the single latest snapshot rather
            // than appending (#56) -- unlike History, there's no log here.
            TuiState { stacked: true }.save(&path).unwrap();
            assert_eq!(TuiState::load(&path), TuiState { stacked: true });
            std::fs::remove_file(&path).ok();
        }
    }
}

/// Interactive TUI viewer (`--tui`).
///
/// Reuses the same `StreamEngine`/SQL query path as the plain CLI: the
/// filter bar compiles through [`rewrite_filter_to_sql`], the same
/// function `--filter` uses, so there is exactly one place that
/// understands loglume's filter syntax.
mod tui {
    use super::cache::{self, History};
    use super::config::{ColorRuleConfig, ThemeConfig};
    use super::{
        engine_err, format_cell, format_row, format_scope_report, open_engine,
        resolve_highlight_expr, rewrite_filter_to_sql,
    };
    use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
    use crossterm::execute;
    use crossterm::terminal::{
        disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
    };
    use loglume::{Cell, CompiledPredicate, Engine, QueryResult, Severity};
    use ratatui::backend::CrosstermBackend;
    use ratatui::layout::{Constraint, Direction, Layout};
    use ratatui::style::{Color, Modifier, Style};
    use ratatui::text::{Line, Span};
    use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
    use ratatui::Terminal;
    use std::io::{self, Stdout};
    use std::path::{Path, PathBuf};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    type Tui = Terminal<CrosstermBackend<Stdout>>;

    const TICK_RATE: Duration = Duration::from_millis(100);

    /// The TUI's color roles, defaulting to Catppuccin Mocha (#35) to match
    /// this project's terminal setup (tmux/ghostty), individually
    /// overridable via `config.toml`'s `[tui.theme]`.
    #[derive(Debug, Clone, Copy)]
    struct Theme {
        border_focused: Color,
        highlight_bg: Color,
        highlight_fg: Color,
        detail_dim: Color,
        status_error: Color,
        zebra_bg: Color,
        severity_error: Color,
        severity_warn: Color,
        severity_dim: Color,
    }

    impl Default for Theme {
        fn default() -> Self {
            Self {
                border_focused: Color::Rgb(0x89, 0xb4, 0xfa), // blue
                highlight_bg: Color::Rgb(0xf9, 0xe2, 0xaf),   // yellow
                highlight_fg: Color::Rgb(0x1e, 0x1e, 0x2e),   // base
                detail_dim: Color::Rgb(0x6c, 0x70, 0x86),     // overlay1
                status_error: Color::Rgb(0xf3, 0x8b, 0xa8),   // red
                zebra_bg: Color::Rgb(0x31, 0x32, 0x44),       // surface0
                severity_error: Color::Rgb(0xf3, 0x8b, 0xa8), // red (Error/Fatal)
                severity_warn: Color::Rgb(0xfa, 0xb3, 0x87),  // peach (Warn)
                severity_dim: Color::Rgb(0x6c, 0x70, 0x86),   // overlay1 (Info/Debug/Trace)
            }
        }
    }

    impl Theme {
        /// Apply `[tui.theme]` overrides on top of the Catppuccin Mocha
        /// defaults. An unset or unparsable hex value keeps the default
        /// for that role rather than erroring -- a typo in one key
        /// shouldn't break every other color.
        fn resolve(cfg: &ThemeConfig) -> Self {
            let defaults = Self::default();
            Self {
                border_focused: parse_hex_color(cfg.border_focused.as_deref())
                    .unwrap_or(defaults.border_focused),
                highlight_bg: parse_hex_color(cfg.highlight_bg.as_deref())
                    .unwrap_or(defaults.highlight_bg),
                highlight_fg: parse_hex_color(cfg.highlight_fg.as_deref())
                    .unwrap_or(defaults.highlight_fg),
                detail_dim: parse_hex_color(cfg.detail_dim.as_deref())
                    .unwrap_or(defaults.detail_dim),
                status_error: parse_hex_color(cfg.status_error.as_deref())
                    .unwrap_or(defaults.status_error),
                zebra_bg: parse_hex_color(cfg.zebra_bg.as_deref()).unwrap_or(defaults.zebra_bg),
                severity_error: parse_hex_color(cfg.severity_error.as_deref())
                    .unwrap_or(defaults.severity_error),
                severity_warn: parse_hex_color(cfg.severity_warn.as_deref())
                    .unwrap_or(defaults.severity_warn),
                severity_dim: parse_hex_color(cfg.severity_dim.as_deref())
                    .unwrap_or(defaults.severity_dim),
            }
        }
    }

    /// Foreground color for a row based on its `severity` column: red for
    /// Error/Fatal, peach for Warn, dim for Info/Debug/Trace. `None` only
    /// when the row has no `severity` column at all (#54).
    fn severity_color(theme: &Theme, severity: Option<i64>) -> Option<Color> {
        let severity = severity?;
        if severity >= Severity::Error as u8 as i64 {
            Some(theme.severity_error)
        } else if severity >= Severity::Warn as u8 as i64 {
            Some(theme.severity_warn)
        } else {
            Some(theme.severity_dim)
        }
    }

    /// One compiled `[[tui.color_rules]]` entry (#55): a predicate paired
    /// with the color to apply when it matches. Compiled once per `Pane`
    /// at construction, not per row/frame -- the same cost model as the
    /// existing `highlight` predicate.
    struct ColorRule {
        predicate: CompiledPredicate,
        color: Color,
    }

    /// Compiles each configured rule against `engine`, silently dropping
    /// any entry with an unparsable expression or invalid color -- a typo
    /// in one rule must not prevent the TUI from starting, matching
    /// `Theme::resolve`'s tolerance for a bad hex value (#55).
    fn compile_color_rules(engine: &impl Engine, rules: &[ColorRuleConfig]) -> Vec<ColorRule> {
        rules
            .iter()
            .filter_map(|rule| {
                let predicate = engine
                    .compile_predicate(&resolve_highlight_expr(&rule.expr))
                    .ok()?;
                let color = parse_hex_color(Some(&rule.color))?;
                Some(ColorRule { predicate, color })
            })
            .collect()
    }

    /// Foreground color from the first matching rule in `rules`, in order
    /// -- first match wins (#55). Eval errors are treated as non-match,
    /// same as `Pane::row_is_highlighted`.
    fn color_rule_fg(rules: &[ColorRule], row: &[Cell], columns: &[String]) -> Option<Color> {
        rules
            .iter()
            .find(|rule| rule.predicate.eval(row, columns).unwrap_or(false))
            .map(|rule| rule.color)
    }

    /// Parse a `"#rrggbb"` (or `"rrggbb"`) hex string into a `Color::Rgb`.
    /// Returns `None` for anything absent or malformed.
    fn parse_hex_color(hex: Option<&str>) -> Option<Color> {
        let hex = hex?.trim().strip_prefix('#').unwrap_or(hex?.trim());
        if hex.len() != 6 {
            return None;
        }
        let r = u8::from_str_radix(hex.get(0..2)?, 16).ok()?;
        let g = u8::from_str_radix(hex.get(2..4)?, 16).ok()?;
        let b = u8::from_str_radix(hex.get(4..6)?, 16).ok()?;
        Some(Color::Rgb(r, g, b))
    }

    /// Run the interactive TUI against `paths`, each opened in its own pane,
    /// all starting with `initial_sql`.
    pub(crate) fn run(
        paths: &[PathBuf],
        initial_sql: String,
        theme_cfg: &ThemeConfig,
        color_rules_cfg: &[ColorRuleConfig],
    ) -> io::Result<()> {
        let theme = Theme::resolve(theme_cfg);
        // Open every file *before* touching the terminal: a missing path
        // used to bail out of `App::new` after EnterAlternateScreen/raw
        // mode was already on, leaving the shell wedged in the alternate
        // screen with echo off -- it looked like a hang, not an error.
        let app = App::new(paths, initial_sql, theme, color_rules_cfg)?;
        install_panic_hook();
        let mut terminal = init_terminal()?;
        let result = app.run(&mut terminal);
        // Restore unconditionally, and never let a restore error hide the
        // run error that caused it.
        let restored = restore_terminal(&mut terminal);
        result.and(restored)
    }

    fn init_terminal() -> io::Result<Tui> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        // No EnableMouseCapture: the TUI has no mouse handling at all, and
        // capturing mouse events disables the terminal's native click-drag
        // text selection/copy for no benefit (#38).
        execute!(stdout, EnterAlternateScreen)?;
        Terminal::new(CrosstermBackend::new(stdout))
    }

    fn restore_terminal(terminal: &mut Tui) -> io::Result<()> {
        disable_raw_mode()?;
        execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
        terminal.show_cursor()
    }

    /// A panic mid-render must not leave the user's terminal in raw/
    /// alternate-screen mode, so restore it first, then chain to the
    /// default hook.
    fn install_panic_hook() {
        let original = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _ = disable_raw_mode();
            let _ = execute!(io::stdout(), LeaveAlternateScreen);
            original(info);
        }));
    }

    /// One open file with its own engine, query, and scroll/edit state —
    /// independent of every other pane.
    struct Pane {
        path: PathBuf,
        engine: loglume::StreamEngine,
        sql: String,
        result: QueryResult,
        list_state: ListState,
        filter_text: String,
        /// Cursor position within `filter_text`, in chars (not bytes).
        filter_cursor: usize,
        editing_filter: bool,
        /// Applied filter expressions, most-recent-first, persisted under
        /// the XDG cache dir (#43). Recalled via `Up`/`Down` while editing.
        filter_history: History,
        /// Position within `filter_history` while navigating (`None` when
        /// not navigating, i.e. showing the in-progress `filter_draft`).
        filter_history_pos: Option<usize>,
        /// `filter_text` as it was before history navigation started, so
        /// paging back past the newest entry restores it.
        filter_draft: String,
        /// Restrict-vs-highlight (#4/#14): narrows via `sql`/`filter_text`
        /// above; this only marks matches, never hides anything.
        highlight_text: String,
        /// Cursor position within `highlight_text`, in chars (not bytes).
        highlight_cursor: usize,
        highlight: Option<CompiledPredicate>,
        highlight_enabled: bool,
        editing_highlight: bool,
        /// Same role as `filter_history`, for `?`-highlight expressions.
        highlight_history: History,
        highlight_history_pos: Option<usize>,
        highlight_draft: String,
        /// When true, the list displays newest-first (top-down) instead of
        /// the natural oldest-first order; toggled per pane via 'R'.
        reverse: bool,
        /// Query/filter error, shown in the filter bar (never the
        /// highlight bar -- keeping these separate is the whole point:
        /// mixing them up made a highlight-compile error render in the
        /// filter bar while the highlight bar looked like nothing
        /// happened).
        filter_status: Option<String>,
        /// Highlight-compile error, shown in the highlight bar.
        highlight_status: Option<String>,
        /// When true, a detail pane shows the full raw line and every
        /// column/value pair for the selected row, shrinking the list to
        /// make room (#30). Toggled per pane via 'd'.
        detail_open: bool,
        /// When true, the filter and highlight bars are collapsed so the
        /// list can use the full pane height (#58). Toggled via 'b'; forced
        /// back to `false` when entering filter/highlight edit, since you
        /// can't type into a box you can't see.
        bars_hidden: bool,
        theme: Theme,
        /// Ordered `[tui.color_rules]`, compiled once against this pane's
        /// own engine at construction (#55).
        color_rules: Vec<ColorRule>,
        watch_rx: mpsc::Receiver<()>,
    }

    /// Emacs/readline-style line editing shared by the filter and highlight
    /// text boxes (#42). `cursor` is a char index (not byte), so multi-byte
    /// UTF-8 input stays correct. Returns `true` if `code`/`mods` was
    /// recognized as a line-editing key; `false` if the caller should
    /// handle it itself (e.g. `Enter`/`Esc`, already matched by callers
    /// before falling through here).
    fn edit_line(text: &mut String, cursor: &mut usize, code: KeyCode, mods: KeyModifiers) -> bool {
        let ctrl = mods.contains(KeyModifiers::CONTROL);
        let alt = mods.contains(KeyModifiers::ALT);
        let mut chars: Vec<char> = text.chars().collect();
        let len = chars.len();
        *cursor = (*cursor).min(len);

        match code {
            KeyCode::Char('a') if ctrl => *cursor = 0,
            KeyCode::Char('e') if ctrl => *cursor = len,
            KeyCode::Char('b') if ctrl => *cursor = cursor.saturating_sub(1),
            KeyCode::Left => *cursor = cursor.saturating_sub(1),
            KeyCode::Char('f') if ctrl => *cursor = (*cursor + 1).min(len),
            KeyCode::Right => *cursor = (*cursor + 1).min(len),
            KeyCode::Char('k') if ctrl => {
                chars.truncate(*cursor);
            }
            KeyCode::Char('u') if ctrl => {
                chars.drain(0..*cursor);
                *cursor = 0;
            }
            KeyCode::Char('w') if ctrl => delete_word_backward(&mut chars, cursor),
            KeyCode::Backspace if alt => delete_word_backward(&mut chars, cursor),
            KeyCode::Char('b') if alt => *cursor = word_backward(&chars, *cursor),
            KeyCode::Char('f') if alt => *cursor = word_forward(&chars, *cursor),
            KeyCode::Char('d') if ctrl => {
                if *cursor < chars.len() {
                    chars.remove(*cursor);
                }
            }
            KeyCode::Backspace => {
                if *cursor > 0 {
                    chars.remove(*cursor - 1);
                    *cursor -= 1;
                }
            }
            KeyCode::Char(c) if !ctrl && !alt => {
                chars.insert(*cursor, c);
                *cursor += 1;
            }
            _ => return false,
        }

        *text = chars.into_iter().collect();
        true
    }

    /// Shell-style history recall while editing (#43): `direction` of `1`
    /// (`Up`) moves toward older entries, `-1` (`Down`) toward newer ones.
    /// `pos` is `None` while showing `*draft` (the in-progress text before
    /// navigation started); moving `Down` past the newest entry restores it.
    fn recall_history(
        history: &History,
        pos: &mut Option<usize>,
        draft: &mut String,
        text: &mut String,
        cursor: &mut usize,
        direction: isize,
    ) {
        let next_pos = match (*pos, direction) {
            (None, 1) if !history.is_empty() => {
                *draft = text.clone();
                Some(0)
            }
            (None, _) => return, // no-op: not navigating, nothing to recall
            (Some(p), 1) if p + 1 < history.len() => Some(p + 1),
            (Some(p), -1) if p > 0 => Some(p - 1),
            (Some(_), -1) => None,
            (Some(p), _) => Some(p),
        };

        *pos = next_pos;
        *text = match next_pos {
            Some(p) => history.get(p).unwrap_or_default().to_string(),
            None => draft.clone(),
        };
        *cursor = text.chars().count();
    }

    /// The cursor position after moving backward one word from `cursor`:
    /// skip trailing whitespace, then skip the word itself.
    #[allow(clippy::indexing_slicing)]
    fn word_backward(chars: &[char], mut cursor: usize) -> usize {
        while cursor > 0 && chars[cursor - 1].is_whitespace() {
            cursor -= 1;
        }
        while cursor > 0 && !chars[cursor - 1].is_whitespace() {
            cursor -= 1;
        }
        cursor
    }

    /// The cursor position after moving forward one word from `cursor`:
    /// skip leading whitespace, then skip the word itself.
    #[allow(clippy::indexing_slicing)]
    fn word_forward(chars: &[char], mut cursor: usize) -> usize {
        let len = chars.len();
        while cursor < len && chars[cursor].is_whitespace() {
            cursor += 1;
        }
        while cursor < len && !chars[cursor].is_whitespace() {
            cursor += 1;
        }
        cursor
    }

    /// Deletes the word immediately before `cursor` (`Ctrl-W`/`Alt-Backspace`).
    fn delete_word_backward(chars: &mut Vec<char>, cursor: &mut usize) {
        let start = word_backward(chars, *cursor);
        chars.drain(start..*cursor);
        *cursor = start;
    }

    /// Renders `text` as a `Line` with the character at `cursor` reverse-
    /// styled to simulate a text-cursor -- a trailing styled space stands
    /// in for the cursor when it sits past the last character.
    #[allow(clippy::indexing_slicing)]
    fn render_cursor_line(text: &str, cursor: usize) -> Line<'static> {
        let chars: Vec<char> = text.chars().collect();
        let cursor = cursor.min(chars.len());
        let before: String = chars[..cursor].iter().collect();
        let (at, after): (String, String) = if cursor < chars.len() {
            (
                chars[cursor].to_string(),
                chars[cursor + 1..].iter().collect(),
            )
        } else {
            (" ".to_string(), String::new())
        };
        Line::from(vec![
            Span::raw(before),
            Span::styled(at, Style::default().add_modifier(Modifier::REVERSED)),
            Span::raw(after),
        ])
    }

    impl Pane {
        fn new(
            path: &Path,
            sql: String,
            theme: Theme,
            color_rules_cfg: &[ColorRuleConfig],
        ) -> io::Result<Self> {
            let mut engine = open_engine(path)?;
            let result = engine.run_query(&sql).map_err(engine_err)?;
            let color_rules = compile_color_rules(&engine, color_rules_cfg);

            let rx = super::watch_file(path);

            let mut list_state = ListState::default();
            if !result.rows.is_empty() {
                list_state.select(Some(0));
            }

            let filter_text = sql.clone();
            Ok(Self {
                path: path.to_path_buf(),
                engine,
                sql,
                result,
                list_state,
                filter_text,
                filter_cursor: 0,
                editing_filter: false,
                filter_history: History::load(&cache::filter_history_path()),
                filter_history_pos: None,
                filter_draft: String::new(),
                highlight_text: String::new(),
                highlight_cursor: 0,
                highlight: None,
                highlight_enabled: false,
                editing_highlight: false,
                highlight_history: History::load(&cache::highlight_history_path()),
                highlight_history_pos: None,
                highlight_draft: String::new(),
                reverse: true,
                filter_status: None,
                highlight_status: None,
                detail_open: false,
                bars_hidden: false,
                theme,
                color_rules,
                watch_rx: rx,
            })
        }

        /// The display position of the newest row, given `len` rows:
        /// index 0 (top) in reverse mode, `len - 1` (bottom) otherwise.
        /// `0` for an empty result either way.
        fn latest_index(&self, len: usize) -> usize {
            if self.reverse || len == 0 {
                0
            } else {
                len - 1
            }
        }

        /// Non-blocking drain of this pane's file watcher; returns true if
        /// a refresh is warranted (coalesces a burst of events into one
        /// requery instead of one per filesystem event).
        fn drain_file_events(&mut self) -> io::Result<bool> {
            let mut changed = false;
            while let Ok(()) = self.watch_rx.try_recv() {
                changed = true;
            }
            if !changed {
                return Ok(false);
            }
            // Only the appended lines are ingested, so the caller's
            // requery applies the filter to just those last lines.
            Ok(self.engine.refresh().map_err(engine_err)? > 0)
        }

        /// Re-run this pane's query. `follow_tail` sticks the selection to
        /// the newest row (for live-append refreshes); otherwise the view
        /// resets to the top (for an explicit filter change).
        fn requery(&mut self, follow_tail: bool) -> io::Result<()> {
            match self.engine.run_query(&self.sql) {
                Ok(result) => {
                    let latest_before = self.latest_index(self.result.rows.len());
                    let was_at_latest = follow_tail
                        && self
                            .list_state
                            .selected()
                            .is_none_or(|i| i == latest_before);
                    self.result = result;
                    self.list_state = ListState::default();
                    if !self.result.rows.is_empty() {
                        let selected = if was_at_latest {
                            self.latest_index(self.result.rows.len())
                        } else {
                            0
                        };
                        self.list_state.select(Some(selected));
                    }
                    self.filter_status = None;
                }
                Err(e) => {
                    self.filter_status = Some(format!("query error: {e}"));
                }
            }
            Ok(())
        }

        /// Handle a key while this pane is focused. Pane-management keys
        /// (quit/close-pane/cycle-focus) are intercepted by `App` before
        /// reaching here, except while editing the filter text, where they
        /// must fall through to ordinary text input instead.
        fn handle_key(&mut self, code: KeyCode, mods: KeyModifiers) -> io::Result<()> {
            if self.editing_filter {
                match code {
                    KeyCode::Enter => {
                        self.editing_filter = false;
                        self.filter_history_pos = None;
                        self.filter_history
                            .append(&cache::filter_history_path(), self.filter_text.trim())?;
                        match rewrite_filter_to_sql(&self.filter_text) {
                            Ok(sql) => {
                                self.sql = sql;
                                self.requery(false)?;
                            }
                            Err(e) => self.filter_status = Some(format!("filter error: {e}")),
                        }
                    }
                    KeyCode::Esc => {
                        self.editing_filter = false;
                        self.filter_history_pos = None;
                        self.filter_text = self.sql.clone();
                    }
                    KeyCode::Up => {
                        recall_history(
                            &self.filter_history,
                            &mut self.filter_history_pos,
                            &mut self.filter_draft,
                            &mut self.filter_text,
                            &mut self.filter_cursor,
                            1,
                        );
                    }
                    KeyCode::Down => {
                        recall_history(
                            &self.filter_history,
                            &mut self.filter_history_pos,
                            &mut self.filter_draft,
                            &mut self.filter_text,
                            &mut self.filter_cursor,
                            -1,
                        );
                    }
                    _ => {
                        edit_line(&mut self.filter_text, &mut self.filter_cursor, code, mods);
                    }
                }
                return Ok(());
            }

            if self.editing_highlight {
                match code {
                    KeyCode::Enter => {
                        self.editing_highlight = false;
                        self.highlight_history_pos = None;
                        self.highlight_history
                            .append(&cache::highlight_history_path(), self.highlight_text.trim())?;
                        if self.highlight_text.trim().is_empty() {
                            self.highlight = None;
                            self.highlight_enabled = false;
                            self.highlight_status = None;
                        } else {
                            let expr = resolve_highlight_expr(&self.highlight_text);
                            match self.engine.compile_predicate(&expr).map_err(engine_err) {
                                Ok(predicate) => {
                                    self.highlight = Some(predicate);
                                    self.highlight_enabled = true;
                                    self.highlight_status = None;
                                }
                                Err(e) => {
                                    self.highlight = None;
                                    self.highlight_enabled = false;
                                    self.highlight_status = Some(format!("highlight error: {e}"));
                                }
                            }
                        }
                    }
                    KeyCode::Esc => {
                        self.editing_highlight = false;
                        self.highlight_history_pos = None;
                    }
                    KeyCode::Up => {
                        recall_history(
                            &self.highlight_history,
                            &mut self.highlight_history_pos,
                            &mut self.highlight_draft,
                            &mut self.highlight_text,
                            &mut self.highlight_cursor,
                            1,
                        );
                    }
                    KeyCode::Down => {
                        recall_history(
                            &self.highlight_history,
                            &mut self.highlight_history_pos,
                            &mut self.highlight_draft,
                            &mut self.highlight_text,
                            &mut self.highlight_cursor,
                            -1,
                        );
                    }
                    _ => {
                        edit_line(
                            &mut self.highlight_text,
                            &mut self.highlight_cursor,
                            code,
                            mods,
                        );
                    }
                }
                return Ok(());
            }

            match code {
                KeyCode::Char('j') | KeyCode::Down => self.select_relative(1),
                KeyCode::Char('k') | KeyCode::Up => self.select_relative(-1),
                KeyCode::PageDown => self.select_relative(10),
                KeyCode::PageUp => self.select_relative(-10),
                KeyCode::Char('/') | KeyCode::Char(':') => {
                    self.editing_filter = true;
                    self.bars_hidden = false;
                    self.filter_text = self.sql.clone();
                    self.filter_cursor = self.filter_text.chars().count();
                }
                KeyCode::Char('?') => {
                    self.editing_highlight = true;
                    self.bars_hidden = false;
                    self.highlight_cursor = self.highlight_text.chars().count();
                }
                KeyCode::Char('b') => self.bars_hidden = !self.bars_hidden,
                KeyCode::Char('h') => {
                    // Toggle rendering on/off without clearing the
                    // expression (explicit requirement of #14).
                    if self.highlight.is_some() {
                        self.highlight_enabled = !self.highlight_enabled;
                    }
                }
                KeyCode::Char('r') => self.requery(true)?,
                KeyCode::Char('R') => {
                    self.reverse = !self.reverse;
                    if !self.result.rows.is_empty() {
                        let idx = self.latest_index(self.result.rows.len());
                        self.list_state.select(Some(idx));
                    }
                }
                KeyCode::Char('d') => self.detail_open = !self.detail_open,
                KeyCode::Esc if self.detail_open => self.detail_open = false,
                _ => {}
            }
            Ok(())
        }

        fn select_relative(&mut self, delta: isize) {
            let len = self.result.rows.len();
            if len == 0 {
                return;
            }
            let current = self.list_state.selected().unwrap_or(0) as isize;
            let next = (current + delta).clamp(0, len as isize - 1);
            self.list_state.select(Some(next as usize));
        }

        /// The row currently under the selection cursor, accounting for
        /// `reverse` (display index 0 maps to the *last* actual row when
        /// reversed, not the first). Test-only: production code always
        /// has the row in hand already while iterating in `draw`.
        #[cfg(test)]
        fn selected_row(&self) -> Option<&Vec<Cell>> {
            let display_idx = self.list_state.selected()?;
            let len = self.result.rows.len();
            let actual_idx = if self.reverse {
                len.checked_sub(1)?.checked_sub(display_idx)?
            } else {
                display_idx
            };
            self.result.rows.get(actual_idx)
        }

        /// One "name: value" line per field of `row`, excluding `raw` --
        /// the raw text is already shown inline on the row's own summary
        /// line when expanded (#33), not repeated as a field.
        fn field_lines_for(&self, row: &[Cell]) -> Vec<String> {
            self.result
                .columns
                .iter()
                .zip(row.iter())
                .filter(|(name, _)| name.as_str() != "raw")
                .map(|(name, cell)| format!("{name}: {}", format_cell(cell)))
                .collect()
        }

        /// Renders just the log list into `area` -- shared by `draw`'s
        /// normal (list + filter/highlight bars) and `bars_hidden`
        /// (list only, full pane height) layouts.
        fn draw_list(
            &mut self,
            frame: &mut ratatui::Frame,
            list_area: ratatui::layout::Rect,
            focused: bool,
        ) {
            let border_style = if focused {
                Style::default().fg(self.theme.border_focused)
            } else {
                Style::default()
            };

            let raw_idx = self.result.columns.iter().position(|c| c == "raw");
            let severity_idx = self.result.columns.iter().position(|c| c == "severity");
            let highlight_style = Style::default()
                .bg(self.theme.highlight_bg)
                .fg(self.theme.highlight_fg)
                .add_modifier(Modifier::BOLD);
            // Detail rows (#33) are inline, not a separate bordered panel:
            // distinct dim coloring plus a "+-" prefix is what sets them
            // apart from ordinary list rows.
            let detail_style = Style::default().fg(self.theme.detail_dim);

            // Only the currently selected row can be expanded (detail
            // follows the selection cursor, same as before #33's rework),
            // so injecting its extra field rows right after it can never
            // shift any *earlier* row's display position -- the selected
            // row's own index within `items` still matches
            // `self.list_state.selected()` set by select_relative/etc.
            let selected_display_idx = self.list_state.selected();
            let rows_in_display_order: Box<dyn Iterator<Item = &Vec<Cell>>> = if self.reverse {
                Box::new(self.result.rows.iter().rev())
            } else {
                Box::new(self.result.rows.iter())
            };

            // Extra items pushed for the expanded row's fields, if any --
            // used below as `scroll_padding` so the widget's auto-scroll
            // (which otherwise only guarantees the *selected* summary line
            // itself is visible) also pulls the detail lines into view
            // instead of leaving them clipped off the bottom of the pane.
            let mut expanded_field_count = 0usize;

            let mut items: Vec<ListItem> = Vec::new();
            for (display_idx, row) in rows_in_display_order.enumerate() {
                let expand = self.detail_open && selected_display_idx == Some(display_idx);
                let text = format_row(row, raw_idx);
                let summary_text = if expand { format!("- {text}") } else { text };
                let summary_item = if self.row_is_highlighted(row) {
                    ListItem::new(summary_text).style(highlight_style)
                } else {
                    let severity =
                        severity_idx
                            .and_then(|idx| row.get(idx))
                            .and_then(|cell| match cell {
                                Cell::Int(i) => Some(*i),
                                _ => None,
                            });
                    let mut style = Style::default();
                    if display_idx % 2 == 1 {
                        style = style.bg(self.theme.zebra_bg);
                    }
                    let fg = color_rule_fg(&self.color_rules, row, &self.result.columns)
                        .or_else(|| severity_color(&self.theme, severity));
                    if let Some(fg) = fg {
                        style = style.fg(fg);
                    }
                    ListItem::new(summary_text).style(style)
                };
                items.push(summary_item);

                if expand {
                    let field_lines = self.field_lines_for(row);
                    expanded_field_count = field_lines.len();
                    for line in field_lines {
                        items.push(ListItem::new(format!("    +- {line}")).style(detail_style));
                    }
                }
            }

            let name = self
                .path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| self.path.to_string_lossy().into_owned());
            let order_tag = if self.reverse { "" } else { " [oldest-first]" };
            let bars_tag = if self.bars_hidden {
                " [bars hidden]"
            } else {
                ""
            };
            let title = self
                .result
                .scope_report
                .as_ref()
                .map(|r| format!("{name}{order_tag}{bars_tag} — {}", format_scope_report(r)))
                .unwrap_or_else(|| format!("{name}{order_tag}{bars_tag}"));

            let list = List::new(items)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(border_style)
                        .title(title),
                )
                .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
                .scroll_padding(expanded_field_count);
            frame.render_stateful_widget(list, list_area, &mut self.list_state);
        }

        fn draw(&mut self, frame: &mut ratatui::Frame, area: ratatui::layout::Rect, focused: bool) {
            if self.bars_hidden {
                self.draw_list(frame, area, focused);
                return;
            }

            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Min(1),
                    Constraint::Length(3),
                    Constraint::Length(3),
                ])
                .split(area);
            let (Some(&list_area), Some(&filter_area), Some(&highlight_area)) =
                (chunks.first(), chunks.get(1), chunks.get(2))
            else {
                return;
            };

            self.draw_list(frame, list_area, focused);

            let border_style = if focused {
                Style::default().fg(self.theme.border_focused)
            } else {
                Style::default()
            };

            let filter_title = if self.editing_filter {
                "filter (Enter to apply, Esc to cancel)"
            } else {
                "filter (/ edit, j/k move, d detail, R reverse, Tab pane, v view, b bars, x close, q quit)"
            };
            let filter_body_style = if self.filter_status.is_some() {
                Style::default().fg(self.theme.status_error)
            } else {
                Style::default()
            };
            let filter_line = if self.editing_filter {
                render_cursor_line(&self.filter_text, self.filter_cursor)
            } else {
                Line::from(
                    self.filter_status
                        .clone()
                        .unwrap_or_else(|| self.filter_text.clone()),
                )
            };
            let input = Paragraph::new(filter_line).style(filter_body_style).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(border_style)
                    .title(filter_title),
            );
            frame.render_widget(input, filter_area);

            let highlight_title = if self.editing_highlight {
                "highlight (Enter to apply, Esc to cancel)"
            } else {
                "highlight (? edit, h toggle on/off)"
            };
            let highlight_line = if self.editing_highlight {
                render_cursor_line(&self.highlight_text, self.highlight_cursor)
            } else if let Some(err) = &self.highlight_status {
                Line::from(err.clone())
            } else if self.highlight_text.is_empty() {
                Line::from("(none)")
            } else {
                let state = if self.highlight_enabled { "on" } else { "off" };
                Line::from(format!("{} [{state}]", self.highlight_text))
            };
            let highlight_body_style = if self.highlight_status.is_some() {
                Style::default().fg(self.theme.status_error)
            } else {
                Style::default()
            };
            let highlight_widget = Paragraph::new(highlight_line)
                .style(highlight_body_style)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(border_style)
                        .title(highlight_title),
                );
            frame.render_widget(highlight_widget, highlight_area);
        }

        /// Whether `row` matches the active highlight predicate, if any is
        /// compiled and enabled. Eval errors are treated as non-match rather
        /// than propagated, so one bad row can't crash the render loop.
        fn row_is_highlighted(&self, row: &[Cell]) -> bool {
            self.highlight_enabled
                && self
                    .highlight
                    .as_ref()
                    .is_some_and(|p| p.eval(row, &self.result.columns).unwrap_or(false))
        }
    }

    struct App {
        panes: Vec<Pane>,
        focused: usize,
        /// True: only the focused pane is drawn, full-area (stacked/tabbed).
        /// False: all panes are drawn side-by-side, as columns. Background
        /// panes keep processing file events/requeries either way (`run`
        /// drains every pane each tick regardless of what's drawn).
        stacked: bool,
    }

    impl App {
        fn new(
            paths: &[PathBuf],
            sql: String,
            theme: Theme,
            color_rules_cfg: &[ColorRuleConfig],
        ) -> io::Result<Self> {
            let panes = paths
                .iter()
                .map(|path| Pane::new(path, sql.clone(), theme, color_rules_cfg))
                .collect::<io::Result<Vec<_>>>()?;
            Ok(Self {
                panes,
                focused: 0,
                stacked: cache::TuiState::load(&cache::tui_state_path()).stacked,
            })
        }

        fn run(mut self, terminal: &mut Tui) -> io::Result<()> {
            let mut last_tick = Instant::now();
            loop {
                terminal.draw(|frame| self.draw(frame))?;

                for pane in &mut self.panes {
                    if pane.drain_file_events()? {
                        pane.requery(true)?;
                    }
                }

                let timeout = TICK_RATE.saturating_sub(last_tick.elapsed());
                if event::poll(timeout)? {
                    if let Event::Key(key) = event::read()? {
                        if key.kind == KeyEventKind::Press
                            && self.handle_key(key.code, key.modifiers)?
                        {
                            return Ok(());
                        }
                    }
                }
                if last_tick.elapsed() >= TICK_RATE {
                    last_tick = Instant::now();
                }
            }
        }

        /// Returns true if the whole app should quit.
        fn handle_key(&mut self, code: KeyCode, mods: KeyModifiers) -> io::Result<bool> {
            let editing = self
                .panes
                .get(self.focused)
                .is_some_and(|p| p.editing_filter || p.editing_highlight);
            let detail_open = self.panes.get(self.focused).is_some_and(|p| p.detail_open);

            // Pane-management keys are only intercepted outside of filter/
            // highlight editing, so '/'- or '?'-mode can still type
            // 'q'/'x'/etc. as ordinary characters.
            if !editing {
                match code {
                    KeyCode::Char('q') => return Ok(true),
                    // With a single pane, Esc quits (matches the original
                    // single-pane behavior); with multiple panes it's a no-op
                    // here since closing/quitting has dedicated keys below.
                    // Detail pane open takes priority: Esc closes it first
                    // (via the fall-through to pane.handle_key below)
                    // rather than quitting/no-op-ing.
                    KeyCode::Esc if self.panes.len() == 1 && !detail_open => return Ok(true),
                    KeyCode::Tab if self.panes.len() > 1 => {
                        self.focused = (self.focused + 1) % self.panes.len();
                        return Ok(false);
                    }
                    KeyCode::Char('v') if self.panes.len() > 1 => {
                        self.stacked = !self.stacked;
                        cache::TuiState {
                            stacked: self.stacked,
                        }
                        .save(&cache::tui_state_path())?;
                        return Ok(false);
                    }
                    KeyCode::Char('x') => {
                        if self.panes.len() <= 1 {
                            return Ok(true);
                        }
                        self.panes.remove(self.focused);
                        if self.focused >= self.panes.len() {
                            self.focused = self.panes.len() - 1;
                        }
                        return Ok(false);
                    }
                    _ => {}
                }
            }

            if let Some(pane) = self.panes.get_mut(self.focused) {
                pane.handle_key(code, mods)?;
            }
            Ok(false)
        }

        fn draw(&mut self, frame: &mut ratatui::Frame) {
            let focused = self.focused;

            if self.stacked {
                let area = frame.area();
                if let Some(pane) = self.panes.get_mut(focused) {
                    pane.draw(frame, area, true);
                }
                return;
            }

            let n = self.panes.len().max(1);
            #[allow(clippy::cast_possible_truncation)]
            let constraints: Vec<Constraint> =
                (0..n).map(|_| Constraint::Ratio(1, n as u32)).collect();
            let columns = Layout::default()
                .direction(Direction::Horizontal)
                .constraints(constraints)
                .split(frame.area());

            for (i, pane) in self.panes.iter_mut().enumerate() {
                if let Some(&area) = columns.get(i) {
                    pane.draw(frame, area, i == focused);
                }
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        const SAMPLE_LOG: &str = "tests/logs/sample.log";
        const SAMPLE_LOG_2: &str = "tests/logs/sample2.log";

        /// Two *distinct* fixtures (different seed/line count), so pane
        /// isolation is exercised with genuinely different data per pane
        /// rather than the same file opened twice.
        fn two_pane_app() -> App {
            App::new(
                &[PathBuf::from(SAMPLE_LOG), PathBuf::from(SAMPLE_LOG_2)],
                "SELECT * FROM log WHERE severity >= 'DEBUG'".to_string(),
                Theme::default(),
                &[],
            )
            .expect("open two panes")
        }

        fn one_pane_app() -> App {
            App::new(
                &[PathBuf::from(SAMPLE_LOG)],
                "SELECT * FROM log WHERE severity >= 'DEBUG'".to_string(),
                Theme::default(),
                &[],
            )
            .expect("open one pane")
        }

        #[test]
        fn defaults_to_stacked_layout() {
            let app = two_pane_app();
            assert!(app.stacked, "issue #50: stacked/tabbed is the default");
        }

        #[test]
        fn v_toggles_layout_mode() {
            let mut app = two_pane_app();
            assert!(app.stacked);
            assert!(!app
                .handle_key(KeyCode::Char('v'), KeyModifiers::NONE)
                .unwrap());
            assert!(!app.stacked);
            assert!(!app
                .handle_key(KeyCode::Char('v'), KeyModifiers::NONE)
                .unwrap());
            assert!(app.stacked);
        }

        #[test]
        fn v_is_a_noop_with_a_single_pane() {
            let mut app = one_pane_app();
            assert!(app.stacked);
            assert!(!app
                .handle_key(KeyCode::Char('v'), KeyModifiers::NONE)
                .unwrap());
            assert!(app.stacked, "toggling with one pane should be a no-op");
        }

        #[test]
        fn stacked_mode_still_maintains_all_pane_state() {
            let mut app = two_pane_app();
            app.stacked = true;
            app.focused = 0;

            // In stacked mode, even the non-focused pane (1) should still be
            // in the app and maintain its independent state. Switching focus
            // should show different data.
            assert_eq!(app.panes.len(), 2);
            let pane0_rows = app.panes[0].result.rows.len();
            let pane1_rows = app.panes[1].result.rows.len();

            // Panes have different row counts (per two_pane_app setup).
            assert_ne!(
                pane0_rows, pane1_rows,
                "test setup: panes should have different row counts"
            );

            // Switching focus should be ready to show different data without
            // the background pane losing state.
            app.focused = 1;
            assert_eq!(
                app.panes[1].result.rows.len(),
                pane1_rows,
                "background pane's state should not change when not drawn"
            );
        }

        #[test]
        fn tab_cycles_focus_across_panes() {
            let mut app = two_pane_app();
            assert_eq!(app.focused, 0);
            assert!(!app.handle_key(KeyCode::Tab, KeyModifiers::NONE).unwrap());
            assert_eq!(app.focused, 1);
            assert!(!app.handle_key(KeyCode::Tab, KeyModifiers::NONE).unwrap());
            assert_eq!(app.focused, 0);
        }

        #[test]
        fn closing_a_pane_does_not_affect_the_other() {
            let mut app = two_pane_app();
            let remaining_rows_before = app.panes[1].result.rows.len();

            // focused starts at 0 (SAMPLE_LOG), so closing it should leave
            // pane 1 (SAMPLE_LOG_2) behind, untouched.
            assert!(!app
                .handle_key(KeyCode::Char('x'), KeyModifiers::NONE)
                .unwrap());
            assert_eq!(app.panes.len(), 1);
            assert_eq!(app.focused, 0);
            assert_eq!(app.panes[0].path, PathBuf::from(SAMPLE_LOG_2));
            assert_eq!(app.panes[0].result.rows.len(), remaining_rows_before);
        }

        #[test]
        fn panes_hold_genuinely_distinct_data() {
            let app = two_pane_app();
            assert_eq!(app.panes[0].path, PathBuf::from(SAMPLE_LOG));
            assert_eq!(app.panes[1].path, PathBuf::from(SAMPLE_LOG_2));
            assert_ne!(
                app.panes[0].result.rows.len(),
                app.panes[1].result.rows.len(),
                "the two fixtures have different line counts, so their \
                 filtered row counts should differ too"
            );
        }

        #[test]
        fn closing_the_last_pane_quits() {
            let mut app = one_pane_app();
            assert!(app
                .handle_key(KeyCode::Char('x'), KeyModifiers::NONE)
                .unwrap());
        }

        #[test]
        fn q_quits_regardless_of_pane_count() {
            let mut app = two_pane_app();
            assert!(app
                .handle_key(KeyCode::Char('q'), KeyModifiers::NONE)
                .unwrap());
        }

        #[test]
        fn esc_quits_single_pane_but_not_multi_pane() {
            let mut multi = two_pane_app();
            assert!(!multi.handle_key(KeyCode::Esc, KeyModifiers::NONE).unwrap());

            let mut single = one_pane_app();
            assert!(single.handle_key(KeyCode::Esc, KeyModifiers::NONE).unwrap());
        }

        #[test]
        fn d_toggles_detail_pane() {
            let mut app = one_pane_app();
            assert!(!app.panes[0].detail_open);
            assert!(!app
                .handle_key(KeyCode::Char('d'), KeyModifiers::NONE)
                .unwrap());
            assert!(app.panes[0].detail_open);
            assert!(!app
                .handle_key(KeyCode::Char('d'), KeyModifiers::NONE)
                .unwrap());
            assert!(!app.panes[0].detail_open);
        }

        #[test]
        fn b_toggles_bars_hidden() {
            let mut app = one_pane_app();
            assert!(!app.panes[0].bars_hidden);
            assert!(!app
                .handle_key(KeyCode::Char('b'), KeyModifiers::NONE)
                .unwrap());
            assert!(app.panes[0].bars_hidden);
            assert!(!app
                .handle_key(KeyCode::Char('b'), KeyModifiers::NONE)
                .unwrap());
            assert!(!app.panes[0].bars_hidden);
        }

        #[test]
        fn entering_filter_edit_unhides_bars() {
            let mut app = one_pane_app();
            app.handle_key(KeyCode::Char('b'), KeyModifiers::NONE)
                .unwrap();
            assert!(app.panes[0].bars_hidden);

            app.handle_key(KeyCode::Char('/'), KeyModifiers::NONE)
                .unwrap();
            assert!(app.panes[0].editing_filter);
            assert!(
                !app.panes[0].bars_hidden,
                "entering filter edit should unhide bars so the typed filter is visible"
            );
        }

        #[test]
        fn entering_highlight_edit_unhides_bars() {
            let mut app = one_pane_app();
            app.handle_key(KeyCode::Char('b'), KeyModifiers::NONE)
                .unwrap();
            assert!(app.panes[0].bars_hidden);

            app.handle_key(KeyCode::Char('?'), KeyModifiers::NONE)
                .unwrap();
            assert!(app.panes[0].editing_highlight);
            assert!(
                !app.panes[0].bars_hidden,
                "entering highlight edit should unhide bars so the typed expression is visible"
            );
        }

        #[test]
        fn esc_closes_detail_pane_instead_of_quitting_single_pane_app() {
            let mut app = one_pane_app();
            app.handle_key(KeyCode::Char('d'), KeyModifiers::NONE)
                .unwrap();
            assert!(app.panes[0].detail_open);

            // With detail open, Esc must close it, not quit the app -- even
            // though a single pane would normally quit on Esc.
            assert!(!app.handle_key(KeyCode::Esc, KeyModifiers::NONE).unwrap());
            assert!(!app.panes[0].detail_open);

            // Detail now closed: Esc goes back to its normal single-pane
            // quit behavior.
            assert!(app.handle_key(KeyCode::Esc, KeyModifiers::NONE).unwrap());
        }

        #[test]
        fn field_lines_show_every_column_except_raw() {
            let app = one_pane_app();
            let pane = &app.panes[0];
            let row = pane.selected_row().expect("a selected row");
            let lines = pane.field_lines_for(row);

            assert!(
                lines.iter().all(|l| !l.starts_with("raw: ")),
                "raw must not be repeated as a field line: {lines:?}"
            );
            for name in &pane.result.columns {
                if name == "raw" {
                    continue;
                }
                assert!(
                    lines.iter().any(|l| l.starts_with(&format!("{name}: "))),
                    "expected a line for column '{name}', got: {lines:?}"
                );
            }
        }

        #[test]
        fn field_lines_follow_selection_across_j_k() {
            let mut app = one_pane_app();
            let row_before = app.panes[0].selected_row().unwrap().clone();
            let before = app.panes[0].field_lines_for(&row_before);
            assert!(!app
                .handle_key(KeyCode::Char('j'), KeyModifiers::NONE)
                .unwrap());
            let row_after = app.panes[0].selected_row().unwrap().clone();
            let after = app.panes[0].field_lines_for(&row_after);
            assert_ne!(
                before, after,
                "moving the selection should change which row's fields are shown"
            );
        }

        #[test]
        fn navigation_key_reaches_focused_pane_only() {
            let mut app = two_pane_app();
            let other_selected = app.panes[1].list_state.selected();
            assert!(!app
                .handle_key(KeyCode::Char('k'), KeyModifiers::NONE)
                .unwrap());
            assert_eq!(
                app.panes[1].list_state.selected(),
                other_selected,
                "unfocused pane's selection must not change"
            );
        }

        #[test]
        fn pane_defaults_to_reverse_newest_first() {
            let app = one_pane_app();
            let len = app.panes[0].result.rows.len();
            assert!(len > 1, "fixture needs multiple rows for this test");
            assert!(
                app.panes[0].reverse,
                "panes should open newest-first by default"
            );
            assert_eq!(app.panes[0].list_state.selected(), Some(0));
        }

        #[test]
        fn reverse_toggle_flips_render_order_and_jumps_to_latest() {
            let mut app = one_pane_app();
            let len = app.panes[0].result.rows.len();
            assert!(len > 1, "fixture needs multiple rows for this test");
            assert!(app.panes[0].reverse);
            assert_eq!(app.panes[0].list_state.selected(), Some(0));

            // Toggling flips to non-reverse and jumps to "latest is the last index".
            assert!(!app
                .handle_key(KeyCode::Char('R'), KeyModifiers::NONE)
                .unwrap());
            let pane = &app.panes[0];
            assert!(!pane.reverse);
            assert_eq!(pane.list_state.selected(), Some(len - 1));

            // Toggling back returns to reverse mode, latest at index 0.
            assert!(!app
                .handle_key(KeyCode::Char('R'), KeyModifiers::NONE)
                .unwrap());
            let pane = &app.panes[0];
            assert!(pane.reverse);
            assert_eq!(
                pane.list_state.selected(),
                Some(0),
                "toggling reverse should jump the view to the latest row (index 0 in reverse mode)"
            );
        }

        #[test]
        fn reverse_mode_keeps_newest_row_at_index_zero_after_follow_tail_requery() {
            let mut app = one_pane_app();
            app.panes[0].reverse = true;
            app.panes[0].list_state.select(Some(0));

            // Simulate a live-append refresh (follow_tail = true) by
            // re-running the same query; row count doesn't actually change
            // here (no new data was written), but this exercises the
            // reverse-aware "was at latest -> stay at latest" path that a
            // real appended line would hit.
            app.panes[0].requery(true).unwrap();

            assert_eq!(
                app.panes[0].list_state.selected(),
                Some(0),
                "reverse mode's 'latest' position is index 0, must stay there across a follow-tail requery"
            );
        }

        /// Type each char of `s` as a `KeyCode::Char` key press.
        fn type_str(app: &mut App, s: &str) {
            for c in s.chars() {
                assert!(!app
                    .handle_key(KeyCode::Char(c), KeyModifiers::NONE)
                    .unwrap());
            }
        }

        #[test]
        fn highlight_expression_compiles_and_marks_matching_rows() {
            let mut app = one_pane_app();
            assert!(!app
                .handle_key(KeyCode::Char('?'), KeyModifiers::NONE)
                .unwrap());
            type_str(&mut app, "severity >= ERR");
            assert!(!app.handle_key(KeyCode::Enter, KeyModifiers::NONE).unwrap());

            let pane = &app.panes[0];
            assert!(pane.highlight.is_some());
            assert!(pane.highlight_enabled);
            assert!(
                pane.highlight_status.is_none(),
                "no error expected: {:?}",
                pane.highlight_status
            );

            let has_match = pane
                .result
                .rows
                .iter()
                .any(|row| pane.row_is_highlighted(row));
            assert!(
                has_match,
                "expected at least one row to match severity >= ERR"
            );
        }

        #[test]
        fn highlight_toggle_disables_without_clearing_expression() {
            let mut app = one_pane_app();
            app.handle_key(KeyCode::Char('?'), KeyModifiers::NONE)
                .unwrap();
            type_str(&mut app, "severity >= ERR");
            app.handle_key(KeyCode::Enter, KeyModifiers::NONE).unwrap();

            assert!(!app
                .handle_key(KeyCode::Char('h'), KeyModifiers::NONE)
                .unwrap());
            let pane = &app.panes[0];
            assert!(!pane.highlight_enabled);
            assert_eq!(pane.highlight_text, "severity >= ERR");
            assert!(
                pane.highlight.is_some(),
                "compiled predicate is retained, not cleared"
            );

            assert!(!app
                .handle_key(KeyCode::Char('h'), KeyModifiers::NONE)
                .unwrap());
            assert!(app.panes[0].highlight_enabled);
        }

        #[test]
        fn highlight_toggle_is_a_no_op_with_no_expression_set() {
            let mut app = one_pane_app();
            assert!(!app
                .handle_key(KeyCode::Char('h'), KeyModifiers::NONE)
                .unwrap());
            assert!(!app.panes[0].highlight_enabled);
        }

        #[test]
        fn invalid_highlight_expression_reports_an_error_without_crashing() {
            let mut app = one_pane_app();
            app.handle_key(KeyCode::Char('?'), KeyModifiers::NONE)
                .unwrap();
            type_str(&mut app, "not a valid expression at all");
            assert!(!app.handle_key(KeyCode::Enter, KeyModifiers::NONE).unwrap());

            let pane = &app.panes[0];
            assert!(pane.highlight.is_none());
            assert!(pane
                .highlight_status
                .as_deref()
                .is_some_and(|s| s.contains("highlight error")));
        }

        #[test]
        fn invalid_highlight_expression_does_not_pollute_the_filter_bar() {
            // Regression test: a highlight compile error must show in the
            // highlight bar's own status, never overwrite the filter bar's
            // display of the active filter/SQL.
            let mut app = one_pane_app();
            let original_filter_text = app.panes[0].filter_text.clone();

            app.handle_key(KeyCode::Char('?'), KeyModifiers::NONE)
                .unwrap();
            type_str(&mut app, "not a valid expression at all");
            app.handle_key(KeyCode::Enter, KeyModifiers::NONE).unwrap();

            let pane = &app.panes[0];
            assert!(
                pane.filter_status.is_none(),
                "filter bar must be unaffected"
            );
            assert_eq!(pane.filter_text, original_filter_text);
        }

        #[test]
        fn invalid_highlight_expression_clears_any_previously_compiled_highlight() {
            let mut app = one_pane_app();
            app.handle_key(KeyCode::Char('?'), KeyModifiers::NONE)
                .unwrap();
            type_str(&mut app, "severity >= ERR");
            app.handle_key(KeyCode::Enter, KeyModifiers::NONE).unwrap();
            assert!(app.panes[0].highlight.is_some());

            // Now overwrite with a broken expression -- the stale compiled
            // predicate from the previous valid one must not linger.
            app.handle_key(KeyCode::Char('?'), KeyModifiers::NONE)
                .unwrap();
            for _ in 0.."severity >= ERR".len() {
                app.handle_key(KeyCode::Backspace, KeyModifiers::NONE)
                    .unwrap();
            }
            type_str(&mut app, "not valid");
            app.handle_key(KeyCode::Enter, KeyModifiers::NONE).unwrap();

            let pane = &app.panes[0];
            assert!(
                pane.highlight.is_none(),
                "stale predicate must be cleared on a new compile error"
            );
            assert!(!pane.highlight_enabled);
        }

        #[test]
        fn esc_cancels_highlight_edit_without_applying() {
            let mut app = one_pane_app();
            app.handle_key(KeyCode::Char('?'), KeyModifiers::NONE)
                .unwrap();
            type_str(&mut app, "severity >= ERR");
            assert!(!app.handle_key(KeyCode::Esc, KeyModifiers::NONE).unwrap());

            let pane = &app.panes[0];
            assert!(!pane.editing_highlight);
            assert!(
                pane.highlight.is_none(),
                "Esc must not apply the typed expression"
            );
        }

        #[test]
        fn edit_line_ctrl_a_and_ctrl_e_jump_to_start_and_end() {
            let mut text = "hello world".to_string();
            let mut cursor = 5;
            assert!(edit_line(
                &mut text,
                &mut cursor,
                KeyCode::Char('a'),
                KeyModifiers::CONTROL
            ));
            assert_eq!(cursor, 0);
            assert!(edit_line(
                &mut text,
                &mut cursor,
                KeyCode::Char('e'),
                KeyModifiers::CONTROL
            ));
            assert_eq!(cursor, 11);
            assert_eq!(text, "hello world");
        }

        #[test]
        fn edit_line_ctrl_b_ctrl_f_and_arrows_move_one_char() {
            let mut text = "abc".to_string();
            let mut cursor = 1;
            assert!(edit_line(
                &mut text,
                &mut cursor,
                KeyCode::Char('f'),
                KeyModifiers::CONTROL
            ));
            assert_eq!(cursor, 2);
            assert!(edit_line(
                &mut text,
                &mut cursor,
                KeyCode::Char('b'),
                KeyModifiers::CONTROL
            ));
            assert_eq!(cursor, 1);
            assert!(edit_line(
                &mut text,
                &mut cursor,
                KeyCode::Right,
                KeyModifiers::NONE
            ));
            assert_eq!(cursor, 2);
            assert!(edit_line(
                &mut text,
                &mut cursor,
                KeyCode::Left,
                KeyModifiers::NONE
            ));
            assert_eq!(cursor, 1);

            // Clamped at both ends.
            cursor = 0;
            edit_line(&mut text, &mut cursor, KeyCode::Left, KeyModifiers::NONE);
            assert_eq!(cursor, 0);
            cursor = 3;
            edit_line(&mut text, &mut cursor, KeyCode::Right, KeyModifiers::NONE);
            assert_eq!(cursor, 3);
        }

        #[test]
        fn edit_line_ctrl_k_kills_to_end_of_line() {
            let mut text = "hello world".to_string();
            let mut cursor = 5;
            assert!(edit_line(
                &mut text,
                &mut cursor,
                KeyCode::Char('k'),
                KeyModifiers::CONTROL
            ));
            assert_eq!(text, "hello");
            assert_eq!(cursor, 5);
        }

        #[test]
        fn edit_line_ctrl_u_kills_to_start_of_line() {
            let mut text = "hello world".to_string();
            let mut cursor = 6;
            assert!(edit_line(
                &mut text,
                &mut cursor,
                KeyCode::Char('u'),
                KeyModifiers::CONTROL
            ));
            assert_eq!(text, "world");
            assert_eq!(cursor, 0);
        }

        #[test]
        fn edit_line_ctrl_w_and_alt_backspace_delete_word_before_cursor() {
            let mut text = "hello brave world".to_string();
            let mut cursor = 11; // just after "brave"
            assert!(edit_line(
                &mut text,
                &mut cursor,
                KeyCode::Char('w'),
                KeyModifiers::CONTROL
            ));
            assert_eq!(text, "hello  world");
            assert_eq!(cursor, 6);

            let mut text2 = "hello brave world".to_string();
            let mut cursor2 = 11;
            assert!(edit_line(
                &mut text2,
                &mut cursor2,
                KeyCode::Backspace,
                KeyModifiers::ALT
            ));
            assert_eq!(text2, "hello  world");
            assert_eq!(cursor2, 6);
        }

        #[test]
        fn edit_line_alt_b_and_alt_f_move_one_word() {
            let mut text = "hello brave world".to_string();
            let mut cursor = 11; // just after "brave"
            assert!(edit_line(
                &mut text,
                &mut cursor,
                KeyCode::Char('b'),
                KeyModifiers::ALT
            ));
            assert_eq!(cursor, 6, "should land at start of 'brave'");
            assert!(edit_line(
                &mut text,
                &mut cursor,
                KeyCode::Char('f'),
                KeyModifiers::ALT
            ));
            assert_eq!(cursor, 11, "should land at end of 'brave'");
        }

        #[test]
        fn edit_line_ctrl_d_deletes_char_under_cursor() {
            let mut text = "hello".to_string();
            let mut cursor = 1;
            assert!(edit_line(
                &mut text,
                &mut cursor,
                KeyCode::Char('d'),
                KeyModifiers::CONTROL
            ));
            assert_eq!(text, "hllo");
            assert_eq!(cursor, 1);
        }

        #[test]
        fn edit_line_backspace_deletes_char_before_cursor_not_always_last() {
            let mut text = "hello".to_string();
            let mut cursor = 2;
            assert!(edit_line(
                &mut text,
                &mut cursor,
                KeyCode::Backspace,
                KeyModifiers::NONE
            ));
            assert_eq!(text, "hllo");
            assert_eq!(cursor, 1);
        }

        #[test]
        fn edit_line_plain_char_inserts_at_cursor_not_only_at_end() {
            let mut text = "helo".to_string();
            let mut cursor = 3;
            assert!(edit_line(
                &mut text,
                &mut cursor,
                KeyCode::Char('l'),
                KeyModifiers::NONE
            ));
            assert_eq!(text, "hello");
            assert_eq!(cursor, 4);
        }

        #[test]
        fn edit_line_ignores_unrecognized_key() {
            let mut text = "abc".to_string();
            let mut cursor = 1;
            assert!(!edit_line(
                &mut text,
                &mut cursor,
                KeyCode::Tab,
                KeyModifiers::NONE
            ));
            assert_eq!(text, "abc");
            assert_eq!(cursor, 1);
        }

        /// Builds a `History` with `entries` as most-recent-first, without
        /// touching the real XDG cache dir: writes them oldest-first to a
        /// throwaway temp file, then loads (which reverses the order).
        fn history_of(entries: &[&str]) -> History {
            let path = std::env::temp_dir().join(format!(
                "loglume-recall-test-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or_default()
            ));
            let oldest_first = entries.iter().rev().copied().collect::<Vec<_>>().join("\n");
            std::fs::write(&path, oldest_first + "\n").unwrap();
            let history = History::load(&path);
            std::fs::remove_file(&path).ok();
            history
        }

        #[test]
        fn recall_history_up_walks_toward_older_entries_saving_draft() {
            let history = history_of(&["c", "b", "a"]);
            let mut pos = None;
            let mut draft = String::new();
            let mut text = "typing".to_string();
            let mut cursor = text.chars().count();

            recall_history(&history, &mut pos, &mut draft, &mut text, &mut cursor, 1);
            assert_eq!(text, "c");
            assert_eq!(draft, "typing");
            assert_eq!(cursor, 1);

            recall_history(&history, &mut pos, &mut draft, &mut text, &mut cursor, 1);
            assert_eq!(text, "b");

            recall_history(&history, &mut pos, &mut draft, &mut text, &mut cursor, 1);
            assert_eq!(text, "a");

            // At the oldest entry, another Up is a no-op.
            recall_history(&history, &mut pos, &mut draft, &mut text, &mut cursor, 1);
            assert_eq!(text, "a");
        }

        #[test]
        fn recall_history_down_walks_back_to_draft() {
            let history = history_of(&["c", "b"]);
            let mut pos = None;
            let mut draft = String::new();
            let mut text = "typing".to_string();
            let mut cursor = text.chars().count();

            recall_history(&history, &mut pos, &mut draft, &mut text, &mut cursor, 1);
            recall_history(&history, &mut pos, &mut draft, &mut text, &mut cursor, 1);
            assert_eq!(text, "b");

            recall_history(&history, &mut pos, &mut draft, &mut text, &mut cursor, -1);
            assert_eq!(text, "c");

            recall_history(&history, &mut pos, &mut draft, &mut text, &mut cursor, -1);
            assert_eq!(text, "typing");
            assert_eq!(pos, None);

            // Down with nothing to return to (not navigating) is a no-op.
            recall_history(&history, &mut pos, &mut draft, &mut text, &mut cursor, -1);
            assert_eq!(text, "typing");
        }

        #[test]
        fn recall_history_up_is_a_no_op_with_empty_history() {
            let history = History::default();
            let mut pos = None;
            let mut draft = String::new();
            let mut text = "typing".to_string();
            let mut cursor = text.chars().count();

            recall_history(&history, &mut pos, &mut draft, &mut text, &mut cursor, 1);
            assert_eq!(text, "typing");
            assert_eq!(pos, None);
        }

        #[test]
        fn filter_edit_supports_mid_line_cursor_movement_and_editing() {
            let mut app = one_pane_app();
            app.handle_key(KeyCode::Char('/'), KeyModifiers::NONE)
                .unwrap();
            app.panes[0].filter_text.clear();
            app.panes[0].filter_cursor = 0;
            type_str(&mut app, "hello world");
            assert_eq!(app.panes[0].filter_cursor, 11);

            // Ctrl-A to start, Ctrl-F x6 to land right after "hello ".
            app.handle_key(KeyCode::Char('a'), KeyModifiers::CONTROL)
                .unwrap();
            for _ in 0..6 {
                app.handle_key(KeyCode::Char('f'), KeyModifiers::CONTROL)
                    .unwrap();
            }
            assert_eq!(app.panes[0].filter_cursor, 6);

            // Ctrl-K kills "world" from here.
            app.handle_key(KeyCode::Char('k'), KeyModifiers::CONTROL)
                .unwrap();
            assert_eq!(app.panes[0].filter_text, "hello ");
            assert_eq!(app.panes[0].filter_cursor, 6);
        }

        #[test]
        fn default_theme_is_catppuccin_mocha() {
            let theme = Theme::default();
            assert_eq!(theme.border_focused, Color::Rgb(0x89, 0xb4, 0xfa));
            assert_eq!(theme.highlight_bg, Color::Rgb(0xf9, 0xe2, 0xaf));
            assert_eq!(theme.highlight_fg, Color::Rgb(0x1e, 0x1e, 0x2e));
            assert_eq!(theme.detail_dim, Color::Rgb(0x6c, 0x70, 0x86));
            assert_eq!(theme.status_error, Color::Rgb(0xf3, 0x8b, 0xa8));
            assert_eq!(theme.zebra_bg, Color::Rgb(0x31, 0x32, 0x44));
            assert_eq!(theme.severity_error, Color::Rgb(0xf3, 0x8b, 0xa8));
            assert_eq!(theme.severity_warn, Color::Rgb(0xfa, 0xb3, 0x87));
            assert_eq!(theme.severity_dim, Color::Rgb(0x6c, 0x70, 0x86));
        }

        #[test]
        fn theme_resolve_with_no_overrides_keeps_defaults() {
            let theme = Theme::resolve(&ThemeConfig::default());
            assert_eq!(theme.border_focused, Theme::default().border_focused);
        }

        #[test]
        fn theme_resolve_applies_a_valid_override() {
            let cfg = ThemeConfig {
                border_focused: Some("#ff0000".to_string()),
                ..Default::default()
            };
            let theme = Theme::resolve(&cfg);
            assert_eq!(theme.border_focused, Color::Rgb(0xff, 0x00, 0x00));
            // Untouched keys still fall back to the default.
            assert_eq!(theme.highlight_bg, Theme::default().highlight_bg);
        }

        #[test]
        fn theme_resolve_ignores_an_invalid_override() {
            let cfg = ThemeConfig {
                border_focused: Some("not-a-color".to_string()),
                ..Default::default()
            };
            let theme = Theme::resolve(&cfg);
            assert_eq!(
                theme.border_focused,
                Theme::default().border_focused,
                "an unparsable hex value must fall back to the default, not panic or leave a garbage color"
            );
        }

        #[test]
        fn severity_color_maps_bands_to_theme_colors() {
            let theme = Theme::default();
            assert_eq!(
                severity_color(&theme, Some(Severity::Fatal as u8 as i64)),
                Some(theme.severity_error)
            );
            assert_eq!(
                severity_color(&theme, Some(Severity::Error as u8 as i64)),
                Some(theme.severity_error)
            );
            assert_eq!(
                severity_color(&theme, Some(Severity::Warn as u8 as i64)),
                Some(theme.severity_warn)
            );
            assert_eq!(
                severity_color(&theme, Some(Severity::Info as u8 as i64)),
                Some(theme.severity_dim)
            );
            assert_eq!(
                severity_color(&theme, Some(Severity::Debug as u8 as i64)),
                Some(theme.severity_dim)
            );
            assert_eq!(
                severity_color(&theme, Some(Severity::Trace as u8 as i64)),
                Some(theme.severity_dim)
            );
        }

        #[test]
        fn severity_color_is_none_without_a_severity_column() {
            assert_eq!(severity_color(&Theme::default(), None), None);
        }

        #[test]
        fn compile_color_rules_skips_invalid_expression() {
            let engine = open_engine(Path::new(SAMPLE_LOG)).unwrap();
            let rules = compile_color_rules(
                &engine,
                &[ColorRuleConfig {
                    expr: "not a valid expr !!!".to_string(),
                    color: "#ff0000".to_string(),
                }],
            );
            assert!(rules.is_empty());
        }

        #[test]
        fn compile_color_rules_skips_invalid_color() {
            let engine = open_engine(Path::new(SAMPLE_LOG)).unwrap();
            let rules = compile_color_rules(
                &engine,
                &[ColorRuleConfig {
                    expr: "severity >= WARN".to_string(),
                    color: "not-a-color".to_string(),
                }],
            );
            assert!(rules.is_empty());
        }

        #[test]
        fn compile_color_rules_compiles_valid_entries() {
            let engine = open_engine(Path::new(SAMPLE_LOG)).unwrap();
            let rules = compile_color_rules(
                &engine,
                &[ColorRuleConfig {
                    expr: "severity >= WARN".to_string(),
                    color: "#ff0000".to_string(),
                }],
            );
            assert_eq!(rules.len(), 1);
            assert_eq!(rules[0].color, Color::Rgb(0xff, 0x00, 0x00));
        }

        #[test]
        fn color_rule_fg_returns_first_match_in_order() {
            let engine = open_engine(Path::new(SAMPLE_LOG)).unwrap();
            let rules = compile_color_rules(
                &engine,
                &[
                    ColorRuleConfig {
                        expr: "severity >= WARN".to_string(),
                        color: "#ff0000".to_string(),
                    },
                    ColorRuleConfig {
                        expr: "severity >= INFO".to_string(),
                        color: "#00ff00".to_string(),
                    },
                ],
            );
            let columns = vec!["severity".to_string()];
            // Error severity matches both rules; the first in order wins.
            let row = vec![Cell::Int(Severity::Error as u8 as i64)];
            assert_eq!(
                color_rule_fg(&rules, &row, &columns),
                Some(Color::Rgb(0xff, 0x00, 0x00))
            );
        }

        #[test]
        fn color_rule_fg_is_none_when_nothing_matches() {
            let engine = open_engine(Path::new(SAMPLE_LOG)).unwrap();
            let rules = compile_color_rules(
                &engine,
                &[ColorRuleConfig {
                    expr: "severity >= WARN".to_string(),
                    color: "#ff0000".to_string(),
                }],
            );
            let columns = vec!["severity".to_string()];
            let row = vec![Cell::Int(Severity::Info as u8 as i64)];
            assert_eq!(color_rule_fg(&rules, &row, &columns), None);
        }

        #[test]
        fn parse_hex_color_accepts_with_and_without_hash() {
            assert_eq!(
                parse_hex_color(Some("#89b4fa")),
                Some(Color::Rgb(0x89, 0xb4, 0xfa))
            );
            assert_eq!(
                parse_hex_color(Some("89b4fa")),
                Some(Color::Rgb(0x89, 0xb4, 0xfa))
            );
        }

        #[test]
        fn parse_hex_color_rejects_wrong_length_and_non_hex() {
            assert_eq!(parse_hex_color(Some("#fff")), None);
            assert_eq!(parse_hex_color(Some("#gggggg")), None);
            assert_eq!(parse_hex_color(None), None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn format_row_uses_raw_column_when_present() {
        let row = vec![Cell::Int(42), Cell::Text("hello world".to_string())];
        assert_eq!(format_row(&row, Some(1)), "hello world");
    }

    #[test]
    fn format_row_falls_back_to_all_cells_without_raw_column() {
        let row = vec![Cell::Int(42), Cell::Text("count".to_string())];
        assert_eq!(format_row(&row, None), "42\tcount");
    }

    #[test]
    fn format_row_falls_back_when_raw_column_is_not_text() {
        let row = vec![Cell::Int(42)];
        assert_eq!(format_row(&row, Some(0)), "42");
    }

    #[test]
    fn severity_ge_rewrites_to_sql() {
        let sql = rewrite_filter_to_sql("severity >= WARN").expect("valid filter");
        assert_eq!(
            sql,
            format!(
                "SELECT * FROM log WHERE severity >= {}",
                Severity::Warn as u8
            )
        );
    }

    #[test]
    fn severity_gt_rewrites_to_sql() {
        let sql = rewrite_filter_to_sql("severity > ERR").expect("valid filter");
        assert_eq!(
            sql,
            format!(
                "SELECT * FROM log WHERE severity > {}",
                Severity::Error as u8
            )
        );
    }

    #[test]
    fn severity_le_rewrites_to_sql() {
        let sql = rewrite_filter_to_sql("severity <= ERR").expect("valid filter");
        assert_eq!(
            sql,
            format!(
                "SELECT * FROM log WHERE severity <= {}",
                Severity::Error as u8
            )
        );
    }

    #[test]
    fn severity_lt_rewrites_to_sql() {
        let sql = rewrite_filter_to_sql("severity < WARN").expect("valid filter");
        assert_eq!(
            sql,
            format!(
                "SELECT * FROM log WHERE severity < {}",
                Severity::Warn as u8
            )
        );
    }

    #[test]
    fn severity_eq_rewrites_to_sql() {
        let sql = rewrite_filter_to_sql("severity = WARN").expect("valid filter");
        assert_eq!(
            sql,
            format!(
                "SELECT * FROM log WHERE severity = {}",
                Severity::Warn as u8
            )
        );
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
            format!(
                "SELECT * FROM log WHERE severity >= {} AND facility = 'auth'",
                Severity::Warn as u8
            )
        );
    }

    #[test]
    fn and_combinator_lowercase_rewrites_to_sql() {
        let sql =
            rewrite_filter_to_sql("severity >= WARN and facility = auth").expect("valid filter");
        assert_eq!(
            sql,
            format!(
                "SELECT * FROM log WHERE severity >= {} AND facility = 'auth'",
                Severity::Warn as u8
            )
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
    fn raw_sql_double_quoted_string_literal_is_normalized_to_single_quoted() {
        // #39: db-core parses "..." as a quoted identifier, not a string
        // literal -- normalize to the single-quoted form so it compiles
        // as the string literal most users expect.
        let sql = rewrite_filter_to_sql(r#"select * from log where tag = "kernel""#)
            .expect("valid filter");
        assert_eq!(sql, "select * from log where tag = 'kernel'");
    }

    #[test]
    fn normalize_double_quoted_literals_preserves_single_quoted_spans() {
        assert_eq!(
            normalize_double_quoted_literals("tag = 'kernel'"),
            "tag = 'kernel'"
        );
    }

    #[test]
    fn normalize_double_quoted_literals_escapes_embedded_single_quote() {
        assert_eq!(
            normalize_double_quoted_literals(r#"tag = "o'brien""#),
            "tag = 'o''brien'"
        );
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
            format!(
                "SELECT * FROM log WHERE severity >= {} SINCE 1h LIMIT 10",
                Severity::Warn as u8
            )
        );
    }

    #[test]
    fn resolve_highlight_expr_uses_short_form_when_recognized() {
        let expr = resolve_highlight_expr("severity >= ERR");
        assert_eq!(expr, format!("severity >= {}", Severity::Error as u8));
    }

    #[test]
    fn resolve_highlight_expr_passes_through_raw_expression() {
        // Not one of loglume's short forms (no "severity"/"facility"
        // prefix) -- passed straight to compile_predicate untouched.
        let expr = resolve_highlight_expr("message LIKE '%oom%'");
        assert_eq!(expr, "message LIKE '%oom%'");
    }

    #[test]
    fn resolve_highlight_expr_normalizes_double_quoted_literal_to_single_quoted() {
        // #39: `tag = "kernel"` and `tag = 'kernel'` must resolve to the
        // same compiled expression, not the confusing "unknown column"
        // error double quotes trigger as an identifier reference.
        let double = resolve_highlight_expr(r#"tag = "kernel""#);
        let single = resolve_highlight_expr("tag = 'kernel'");
        assert_eq!(double, single);
        assert_eq!(double, "tag = 'kernel'");
    }

    #[test]
    fn parse_duration_bare_number_is_seconds() {
        assert_eq!(parse_duration_str("30").unwrap(), Duration::from_secs(30));
    }

    #[test]
    fn parse_duration_seconds_suffix() {
        assert_eq!(parse_duration_str("45s").unwrap(), Duration::from_secs(45));
    }

    #[test]
    fn parse_duration_minutes_suffix() {
        assert_eq!(parse_duration_str("1m").unwrap(), Duration::from_secs(60));
    }

    #[test]
    fn parse_duration_hours_suffix() {
        assert_eq!(parse_duration_str("2h").unwrap(), Duration::from_secs(7200));
    }

    #[test]
    fn parse_duration_days_suffix() {
        assert_eq!(
            parse_duration_str("1d").unwrap(),
            Duration::from_secs(86400)
        );
    }

    #[test]
    fn parse_duration_rejects_unknown_unit() {
        let err = parse_duration_str("5x").unwrap_err();
        assert!(err.contains('x'));
    }

    #[test]
    fn parse_duration_rejects_non_numeric_amount() {
        assert!(parse_duration_str("abc").is_err());
    }

    #[test]
    fn parse_binary_op_recognizes_all_operators() {
        assert_eq!(parse_binary_op(">").unwrap(), BinaryOp::Gt);
        assert_eq!(parse_binary_op(">=").unwrap(), BinaryOp::Ge);
        assert_eq!(parse_binary_op("<").unwrap(), BinaryOp::Lt);
        assert_eq!(parse_binary_op("<=").unwrap(), BinaryOp::Le);
        assert_eq!(parse_binary_op("=").unwrap(), BinaryOp::Eq);
        assert_eq!(parse_binary_op("==").unwrap(), BinaryOp::Eq);
        assert_eq!(parse_binary_op("!=").unwrap(), BinaryOp::Ne);
        assert_eq!(parse_binary_op("<>").unwrap(), BinaryOp::Ne);
    }

    #[test]
    fn parse_binary_op_rejects_unrecognized() {
        assert!(parse_binary_op("~=").is_err());
    }

    #[test]
    fn resolve_emit_mode_defaults_to_on_change() {
        assert_eq!(resolve_emit_mode(None, None).unwrap(), EmitMode::OnChange);
    }

    #[test]
    fn resolve_emit_mode_builds_threshold_from_pair() {
        let mode = resolve_emit_mode(Some(">="), Some(3.0)).unwrap();
        assert_eq!(
            mode,
            EmitMode::Threshold {
                op: BinaryOp::Ge,
                threshold: 3.0
            }
        );
    }

    #[test]
    fn resolve_emit_mode_rejects_mismatched_pair() {
        assert!(resolve_emit_mode(Some(">="), None).is_err());
        assert!(resolve_emit_mode(None, Some(3.0)).is_err());
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
