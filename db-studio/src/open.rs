//! Dispatches a file path to whichever `Engine` its extension implies
//! (`.sqlite`/`.db` -> row, `.parquet` -> batch, `.log` -> stream).
//! Shared by `main.rs` (files given on the command line) and `app.rs`
//! (db-studio#42's `Ctrl+O` prompt to open a file mid-session) --
//! previously private to `main.rs`, duplicated logic would have
//! drifted the moment one of the two gained a fourth extension.

use std::path::Path;

use db_core::engine::column::BatchEngine;
use db_core::engine::row::RowEngine;
use db_core::engine::stream::StreamEngine;
use db_core::engine::{Engine, EngineError, ErrorKind};

/// Opens `path` through whichever `Engine` its extension implies.
/// Case-insensitive (`.SQLITE`, `.Parquet`, `.LOG`, ... all match) --
/// file extensions aren't a place users expect case to matter.
pub fn open_by_extension(path: &Path) -> Result<Box<dyn Engine>, EngineError> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    match ext.as_deref() {
        Some("sqlite" | "db") => RowEngine::open(path).map(|e| Box::new(e) as Box<dyn Engine>),
        Some("parquet") => BatchEngine::open(path).map(|e| Box::new(e) as Box<dyn Engine>),
        Some("log") => StreamEngine::open(path).map(|e| Box::new(e) as Box<dyn Engine>),
        other => Err(EngineError::new(
            ErrorKind::Open,
            format!(
                "unrecognized file extension {:?} -- expected .sqlite/.db (row mode), .parquet (batch mode), or .log (stream mode)",
                other.unwrap_or("<none>")
            ),
        )),
    }
}
