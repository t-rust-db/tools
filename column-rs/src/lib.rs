pub mod query;

/// Re-exports preserving column-rs's pre-extraction `column_rs::vm::*`
/// surface — the actual implementation now lives in `db-core`'s
/// `BatchExecutor` (`db_core::vm::batch`), alongside `vm::row`/`vm::stream`
/// stubs for the other two executors that column-rs doesn't use. Emitted
/// binaries (`column-rs codegen`, via `db_core::emit::batch`) import
/// `column_rs::vm::{AggPart, MapOp, Opcode, Value}` from here.
pub mod vm {
    pub use db_core::vm::batch::*;
}

/// Re-exports preserving column-rs's pre-extraction `column_rs::file::*`
/// surface — the actual implementation now lives in `db-storage`'s
/// `column::parquet` module (folded in from the standalone `db-parquet`
/// repo, `db-storage#4`).
pub mod file {
    pub use db_storage::{DictionaryIndices, FileError, ParquetFile, RowGroupReader};
}
pub use db_storage::column::parquet::nested;
pub use db_storage::column::parquet::reader;

/// Re-exports preserving column-rs's pre-extraction `column_rs::sql::*`
/// surface — the actual implementation now lives in `db-core`'s
/// `types`/`expr`/`parser` modules.
pub mod sql {
    pub use db_core::expr::{
        AggFunc, BinOp, Expr, Join, JoinKind, OrderBy, Query, SelectItem, WindowFunc, WindowSpec,
    };
    pub use db_core::parser::{parse, parse_explain, ParseError, Result};
    pub use db_core::types::Literal;
}
