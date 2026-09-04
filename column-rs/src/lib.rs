pub mod codegen;
pub mod query;

/// Re-exports preserving column-rs's pre-extraction `column_rs::vm::*`
/// surface — the actual implementation now lives in `sql-vm`'s
/// `BatchExecutor` (`sql_vm::batch`), alongside `sql_vm::row`/`sql_vm::stream`
/// stubs for the other two executors that column-rs doesn't use.
pub mod vm {
    pub use sql_vm::batch::*;
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
/// surface — the actual implementation now lives in `sql-types`/`sql-expr`/
/// `sql-parser`.
pub mod sql {
    pub use sql_expr::{
        AggFunc, BinOp, Expr, Join, JoinKind, OrderBy, Query, SelectItem, WindowFunc, WindowSpec,
    };
    pub use sql_parser::{parse, parse_explain, ParseError, Result};
    pub use sql_types::Literal;
}
