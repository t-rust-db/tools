pub mod codegen;
pub mod query;
pub mod vm;

/// Re-exports preserving column-rs's pre-extraction `column_rs::file::*`
/// surface — the actual implementation now lives in `db-parquet`.
pub mod file {
    pub use db_parquet::{DictionaryIndices, FileError, ParquetFile, RowGroupReader};
}
pub use db_parquet::nested;
pub use db_parquet::reader;

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
