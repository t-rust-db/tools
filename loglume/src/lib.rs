//! loglume: Fast CLI log viewer with SQL filtering.
//!
//! Built on `db-core`'s streaming storage layer.

#![deny(unsafe_code)]
#![warn(missing_docs)]

pub use db_core::storage::{
    Facility, FieldColumn, FieldStore, LogBatch, Resource, Severity, Source, SourceKind,
    SyslogParser,
};

/// Table directories (tools#15): db-core's snapshot reader, its query
/// entry point and the manifest's file name.
pub use db_core::engine::column::table::{query_snapshot, TableEngine};
pub use db_core::engine::stream::{StandingQuery, StandingQueryEvent, StreamEngine};
pub use db_core::engine::{
    Cell, CompiledPredicate, Engine, EngineError, ErrorKind, QueryResult, ScopeReport,
};
pub use db_core::parser::ast::BinaryOp;
pub use db_core::storage::ingest::MANIFEST_FILE;
pub use db_core::vm::stream::EmitMode;
