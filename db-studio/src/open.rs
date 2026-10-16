//! Dispatches a file path to whichever `Engine` its extension implies
//! (`.sqlite`/`.db` -> row, `.parquet` -> batch, `.log` -> stream), or a
//! table directory (one holding `manifest.json`, written by db-core's
//! ingest or `column-ingest`) to db-core's `TableEngine`, in batch mode.
//! Shared by `main.rs` (files given on the command line) and `app.rs`
//! (db-studio#42's `Ctrl+O` prompt to open a file mid-session) --
//! previously private to `main.rs`, duplicated logic would have
//! drifted the moment one of the two gained a fourth extension.

use std::path::Path;

use db_core::storage::ingest::MANIFEST_FILE;

use db_core::engine::column::table::TableEngine;
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
    /// A table directory: its Parquet files and WAL pages as one batch
    /// table, read from a fresh snapshot on every query.
    Table(TableEngine),
}

impl EngineHandle {
    pub fn mode(&self) -> Mode {
        match self {
            Self::Row(e) => e.mode(),
            Self::Batch(e) => e.mode(),
            Self::Stream(e) => e.mode(),
            Self::Table(e) => e.mode(),
        }
    }

    pub fn run_query(&mut self, sql: &str) -> Result<QueryResult, EngineError> {
        match self {
            Self::Row(e) => e.run_query(sql),
            Self::Batch(e) => e.run_query(sql),
            Self::Stream(e) => e.run_query(sql),
            Self::Table(e) => e.run_query(sql),
        }
    }

    pub fn explain_plan(&self, sql: &str) -> Result<Vec<PlanRow>, EngineError> {
        match self {
            Self::Row(e) => e.explain_plan(sql),
            Self::Batch(e) => e.explain_plan(sql),
            Self::Stream(e) => e.explain_plan(sql),
            Self::Table(e) => e.explain_plan(sql),
        }
    }

    pub fn explain_opcodes(&self, sql: &str) -> Result<Vec<OpcodeSection>, EngineError> {
        match self {
            Self::Row(e) => e.explain_opcodes(sql),
            Self::Batch(e) => e.explain_opcodes(sql),
            Self::Stream(e) => e.explain_opcodes(sql),
            Self::Table(e) => e.explain_opcodes(sql),
        }
    }

    pub fn stats(&self) -> FileStats {
        match self {
            Self::Row(e) => e.stats(),
            Self::Batch(e) => e.stats(),
            Self::Stream(e) => e.stats(),
            Self::Table(e) => e.stats(),
        }
    }

    pub fn tables(&self) -> Result<Vec<TableInfo>, EngineError> {
        match self {
            Self::Row(e) => e.tables(),
            Self::Batch(e) => e.tables(),
            Self::Stream(e) => e.tables(),
            Self::Table(e) => e.tables(),
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

/// Opens `path` through whichever `Engine` its extension implies, or as a
/// table directory if it is a directory holding `manifest.json`.
/// Case-insensitive (`.SQLITE`, `.Parquet`, `.LOG`, ... all match) --
/// file extensions aren't a place users expect case to matter.
pub fn open_by_extension(path: &Path) -> Result<EngineHandle, EngineError> {
    if path.is_dir() && path.join(MANIFEST_FILE).is_file() {
        return TableEngine::open(path).map(EngineHandle::Table);
    }
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
                "unrecognized file extension {:?} -- expected .sqlite/.db (row mode), .parquet (batch mode), .log (stream mode), or a table directory with a manifest.json (batch mode)",
                other.unwrap_or("<none>")
            ),
        )),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]

    use super::*;
    use db_core::engine::Cell;
    use db_core::storage::ingest::{Ack, Table, TableOptions};
    use std::path::PathBuf;

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("db-studio-open-{}-{name}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_table_directory_opens_in_batch_mode_and_sees_new_records() {
        let dir = temp_dir("table").join("events");
        let table = Table::open(&dir, TableOptions::default()).unwrap();
        table
            .append_batch(&[r#"{"n":1}"#, r#"{"n":2}"#], Ack::Durable)
            .unwrap();
        table.flush().unwrap();

        let mut handle = open_by_extension(&dir).unwrap();
        assert!(matches!(handle, EngineHandle::Table(_)));
        assert_eq!(handle.mode(), Mode::Batch);
        let names: Vec<String> = handle
            .tables()
            .unwrap()
            .into_iter()
            .map(|t| t.name)
            .collect();
        assert_eq!(names, ["events"], "named after the directory");
        let count = |h: &mut EngineHandle| h.run_query("SELECT COUNT(*) FROM events").unwrap().rows;
        assert_eq!(count(&mut handle), [vec![Cell::Int(2)]]);

        // A record appended (not yet flushed) shows up on the next query.
        table.append(br#"{"n":3}"#, Ack::Durable).unwrap();
        assert_eq!(count(&mut handle), [vec![Cell::Int(3)]]);
        table.close().unwrap();
        std::fs::remove_dir_all(dir.parent().unwrap()).ok();
    }

    #[test]
    fn a_directory_without_a_manifest_is_refused() {
        let dir = temp_dir("plain");
        let err = open_by_extension(&dir).err().unwrap();
        assert_eq!(err.kind, ErrorKind::Open);
        assert!(err.to_string().contains("table directory"), "{err}");
        std::fs::remove_dir_all(&dir).ok();
    }
}
