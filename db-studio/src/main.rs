//! `db-studio`: a TUI database studio for the t-rust-db family. See
//! `.openspec/plan.md` for scope and phasing. M1 is `.sqlite` only, one
//! file per invocation -- `db-studio path/to.sqlite`.

mod app;
mod completion;
mod error_pane;
mod grid_pane;
mod highlight;
mod query_pane;
mod schema_tree;
mod terminal;
mod theme;

use std::path::Path;
use std::process::ExitCode;

use db_core::engine::row::RowEngine;
use db_core::engine::Engine;

fn main() -> ExitCode {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: db-studio <path.sqlite>");
        return ExitCode::FAILURE;
    };
    // Opened before the terminal is touched: a bad path is a plain stderr
    // message in the user's shell, not something buried in the error pane
    // of a TUI that then has nothing to show.
    let engine = match RowEngine::open(Path::new(&path)) {
        Ok(engine) => engine,
        Err(err) => {
            eprintln!("db-studio: {err}");
            return ExitCode::FAILURE;
        }
    };

    let result = (|| -> std::io::Result<()> {
        let mut term = terminal::init()?;
        let run_result = app::App::new(Box::new(engine)).run(&mut term);
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
