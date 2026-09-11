//! `db-studio`: a TUI database studio for the t-rust-db family. See
//! `.openspec/plan.md` for scope and phasing. M3 opens `.sqlite` (row
//! mode) and `.parquet` (batch mode) files, one or more per invocation --
//! `db-studio a.sqlite b.parquet [more...]`, dispatching each by
//! extension to the matching `Engine`.

mod app;
mod completion;
mod error_pane;
mod grid_pane;
mod highlight;
mod opcode_pane;
mod plan_pane;
mod query_pane;
mod schema_tree;
mod stats_pane;
mod status_bar;
mod terminal;
mod theme;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use app::OpenFile;
use db_core::engine::column::BatchEngine;
use db_core::engine::row::RowEngine;
use db_core::engine::{Engine, EngineError, ErrorKind};

/// Opens `path` through whichever `Engine` its extension implies.
/// Case-insensitive (`.SQLITE`, `.Parquet`, ... all match) -- file
/// extensions aren't a place users expect case to matter.
fn open_by_extension(path: &Path) -> Result<Box<dyn Engine>, EngineError> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    match ext.as_deref() {
        Some("sqlite" | "db") => RowEngine::open(path).map(|e| Box::new(e) as Box<dyn Engine>),
        Some("parquet") => BatchEngine::open(path).map(|e| Box::new(e) as Box<dyn Engine>),
        other => Err(EngineError::new(
            ErrorKind::Open,
            format!(
                "unrecognized file extension {:?} -- expected .sqlite/.db (row mode) or .parquet (batch mode)",
                other.unwrap_or("<none>")
            ),
        )),
    }
}

fn main() -> ExitCode {
    let paths: Vec<String> = std::env::args().skip(1).collect();
    if paths.is_empty() {
        eprintln!("usage: db-studio <path.sqlite|path.parquet> [more ...]");
        return ExitCode::FAILURE;
    }

    // Every file opened before the terminal is touched: a bad path is a
    // plain stderr message in the user's shell, not something buried in
    // the error pane of a TUI that then has nothing to show. One bad
    // path fails the whole invocation -- no partial "some files open,
    // one silently didn't" (db-studio#16).
    let mut files = Vec::with_capacity(paths.len());
    for path in paths {
        match open_by_extension(Path::new(&path)) {
            Ok(engine) => files.push(OpenFile {
                path: PathBuf::from(path),
                engine,
            }),
            Err(err) => {
                eprintln!("db-studio: {path}: {err}");
                return ExitCode::FAILURE;
            }
        }
    }

    let result = (|| -> std::io::Result<()> {
        let mut term = terminal::init()?;
        let run_result = app::App::new(files).run(&mut term);
        terminal::restore()?;
        run_result
    })();

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("db-studio: {err}");
            ExitCode::FAILURE
        }
    }
}
