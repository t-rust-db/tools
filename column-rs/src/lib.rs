pub mod query;

/// Re-exports preserving column-rs's pre-extraction `column_rs::vm::*`
/// surface — the actual implementation now lives in `db-core`'s
/// `BatchExecutor` (`db_core::vm::batch`), alongside `vm::row`/`vm::stream`
/// stubs for the other two executors that column-rs doesn't use. Emitted
/// binaries (`column-rs codegen`, via `db_core::codegen::batch::emit`) import
/// `column_rs::vm::{AggPart, MapOp, Opcode, Value}` from here.
pub mod vm {
    pub use db_core::vm::batch::*;
}

/// Re-exports preserving column-rs's pre-extraction `column_rs::file::*`
/// surface — the actual implementation now lives in `db-storage`'s
/// `column::parquet` module (folded in from the standalone `db-parquet`
/// repo, `db-storage#4`).
pub mod file {
    pub use db_core::storage::{DictionaryIndices, FileError, ParquetFile, RowGroupReader};
}
pub use db_core::storage::column::parquet::nested;
pub use db_core::storage::column::parquet::reader;

/// Re-exports preserving column-rs's pre-extraction `column_rs::sql::*`
/// surface — the actual implementation now lives in `db-core`'s
/// `types`/`parser::ast`/`parser` modules (the AST is `parser::ast::Select`
/// since db-core#153 retired `expr::Query`).
pub mod sql {
    // The whole AST is re-exported: `db_core::codegen::batch::emit`'s generated
    // binaries reconstruct a parsed `Select` literally (`use
    // column_rs::sql::{BinaryOp, Distinctness, Expr, ..., WindowDef}`).
    pub use db_core::codegen::batch::{WindowFunc, WindowSpec};
    pub use db_core::parser::ast::*;
    pub use db_core::parser::{parse, parse_explain, Explain, ParseError, Result, Span};
    pub use db_core::vm::batch::AggFunc;
}
