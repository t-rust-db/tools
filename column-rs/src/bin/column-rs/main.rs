// column-rs CLI (#89): REPL and one-shot query mode for Parquet files.

mod handler;
mod stream_output;

use std::path::PathBuf;
use std::process::ExitCode;

use column_rs::query::QueryEngine;
use db_cli::{history_path, run_repl, OutputMode, ReplOptions};
use handler::{ColumnHandler, Output};
use stream_output::print_rows_streaming;

fn usage_error(expected: &str) -> ExitCode {
    eprintln!("usage: column-rs {expected}");
    ExitCode::FAILURE
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
        None => usage_error("[--version] [-c \"<SQL>\"] <file.parquet> [more.parquet ...]"),
    }
}

/// `-c "<SQL>" <file...>`: run a single query (against one or more loaded
/// tables) and print the result, without entering the REPL.
fn run_query(paths: &[PathBuf], sql: &str) -> ExitCode {
    let engine = match open_engine(paths) {
        Ok(e) => e,
        Err(code) => return code,
    };
    let mut handler = ColumnHandler { engine };
    match db_cli::ReplHandler::execute(&mut handler, sql) {
        // #110: one-shot mode streams tab-separated output row-by-row
        // rather than going through `db_cli::render`'s box-drawing table,
        // which needs the whole result collected up front just to compute
        // column widths -- exactly backwards for a large, low-selectivity
        // query where materializing every cell as a padded, bordered
        // string is most of the wall-clock and peak memory.
        Ok(Output::Rows(result)) => {
            print_rows_streaming(&result.columns, result.rows.into_iter());
            ExitCode::SUCCESS
        }
        Ok(output @ Output::Plan(_)) => {
            println!(
                "{}",
                db_cli::ReplHandler::format(&handler, &output, OutputMode::Table)
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
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
/// a `const`, instead of running it.
fn codegen_command(sql: &str, out_path: &std::path::Path) -> ExitCode {
    match column_rs::codegen::generate(sql) {
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
