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
use db_core::engine::{Engine, EngineError, ErrorKind, FileStats, Mode, OpcodeSection};
use db_core::engine::{PlanRow, QueryResult, TableInfo};

/// One open file's engine, in whichever mode its extension implied.
///
/// A concrete enum, not `Box<dyn Engine>` (which this replaced,
/// db-studio#54): db-core's cross-mode resolver
/// (`db_core::engine::resolve::run_query`/`explain_plan`) needs concrete
/// `&StreamEngine`/`&RowEngine` references, not a trait object, to join a
/// `.log` file's stream table to a `.sqlite` file's lookup table -- a
/// trait object has thrown away exactly the type information that join
/// needs to recover. Matching on the enum's variants recovers it directly,
/// with no `Any`/downcasting and no change to db-core's `Engine` trait.
///
/// Carries inherent methods mirroring the six `Engine` methods db-studio
/// actually calls (`tables`, `stats`, `mode`, `run_query`, `explain_plan`,
/// `explain_opcodes`) so every existing call site keeps working unchanged
/// -- it does not implement `Engine` itself, since `Engine::open` doesn't
/// fit an enum's per-extension-dispatch open logic (there's no single
/// path to open an `EngineHandle` in a particular variant without already
/// knowing which one).
pub enum EngineHandle {
    Row(RowEngine),
    Batch(BatchEngine),
    Stream(StreamEngine),
}

impl EngineHandle {
    pub fn mode(&self) -> Mode {
        match self {
            Self::Row(e) => e.mode(),
            Self::Batch(e) => e.mode(),
            Self::Stream(e) => e.mode(),
        }
    }

    pub fn run_query(&mut self, sql: &str) -> Result<QueryResult, EngineError> {
        match self {
            Self::Row(e) => e.run_query(sql),
            Self::Batch(e) => e.run_query(sql),
            Self::Stream(e) => e.run_query(sql),
        }
    }

    pub fn explain_plan(&self, sql: &str) -> Result<Vec<PlanRow>, EngineError> {
        match self {
            Self::Row(e) => e.explain_plan(sql),
            Self::Batch(e) => e.explain_plan(sql),
            Self::Stream(e) => e.explain_plan(sql),
        }
    }

    pub fn explain_opcodes(&self, sql: &str) -> Result<Vec<OpcodeSection>, EngineError> {
        match self {
            Self::Row(e) => e.explain_opcodes(sql),
            Self::Batch(e) => e.explain_opcodes(sql),
            Self::Stream(e) => e.explain_opcodes(sql),
        }
    }

    pub fn stats(&self) -> FileStats {
        match self {
            Self::Row(e) => e.stats(),
            Self::Batch(e) => e.stats(),
            Self::Stream(e) => e.stats(),
        }
    }

    pub fn tables(&self) -> Result<Vec<TableInfo>, EngineError> {
        match self {
            Self::Row(e) => e.tables(),
            Self::Batch(e) => e.tables(),
            Self::Stream(e) => e.tables(),
        }
    }

    /// The concrete `&RowEngine`, if this file is open in row mode --
    /// db-studio#54's cross-mode join needs this to find the lookup
    /// side by table name among the other currently-open files.
    pub fn as_row(&self) -> Option<&RowEngine> {
        match self {
            Self::Row(e) => Some(e),
            _ => None,
        }
    }

    /// The concrete `&StreamEngine`, if this file is open in stream mode
    /// -- the driving side of a cross-mode join (db-studio#54).
    pub fn as_stream(&self) -> Option<&StreamEngine> {
        match self {
            Self::Stream(e) => Some(e),
            _ => None,
        }
    }
}

/// Opens `path` through whichever `Engine` its extension implies.
/// Case-insensitive (`.SQLITE`, `.Parquet`, `.LOG`, ... all match) --
/// file extensions aren't a place users expect case to matter.
pub fn open_by_extension(path: &Path) -> Result<EngineHandle, EngineError> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    match ext.as_deref() {
        Some("sqlite" | "db") => RowEngine::open(path).map(EngineHandle::Row),
        Some("parquet") => BatchEngine::open(path).map(EngineHandle::Batch),
        Some("log") => StreamEngine::open(path).map(EngineHandle::Stream),
        other => Err(EngineError::new(
            ErrorKind::Open,
            format!(
                "unrecognized file extension {:?} -- expected .sqlite/.db (row mode), .parquet (batch mode), or .log (stream mode)",
                other.unwrap_or("<none>")
            ),
        )),
    }
}
