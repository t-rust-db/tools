//! `db-studio`: a TUI database studio for the t-rust-db family. See
//! `.openspec/plan.md` for scope and phasing. M3 opens `.sqlite` (row
//! mode) and `.parquet` (batch mode) files; M5 (db-studio#35) adds
//! `.log` (stream mode) -- one or more per invocation, `db-studio
//! a.sqlite b.parquet c.log [more...]`, dispatching each by extension
//! to the matching `Engine`.

mod app;
mod clipboard;
mod completion;
mod error_pane;
mod grid_pane;
mod highlight;
mod history;
mod opcode_pane;
mod open;
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
use open::open_by_extension;

fn main() -> ExitCode {
    let paths: Vec<String> = std::env::args().skip(1).collect();
    if paths.is_empty() {
        eprintln!("usage: db-studio <path.sqlite|path.parquet|path.log> [more ...]");
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
        let run_result = app::App::new(files)
            .with_history(history::load())
            .run(&mut term);
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
