// column-rs CLI (#89): REPL and one-shot query mode for Parquet files.

mod handler;
mod stream_output;

use std::io::{self, BufWriter, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use column_rs::query::{QueryEngine, QueryError};
use db_cli::{history_path, run_repl, OutputMode, ReplOptions};
use handler::{ColumnHandler, Output};
use stream_output::{print_chunk, print_result_streaming};

const USAGE: &str = "[--version] [--help] [-c \"<SQL>\"] <file.parquet> [more.parquet ...]";

fn usage_error(expected: &str) -> ExitCode {
    eprintln!("usage: column-rs {expected}");
    ExitCode::FAILURE
}

/// `--help`/`-h`: the top-level usage line, requested rather than
/// provoked -- stdout, exit 0 (what `make smoke` checks).
fn usage() -> ExitCode {
    println!("usage: column-rs {USAGE}");
    ExitCode::SUCCESS
}

fn open_engine(paths: &[PathBuf]) -> Result<QueryEngine, ExitCode> {
    QueryEngine::open_many(paths).map_err(|e| {
        eprintln!("error: {e}");
        ExitCode::FAILURE
    })
}

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("--version" | "-V") => {
            println!("column-rs {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Some("--help" | "-h") => usage(),
        Some("-c") => {
            let Some(sql) = args.next() else {
                return usage_error("-c \"<SQL>\" <file.parquet> [more.parquet ...]");
            };
            let paths: Vec<PathBuf> = args.map(PathBuf::from).collect();
            if paths.is_empty() {
                return usage_error("-c \"<SQL>\" <file.parquet> [more.parquet ...]");
            }
            run_query(&paths, &sql)
        }
        Some("codegen") => {
            let Some(sql) = args.next() else {
                return usage_error("codegen \"<SQL>\" --out <file.rs>");
            };
            let (mut out_path, mut rest_ok) = (None, true);
            match (args.next().as_deref(), args.next()) {
                (Some("--out"), Some(path)) => out_path = Some(PathBuf::from(path)),
                (None, None) => {}
                _ => rest_ok = false,
            }
            let (Some(out_path), true) = (out_path, rest_ok) else {
                return usage_error("codegen \"<SQL>\" --out <file.rs>");
            };
            codegen_command(&sql, &out_path)
        }
        Some(first) => {
            let mut paths: Vec<PathBuf> = vec![PathBuf::from(first)];
            paths.extend(args.map(PathBuf::from));
            run_column_repl(&paths)
        }
        None => usage_error(USAGE),
    }
}

/// `-c "<SQL>" <file...>`: run a single query (against one or more loaded
/// tables) and print the result, without entering the REPL.
fn run_query(paths: &[PathBuf], sql: &str) -> ExitCode {
    let engine = match open_engine(paths) {
        Ok(e) => e,
        Err(code) => return code,
    };
    // db-core#456: try the streaming path first -- it prints as it goes,
    // instead of collecting the whole result before the first line. It
    // only covers a plain single-table SELECT with no aggregate,
    // DISTINCT, ORDER BY, or LIMIT (see `QueryEngine::execute_streaming`);
    // anything else, including EXPLAIN, falls through to the existing
    // collect-then-print path below unchanged.
    match try_stream_query(&engine, sql) {
        Ok(true) => return ExitCode::SUCCESS,
        Ok(false) => {}
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    }
    let mut handler = ColumnHandler { engine };
    match db_cli::ReplHandler::execute(&mut handler, sql) {
        // #110: one-shot mode streams tab-separated output row-by-row
        // rather than going through `db_cli::render`'s box-drawing table,
        // which needs the whole result collected up front just to compute
        // column widths -- exactly backwards for a large, low-selectivity
        // query where materializing every cell as a padded, bordered
        // string is most of the wall-clock and peak memory.
        Ok(Output::Rows(result)) => {
            print_result_streaming(&result);
            ExitCode::SUCCESS
        }
        Ok(output @ (Output::Plan(_) | Output::Opcodes(_))) => {
            println!(
                "{}",
                // headers is irrelevant here -- Plan/Opcodes ignore it (only
                // Output::Rows, handled above via streaming, respects it).
                db_cli::ReplHandler::format(&handler, &output, OutputMode::Table, true)
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Attempts `sql` via [`QueryEngine::execute_streaming`] (db-core#456).
/// `Ok(true)`: streamed and printed, caller is done. `Ok(false)`: not a
/// streamable shape (`EXPLAIN`, a parse error, a `JOIN`/semi-join/window
/// query, or an aggregate/`DISTINCT`/`ORDER BY`/`LIMIT`) -- caller falls
/// back to the existing collect-then-print path, which reports whatever
/// the real error turns out to be if there is one. `Err`: a genuine query
/// error surfaced *during* streaming, after some rows may already have
/// been printed -- there is no whole result to discard and retry with.
fn try_stream_query(engine: &QueryEngine, sql: &str) -> Result<bool, QueryError> {
    // `EXPLAIN ...` isn't ordinary SQL to `db_core::parser::parse`, so
    // strip it first (matching `handler::ColumnHandler::execute`) and
    // decline to stream it here -- the fallback path already renders
    // Plan/Opcodes output correctly.
    match column_rs::sql::parse_explain(sql) {
        Ok((db_core::parser::Explain::None, _)) => {}
        _ => return Ok(false),
    }
    let stdout = io::stdout();
    let mut out = BufWriter::with_capacity(256 * 1024, stdout.lock());
    let mut streamed_header = false;
    let result = engine.execute_streaming(sql, |event| match event {
        column_rs::query::StreamEvent::Columns(columns) => {
            let _ = writeln!(out, "{}", columns.join("\t"));
            streamed_header = true;
        }
        column_rs::query::StreamEvent::Chunk(chunk) => print_chunk(&mut out, &chunk),
    });
    match result {
        Ok(()) => Ok(true),
        // Not this query's fault -- the shape just can't stream. Nothing
        // was printed yet in either case: `on_columns` (and therefore any
        // row) only runs after every up-front check has already passed.
        Err(
            QueryError::NotStreamable(_)
            | QueryError::Vm(db_core::vm::batch::VmError::NotStreamable { .. }),
        ) if !streamed_header => Ok(false),
        Err(e) => Err(e),
    }
}

/// Interactive REPL, via `db_cli::run_repl`.
fn run_column_repl(paths: &[PathBuf]) -> ExitCode {
    let engine = match open_engine(paths) {
        Ok(e) => e,
        Err(code) => return code,
    };
    let handler = ColumnHandler { engine };
    let history_file = history_path("column-rs");
    let result = run_repl(
        handler,
        ReplOptions {
            prompt: "column> ",
            continuation_prompt: "     -> ",
            history_file: history_file.as_deref(),
        },
    );
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// `column-rs codegen "<SQL>" --out <file.rs>` (#98): compile `sql` ahead
/// of time into a standalone `.rs` source file embedding the VM program as
/// a `const`, instead of running it. The subcommand keeps its historical
/// name; the implementation is db-core's AOT *emitter*
/// (`db_core::codegen::batch::emit`, ADR 0007 there), pointed at this crate's
/// runtime glue (`column_rs::query::run_program` etc.).
fn codegen_command(sql: &str, out_path: &std::path::Path) -> ExitCode {
    match db_core::codegen::batch::emit::generate("column_rs", sql) {
        Ok(src) => match std::fs::write(out_path, src) {
            Ok(()) => {
                println!("wrote {}", out_path.display());
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("error: {}: {e}", out_path.display());
                ExitCode::FAILURE
            }
        },
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
