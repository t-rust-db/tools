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
        let mut cfg = cfg;
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
        return tui::run(&args.files, sql);
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
    parse_where_clause(expr).unwrap_or_else(|_| expr.to_string())
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

fn run_follow(path: &Path, sql: &str, tail: usize, highlight: Option<&str>) -> io::Result<()> {
    use notify::{RecursiveMode, Watcher};
    use std::sync::mpsc;

    let mut engine = open_engine(path)?;
    let predicate = compile_highlight(&engine, highlight)?;
    let result = engine.run_query(sql).map_err(engine_err)?;
    print_result(&result, tail, predicate.as_ref());
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
/// fire. Reuses the same notify-based watcher pattern as `--follow`/the
/// TUI: on each qualifying filesystem event, `engine.refresh()` then
/// `standing_query.poll()` -- db-core owns no scheduler, the caller (this
/// loop) decides when to poll.
fn run_alert(
    path: &Path,
    sql: &str,
    window: std::time::Duration,
    mode: EmitMode,
    exec: &str,
) -> io::Result<()> {
    use notify::{RecursiveMode, Watcher};
    use std::sync::mpsc;

    let mut engine = open_engine(path)?;
    let mut standing_query =
        StandingQuery::new(&engine, sql, mode, window, window).map_err(engine_err)?;

    if let Some(event) = standing_query.poll(&mut engine).map_err(engine_err)? {
        exec_alert(exec, &event.result)?;
    }

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

    #[derive(Debug, Default, Serialize, Deserialize)]
    pub(crate) struct Config {
        #[serde(default)]
        pub(crate) tui: TuiConfig,
        /// Saved filter/query name -> resolved SQL, referenced as `@name`.
        #[serde(default)]
        pub(crate) filters: BTreeMap<String, String>,
    }

    #[derive(Debug, Default, Serialize, Deserialize)]
    pub(crate) struct TuiConfig {
        /// Fallback for `--scope` when not given on the command line.
        #[serde(default)]
        pub(crate) default_scope: Option<String>,
        /// Reserved for future key-remapping; not yet applied by the TUI.
        #[serde(default)]
        pub(crate) keybindings: BTreeMap<String, String>,
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

/// Interactive TUI viewer (`--tui`).
///
/// Reuses the same `StreamEngine`/SQL query path as the plain CLI: the
/// filter bar compiles through [`rewrite_filter_to_sql`], the same
/// function `--filter` uses, so there is exactly one place that
/// understands loglume's filter syntax.
mod tui {
    use super::{
        engine_err, format_cell, format_row, format_scope_report, open_engine,
        resolve_highlight_expr, rewrite_filter_to_sql,
    };
    use crossterm::event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind,
    };
    use crossterm::execute;
    use crossterm::terminal::{
        disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
    };
    use loglume::{Cell, CompiledPredicate, Engine, QueryResult};
    use ratatui::backend::CrosstermBackend;
    use ratatui::layout::{Constraint, Direction, Layout};
    use ratatui::style::{Color, Modifier, Style};
    use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
    use ratatui::Terminal;
    use std::io::{self, Stdout};
    use std::path::{Path, PathBuf};
    use std::sync::mpsc::{self, TryRecvError};
    use std::time::{Duration, Instant};

    type Tui = Terminal<CrosstermBackend<Stdout>>;

    const TICK_RATE: Duration = Duration::from_millis(100);

    /// Run the interactive TUI against `paths`, each opened in its own pane,
    /// all starting with `initial_sql`.
    pub(crate) fn run(paths: &[PathBuf], initial_sql: String) -> io::Result<()> {
        install_panic_hook();
        let mut terminal = init_terminal()?;
        let result = App::new(paths, initial_sql)?.run(&mut terminal);
        restore_terminal(&mut terminal)?;
        result
    }

    fn init_terminal() -> io::Result<Tui> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
        Terminal::new(CrosstermBackend::new(stdout))
    }

    fn restore_terminal(terminal: &mut Tui) -> io::Result<()> {
        disable_raw_mode()?;
        execute!(
            terminal.backend_mut(),
            LeaveAlternateScreen,
            DisableMouseCapture
        )?;
        terminal.show_cursor()
    }

    /// A panic mid-render must not leave the user's terminal in raw/
    /// alternate-screen mode, so restore it first, then chain to the
    /// default hook.
    fn install_panic_hook() {
        let original = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _ = disable_raw_mode();
            let _ = execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture);
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
        editing_filter: bool,
        /// Restrict-vs-highlight (#4/#14): narrows via `sql`/`filter_text`
        /// above; this only marks matches, never hides anything.
        highlight_text: String,
        highlight: Option<CompiledPredicate>,
        highlight_enabled: bool,
        editing_highlight: bool,
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
        watch_rx: mpsc::Receiver<notify::Result<notify::Event>>,
        _watcher: notify::RecommendedWatcher,
    }

    impl Pane {
        fn new(path: &Path, sql: String) -> io::Result<Self> {
            use notify::{RecursiveMode, Watcher};

            let mut engine = open_engine(path)?;
            let result = engine.run_query(&sql).map_err(engine_err)?;

            let (tx, rx) = mpsc::channel();
            let mut watcher = notify::recommended_watcher(move |res| {
                let _ = tx.send(res);
            })
            .map_err(|e| io::Error::other(e.to_string()))?;
            watcher
                .watch(path, RecursiveMode::NonRecursive)
                .map_err(|e| io::Error::other(e.to_string()))?;

            let mut list_state = ListState::default();
            if !result.rows.is_empty() {
                list_state.select(Some(result.rows.len() - 1));
            }

            let filter_text = sql.clone();
            Ok(Self {
                path: path.to_path_buf(),
                engine,
                sql,
                result,
                list_state,
                filter_text,
                editing_filter: false,
                highlight_text: String::new(),
                highlight: None,
                highlight_enabled: false,
                editing_highlight: false,
                reverse: false,
                filter_status: None,
                highlight_status: None,
                detail_open: false,
                watch_rx: rx,
                _watcher: watcher,
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
            let mut dirty = false;
            loop {
                match self.watch_rx.try_recv() {
                    Ok(Ok(event)) if event.kind.is_modify() || event.kind.is_create() => {
                        dirty = true;
                    }
                    Ok(_) => {}
                    Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
                }
            }
            if dirty {
                self.engine.refresh().map_err(engine_err)?;
            }
            Ok(dirty)
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
        fn handle_key(&mut self, code: KeyCode) -> io::Result<()> {
            if self.editing_filter {
                match code {
                    KeyCode::Enter => {
                        self.editing_filter = false;
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
                        self.filter_text = self.sql.clone();
                    }
                    KeyCode::Backspace => {
                        self.filter_text.pop();
                    }
                    KeyCode::Char(c) => self.filter_text.push(c),
                    _ => {}
                }
                return Ok(());
            }

            if self.editing_highlight {
                match code {
                    KeyCode::Enter => {
                        self.editing_highlight = false;
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
                    KeyCode::Esc => self.editing_highlight = false,
                    KeyCode::Backspace => {
                        self.highlight_text.pop();
                    }
                    KeyCode::Char(c) => self.highlight_text.push(c),
                    _ => {}
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
                    self.filter_text = self.sql.clone();
                }
                KeyCode::Char('?') => {
                    self.editing_highlight = true;
                }
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
        /// reversed, not the first).
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

        /// One display line per field of the selected row for the detail
        /// pane (#30): the full raw line first (if a `raw` column exists),
        /// then every other column name/value pair.
        fn detail_lines(&self) -> Vec<String> {
            let Some(row) = self.selected_row() else {
                return vec!["(no row selected)".to_string()];
            };
            let mut lines = Vec::new();
            if let Some(idx) = self.result.columns.iter().position(|c| c == "raw") {
                if let Some(cell) = row.get(idx) {
                    lines.push(format!("raw: {}", format_cell(cell)));
                }
            }
            for (name, cell) in self.result.columns.iter().zip(row.iter()) {
                if name == "raw" {
                    continue;
                }
                lines.push(format!("{name}: {}", format_cell(cell)));
            }
            lines
        }

        fn draw(&mut self, frame: &mut ratatui::Frame, area: ratatui::layout::Rect, focused: bool) {
            let mut constraints = vec![
                Constraint::Min(1),
                Constraint::Length(3),
                Constraint::Length(3),
            ];
            if self.detail_open {
                // +2 for the block's own top/bottom border.
                let height = u16::try_from(self.result.columns.len())
                    .unwrap_or(u16::MAX)
                    .saturating_add(2)
                    .clamp(4, 16);
                constraints.push(Constraint::Length(height));
            }
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints(constraints)
                .split(area);
            let (Some(&list_area), Some(&filter_area), Some(&highlight_area)) =
                (chunks.first(), chunks.get(1), chunks.get(2))
            else {
                return;
            };
            let detail_area = if self.detail_open {
                chunks.get(3).copied()
            } else {
                None
            };

            let border_style = if focused {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default()
            };

            let raw_idx = self.result.columns.iter().position(|c| c == "raw");
            let highlight_style = Style::default()
                .bg(Color::Yellow)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD);
            let to_item = |row: &Vec<Cell>| {
                let text = format_row(row, raw_idx);
                if self.row_is_highlighted(row) {
                    ListItem::new(text).style(highlight_style)
                } else {
                    ListItem::new(text)
                }
            };
            let items: Vec<ListItem> = if self.reverse {
                self.result.rows.iter().rev().map(to_item).collect()
            } else {
                self.result.rows.iter().map(to_item).collect()
            };

            let name = self
                .path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| self.path.to_string_lossy().into_owned());
            let order_tag = if self.reverse { " [newest-first]" } else { "" };
            let title = self
                .result
                .scope_report
                .as_ref()
                .map(|r| format!("{name}{order_tag} — {}", format_scope_report(r)))
                .unwrap_or_else(|| format!("{name}{order_tag}"));

            let list = List::new(items)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(border_style)
                        .title(title),
                )
                .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
            frame.render_stateful_widget(list, list_area, &mut self.list_state);

            let filter_title = if self.editing_filter {
                "filter (Enter to apply, Esc to cancel)"
            } else {
                "filter (/ edit, j/k move, d detail, R reverse, Tab pane, x close, q quit)"
            };
            let filter_body = self
                .filter_status
                .clone()
                .unwrap_or_else(|| self.filter_text.clone());
            let input = Paragraph::new(filter_body).block(
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
            let highlight_body = if self.editing_highlight {
                self.highlight_text.clone()
            } else if let Some(err) = &self.highlight_status {
                err.clone()
            } else if self.highlight_text.is_empty() {
                "(none)".to_string()
            } else {
                let state = if self.highlight_enabled { "on" } else { "off" };
                format!("{} [{state}]", self.highlight_text)
            };
            let highlight_widget = Paragraph::new(highlight_body).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(border_style)
                    .title(highlight_title),
            );
            frame.render_widget(highlight_widget, highlight_area);

            if let Some(area) = detail_area {
                let text = self.detail_lines().join("\n");
                let detail_widget = Paragraph::new(text).block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(border_style)
                        .title("detail (d/Esc to close)"),
                );
                frame.render_widget(detail_widget, area);
            }
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
    }

    impl App {
        fn new(paths: &[PathBuf], sql: String) -> io::Result<Self> {
            let panes = paths
                .iter()
                .map(|path| Pane::new(path, sql.clone()))
                .collect::<io::Result<Vec<_>>>()?;
            Ok(Self { panes, focused: 0 })
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
                        if key.kind == KeyEventKind::Press && self.handle_key(key.code)? {
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
        fn handle_key(&mut self, code: KeyCode) -> io::Result<bool> {
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
                pane.handle_key(code)?;
            }
            Ok(false)
        }

        fn draw(&mut self, frame: &mut ratatui::Frame) {
            let n = self.panes.len().max(1);
            #[allow(clippy::cast_possible_truncation)]
            let constraints: Vec<Constraint> =
                (0..n).map(|_| Constraint::Ratio(1, n as u32)).collect();
            let columns = Layout::default()
                .direction(Direction::Horizontal)
                .constraints(constraints)
                .split(frame.area());

            let focused = self.focused;
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
            )
            .expect("open two panes")
        }

        fn one_pane_app() -> App {
            App::new(
                &[PathBuf::from(SAMPLE_LOG)],
                "SELECT * FROM log WHERE severity >= 'DEBUG'".to_string(),
            )
            .expect("open one pane")
        }

        #[test]
        fn tab_cycles_focus_across_panes() {
            let mut app = two_pane_app();
            assert_eq!(app.focused, 0);
            assert!(!app.handle_key(KeyCode::Tab).unwrap());
            assert_eq!(app.focused, 1);
            assert!(!app.handle_key(KeyCode::Tab).unwrap());
            assert_eq!(app.focused, 0);
        }

        #[test]
        fn closing_a_pane_does_not_affect_the_other() {
            let mut app = two_pane_app();
            let remaining_rows_before = app.panes[1].result.rows.len();

            // focused starts at 0 (SAMPLE_LOG), so closing it should leave
            // pane 1 (SAMPLE_LOG_2) behind, untouched.
            assert!(!app.handle_key(KeyCode::Char('x')).unwrap());
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
            assert!(app.handle_key(KeyCode::Char('x')).unwrap());
        }

        #[test]
        fn q_quits_regardless_of_pane_count() {
            let mut app = two_pane_app();
            assert!(app.handle_key(KeyCode::Char('q')).unwrap());
        }

        #[test]
        fn esc_quits_single_pane_but_not_multi_pane() {
            let mut multi = two_pane_app();
            assert!(!multi.handle_key(KeyCode::Esc).unwrap());

            let mut single = one_pane_app();
            assert!(single.handle_key(KeyCode::Esc).unwrap());
        }

        #[test]
        fn d_toggles_detail_pane() {
            let mut app = one_pane_app();
            assert!(!app.panes[0].detail_open);
            assert!(!app.handle_key(KeyCode::Char('d')).unwrap());
            assert!(app.panes[0].detail_open);
            assert!(!app.handle_key(KeyCode::Char('d')).unwrap());
            assert!(!app.panes[0].detail_open);
        }

        #[test]
        fn esc_closes_detail_pane_instead_of_quitting_single_pane_app() {
            let mut app = one_pane_app();
            app.handle_key(KeyCode::Char('d')).unwrap();
            assert!(app.panes[0].detail_open);

            // With detail open, Esc must close it, not quit the app -- even
            // though a single pane would normally quit on Esc.
            assert!(!app.handle_key(KeyCode::Esc).unwrap());
            assert!(!app.panes[0].detail_open);

            // Detail now closed: Esc goes back to its normal single-pane
            // quit behavior.
            assert!(app.handle_key(KeyCode::Esc).unwrap());
        }

        #[test]
        fn detail_lines_show_raw_and_every_other_column() {
            let app = one_pane_app();
            let pane = &app.panes[0];
            let lines = pane.detail_lines();

            assert!(
                lines[0].starts_with("raw: "),
                "raw line must come first: {lines:?}"
            );
            let raw_idx = pane.result.columns.iter().position(|c| c == "raw").unwrap();
            for (i, name) in pane.result.columns.iter().enumerate() {
                if i == raw_idx {
                    continue;
                }
                assert!(
                    lines.iter().any(|l| l.starts_with(&format!("{name}: "))),
                    "expected a line for column '{name}', got: {lines:?}"
                );
            }
        }

        #[test]
        fn detail_lines_follow_selection_across_j_k() {
            let mut app = one_pane_app();
            let before = app.panes[0].detail_lines();
            assert!(!app.handle_key(KeyCode::Char('k')).unwrap());
            let after = app.panes[0].detail_lines();
            assert_ne!(
                before, after,
                "moving the selection should change which row's detail is shown"
            );
        }

        #[test]
        fn navigation_key_reaches_focused_pane_only() {
            let mut app = two_pane_app();
            let other_selected = app.panes[1].list_state.selected();
            assert!(!app.handle_key(KeyCode::Char('k')).unwrap());
            assert_eq!(
                app.panes[1].list_state.selected(),
                other_selected,
                "unfocused pane's selection must not change"
            );
        }

        #[test]
        fn reverse_toggle_flips_render_order_and_jumps_to_latest() {
            let mut app = one_pane_app();
            let len = app.panes[0].result.rows.len();
            assert!(len > 1, "fixture needs multiple rows for this test");
            assert_eq!(app.panes[0].list_state.selected(), Some(len - 1));

            assert!(!app.handle_key(KeyCode::Char('R')).unwrap());
            let pane = &app.panes[0];
            assert!(pane.reverse);
            assert_eq!(
                pane.list_state.selected(),
                Some(0),
                "toggling reverse should jump the view to the latest row (index 0 in reverse mode)"
            );

            // Toggling back returns to non-reverse "latest is the last index".
            assert!(!app.handle_key(KeyCode::Char('R')).unwrap());
            let pane = &app.panes[0];
            assert!(!pane.reverse);
            assert_eq!(pane.list_state.selected(), Some(len - 1));
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
                assert!(!app.handle_key(KeyCode::Char(c)).unwrap());
            }
        }

        #[test]
        fn highlight_expression_compiles_and_marks_matching_rows() {
            let mut app = one_pane_app();
            assert!(!app.handle_key(KeyCode::Char('?')).unwrap());
            type_str(&mut app, "severity >= ERR");
            assert!(!app.handle_key(KeyCode::Enter).unwrap());

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
            app.handle_key(KeyCode::Char('?')).unwrap();
            type_str(&mut app, "severity >= ERR");
            app.handle_key(KeyCode::Enter).unwrap();

            assert!(!app.handle_key(KeyCode::Char('h')).unwrap());
            let pane = &app.panes[0];
            assert!(!pane.highlight_enabled);
            assert_eq!(pane.highlight_text, "severity >= ERR");
            assert!(
                pane.highlight.is_some(),
                "compiled predicate is retained, not cleared"
            );

            assert!(!app.handle_key(KeyCode::Char('h')).unwrap());
            assert!(app.panes[0].highlight_enabled);
        }

        #[test]
        fn highlight_toggle_is_a_no_op_with_no_expression_set() {
            let mut app = one_pane_app();
            assert!(!app.handle_key(KeyCode::Char('h')).unwrap());
            assert!(!app.panes[0].highlight_enabled);
        }

        #[test]
        fn invalid_highlight_expression_reports_an_error_without_crashing() {
            let mut app = one_pane_app();
            app.handle_key(KeyCode::Char('?')).unwrap();
            type_str(&mut app, "not a valid expression at all");
            assert!(!app.handle_key(KeyCode::Enter).unwrap());

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

            app.handle_key(KeyCode::Char('?')).unwrap();
            type_str(&mut app, "not a valid expression at all");
            app.handle_key(KeyCode::Enter).unwrap();

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
            app.handle_key(KeyCode::Char('?')).unwrap();
            type_str(&mut app, "severity >= ERR");
            app.handle_key(KeyCode::Enter).unwrap();
            assert!(app.panes[0].highlight.is_some());

            // Now overwrite with a broken expression -- the stale compiled
            // predicate from the previous valid one must not linger.
            app.handle_key(KeyCode::Char('?')).unwrap();
            for _ in 0.."severity >= ERR".len() {
                app.handle_key(KeyCode::Backspace).unwrap();
            }
            type_str(&mut app, "not valid");
            app.handle_key(KeyCode::Enter).unwrap();

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
            app.handle_key(KeyCode::Char('?')).unwrap();
            type_str(&mut app, "severity >= ERR");
            assert!(!app.handle_key(KeyCode::Esc).unwrap());

            let pane = &app.panes[0];
            assert!(!pane.editing_highlight);
            assert!(
                pane.highlight.is_none(),
                "Esc must not apply the typed expression"
            );
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
