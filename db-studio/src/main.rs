//! `db-studio`: a TUI database studio for the t-rust-db family. See
//! `.openspec/plan.md` for scope and phasing. M2 is `.sqlite` only, one
//! or more files per invocation -- `db-studio path/to.sqlite [more...]`.

mod app;
mod completion;
mod error_pane;
mod grid_pane;
mod highlight;
mod query_pane;
mod schema_tree;
mod terminal;
mod theme;

use std::path::PathBuf;
use std::process::ExitCode;

use app::OpenFile;
use db_core::engine::row::RowEngine;
use db_core::engine::Engine;

fn main() -> ExitCode {
    let paths: Vec<String> = std::env::args().skip(1).collect();
    if paths.is_empty() {
        eprintln!("usage: db-studio <path.sqlite> [more.sqlite ...]");
        return ExitCode::FAILURE;
    }

    // Every file opened before the terminal is touched: a bad path is a
    // plain stderr message in the user's shell, not something buried in
    // the error pane of a TUI that then has nothing to show. One bad
    // path fails the whole invocation -- no partial "some files open,
    // one silently didn't" (db-studio#16).
    let mut files = Vec::with_capacity(paths.len());
    for path in paths {
        match RowEngine::open(&PathBuf::from(&path)) {
            Ok(engine) => files.push(OpenFile {
                path: PathBuf::from(path),
                engine: Box::new(engine),
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
