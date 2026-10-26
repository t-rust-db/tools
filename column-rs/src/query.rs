//! Parquet glue between `db_core::storage`'s `column::parquet` module and
//! db-core's planner ([`db_core::codegen::batch`]) and cross-segment
//! engine ([`db_core::vm::engine`]): resolve a program's column names
//! against a file's leaf schema, expose each row group as a
//! [`Segment`], materialize whole tables for the join/semi-join/window
//! paths, and dispatch a parsed query to the right shape.
//!
//! A table is a Parquet file or a table directory (one holding
//! `manifest.json`, written by db-core's ingest or `column-ingest`; tools#13).
//! A directory is read as a fresh db-core `Snapshot` per query: its files'
//! row groups and its WAL pages, in LSN order, through db-core's
//! `snapshot_segments` (which NULL-fills a column a file lacks and casts
//! one a file stores with another type) and `read_snapshot` (the whole
//! table, for join build sides, semi-joins and windows).
//!
//! Nothing here plans or post-processes anything -- `compile()`,
//! `AggPart`, `post_process` (now the `Opcode::Combine`/`Sort`/`Limit` tail applied by
//! `db_core::vm::engine::run`) and the `EXPLAIN` tree all moved to db-core
//! (ADR 0007 there), because none of them ever touched `ParquetFile`.
//! What stays is exactly what does.

use crate::vm::{Batch, Opcode, Program, Segment, Value};
use db_core::codegen::batch::{self as planner, PlanError, TableStats};
use db_core::engine::column::table::read_snapshot;
use db_core::engine::column::{
    file_segments, snapshot_files, snapshot_segments, FileSegment, SnapshotSegment,
};
use db_core::engine::resolve::CrossModeEngine;
use db_core::engine::row::RowEngine;
use db_core::engine::{Cell, Engine, EngineError, ErrorKind};
use db_core::parser::ast::{ExplainBody, Expr, ExprKind, Join, JoinOp, ResultColumn, Select};
use db_core::parser::row::ParseOutcome;
use db_core::parser::Explain;
use db_core::storage::column::parquet::footer::PhysicalType;
use db_core::storage::column::{decode_column_full, Decoded};
use db_core::storage::ingest::{Snapshot, MANIFEST_FILE};
use db_core::storage::{ParquetFile, Vfs, VfsFile};
use db_core::vm::batch::{Chunk, QueryOutput, ScanSource, VmError};
use db_core::vm::engine::{self, InMemorySegment, NoResolver};
use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

pub use db_core::codegen::batch::{OpcodeRow, OpcodeSection, PlanNode};

#[derive(Debug)]
pub enum QueryError {
    UnknownColumn(String),
    UnknownTable(String),
    DuplicateTable(String),
    UnsupportedSemiJoin(String),
    /// A `SELECT`-list item the batch planner cannot compile.
    UnsupportedSelectItem(String),
    /// The join planner was handed a `SELECT` without a `JOIN` (db-core#231).
    NoJoinClause,
    /// `Right`/`Full`/`Cross` are parseable (`db_core::parser::ast::JoinOp`) but
    /// only `Inner`/`Left` hash-join execution exists so far.
    UnsupportedJoinKind(JoinOp),
    /// More than one `JOIN` between Parquet tables (tools#71): the batch
    /// planner joins exactly two tables.
    TooManyJoins(usize),
    /// `SELECT *` (or a mixed `SELECT col, *`) combined with `GROUP BY`, an
    /// aggregate, or a window function.
    StarWithAggregation,
    /// The batch planner hit one of its own invariants (db-core#232's
    /// `PlanError::Internal`): a planner bug, not a user error.
    PlannerInvariant(String),
    Vm(crate::vm::VmError),
    File(db_core::storage::FileError),
    Io(String),
    /// A `SINCE`/`UNTIL` clause (db-core ADR 0018) reached the batch
    /// planner -- that clause is stream-only (db-core#308); column-rs
    /// has no `.log` files, so this only fires on a query that
    /// deliberately (or mistakenly) uses stream-only syntax.
    ScopeClauseUnsupported,
    /// [`QueryEngine::execute_streaming`] (db-core#456) was given a query
    /// shape it never even hands to `vm::engine::run_streaming` -- a
    /// `JOIN`, an `IN (SELECT ...)` semi-join, or a window function, each
    /// of which needs its build/probe or partition step to see every row
    /// first. [`VmError::NotStreamable`] (arriving as [`QueryError::Vm`])
    /// is the other half of this: everything `run_streaming` itself
    /// refuses (aggregates, `DISTINCT`, `ORDER BY`, `LIMIT`). Callers
    /// treat both the same way -- fall back to [`QueryEngine::execute`].
    NotStreamable(String),
    /// A table directory could not be read (tools#13): its manifest,
    /// WAL or one of its files.
    TableDirectory(String),
    /// A SQLite file or a query over its tables (tools#69): db-core's own
    /// kind and message, or why column-rs can't hand the query to db-core
    /// (`Unsupported`).
    Lookup(ErrorKind, String),
    /// The query isn't SQL column-rs reads (tools#74): a syntax error, or
    /// a construct or function the column dialect doesn't support.
    Parse(db_core::parser::ParseError),
}

impl fmt::Display for QueryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            QueryError::UnsupportedSemiJoin(msg) => write!(f, "unsupported semi-join: {msg}"),
            QueryError::UnsupportedSelectItem(msg) => write!(f, "unsupported SELECT item: {msg}"),
            QueryError::NoJoinClause => write!(f, "join planner called on a SELECT without a JOIN"),
            QueryError::UnsupportedJoinKind(kind) => {
                write!(
                    f,
                    "join kind {kind:?} is not yet executable (only Inner/Left are implemented)"
                )
            }
            QueryError::TooManyJoins(n) => write!(
                f,
                "column-rs joins two Parquet tables; this query has {n} joins \
                 (multi-table joins: t-rust-db/db-core#782)"
            ),
            QueryError::StarWithAggregation => write!(
                f,
                "SELECT * cannot be combined with GROUP BY, an aggregate, or a window function"
            ),
            QueryError::PlannerInvariant(msg) => write!(f, "planner invariant violated: {msg}"),
            QueryError::UnknownColumn(name) => write!(f, "unknown column: {name}"),
            QueryError::UnknownTable(name) => write!(f, "unknown table: {name}"),
            QueryError::DuplicateTable(name) => write!(f, "table already loaded: {name}"),
            QueryError::Vm(e) => write!(f, "{e}"),
            QueryError::File(e) => write!(f, "{e}"),
            QueryError::Io(e) => write!(f, "{e}"),
            QueryError::ScopeClauseUnsupported => {
                write!(
                    f,
                    "SINCE/UNTIL is a stream-only clause, not supported over Parquet"
                )
            }
            QueryError::NotStreamable(reason) => write!(f, "cannot stream this query: {reason}"),
            QueryError::TableDirectory(why) => write!(f, "table directory: {why}"),
            QueryError::Lookup(_, why) => write!(f, "{why}"),
            QueryError::Parse(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for QueryError {}

impl From<crate::vm::VmError> for QueryError {
    fn from(e: crate::vm::VmError) -> Self {
        QueryError::Vm(e)
    }
}

impl From<db_core::storage::FileError> for QueryError {
    fn from(e: db_core::storage::FileError) -> Self {
        QueryError::File(e)
    }
}

impl From<PlanError> for QueryError {
    fn from(e: PlanError) -> Self {
        match e {
            PlanError::UnknownColumn(name) => QueryError::UnknownColumn(name),
            PlanError::UnsupportedSemiJoin(msg) => QueryError::UnsupportedSemiJoin(msg),
            PlanError::UnsupportedJoinKind(kind) => QueryError::UnsupportedJoinKind(kind),
            PlanError::StarWithAggregation => QueryError::StarWithAggregation,
            PlanError::UnsupportedSelectItem(msg) => QueryError::UnsupportedSelectItem(msg),
            PlanError::NoJoinClause => QueryError::NoJoinClause,
            PlanError::Internal(msg) => QueryError::PlannerInvariant(msg),
            PlanError::ScopeClauseUnsupported => QueryError::ScopeClauseUnsupported,
        }
    }
}

impl From<EngineError> for QueryError {
    /// db-core's table-directory readers report an unknown column the way
    /// the planner does; everything else is the directory's.
    fn from(e: EngineError) -> Self {
        match e.message.strip_prefix("unknown column: ") {
            Some(name) if e.kind == ErrorKind::Compile => {
                QueryError::UnknownColumn(name.to_string())
            }
            _ => QueryError::TableDirectory(e.message),
        }
    }
}

/// db-core's message for a query over SQLite tables, unchanged: it says
/// what was refused and how to write it instead.
fn lookup_error(e: EngineError) -> QueryError {
    QueryError::Lookup(e.kind, e.message)
}

pub type Result<T> = std::result::Result<T, QueryError>;

/// One callback of [`QueryEngine::execute_streaming`] (db-core#456): the
/// output column names (always first, always exactly once), then each
/// chunk of the result as it becomes ready.
pub enum StreamEvent {
    Columns(Vec<String>),
    Chunk(Chunk),
}

/// One row group of a Parquet file as a lazily-loaded [`Segment`]: decodes
/// exactly the listed leaf columns into a [`Batch`] on `load()`.
struct RowGroupSegment<'a, 'm> {
    file: &'m ParquetFile<'a>,
    row_group_index: usize,
    columns: Vec<(String, usize, PhysicalType)>,
}

impl<'a, 'm> Segment for RowGroupSegment<'a, 'm> {
    /// Decode failures are errors, not data (column-rs#27): a column whose
    /// page cannot be read used to come back as `num_rows` NULLs, which
    /// made a corrupt or unsupported file look like a file full of NULLs.
    /// `decode_column_full` (db-core#467) keeps that contract -- it
    /// returns `Err`, never NULL-substitutes -- while also decoding
    /// straight into a typed `Column` (dict-encoded strings as
    /// `Column::Dict`, etc.) instead of this loader's own
    /// hand-rolled-per-`PhysicalType` `Vec<Value>` build, which is what
    /// kept storage-layer decode work (#457/#461/#472) from ever reaching
    /// this CLI.
    fn load(&self) -> std::result::Result<Arc<Batch>, VmError> {
        let rg = self
            .file
            .row_group(self.row_group_index)
            .ok_or_else(|| VmError::SegmentLoad {
                reason: format!(
                    "row group {} does not exist (file has {})",
                    self.row_group_index,
                    self.file.num_row_groups()
                ),
            })?;
        let num_rows = rg.num_rows() as usize;
        let mut batch = Batch::new(num_rows);
        for (name, index, physical_type) in &self.columns {
            let Decoded::Column(column) =
                decode_column_full(&rg, *index, *physical_type).map_err(|e| {
                    VmError::SegmentLoad {
                        reason: format!("row group {}: column `{name}`: {e}", self.row_group_index),
                    }
                })?;
            batch = batch.with_typed_column(name.clone(), column);
        }
        Ok(Arc::new(batch))
    }
}

/// Leaf schema columns as `(name, column_index, physical_type)`, in file order.
fn leaf_columns(file: &ParquetFile) -> Vec<(String, usize, PhysicalType)> {
    file.metadata()
        .schema
        .iter()
        .skip(1)
        .enumerate()
        .filter_map(|(i, s)| s.physical_type.map(|pt| (s.name.clone(), i, pt)))
        .collect()
}

/// Resolve `names` against a file's leaf columns, keeping each name's
/// original (possibly-qualified) display form as the batch column key.
fn resolve_columns(
    leaves: &[(String, usize, PhysicalType)],
    names: &[String],
) -> Result<Vec<(String, usize, PhysicalType)>> {
    let lookup: HashMap<&str, (usize, PhysicalType)> = leaves
        .iter()
        .map(|(n, i, t)| (n.as_str(), (*i, *t)))
        .collect();
    names
        .iter()
        .map(|name| {
            let (_, col) = planner::split_qualified(name);
            let (index, physical_type) = *lookup
                .get(col)
                .ok_or_else(|| QueryError::UnknownColumn(name.clone()))?;
            Ok((name.clone(), index, physical_type))
        })
        .collect()
}

/// One segment of a query's table: a Parquet file's row group, or a table
/// directory's row group or WAL page.
enum AnySegment<'a, 'm> {
    File(FileSegment<'a, 'm>),
    Dir(SnapshotSegment<'a, 'm>),
}

impl Segment for AnySegment<'_, '_> {
    fn load(&self) -> std::result::Result<Arc<Batch>, VmError> {
        match self {
            AnySegment::File(s) => s.load(),
            AnySegment::Dir(s) => s.load(),
        }
    }

    fn order_key_bound(&self, descending: bool) -> Option<Value> {
        match self {
            AnySegment::File(s) => s.order_key_bound(descending),
            AnySegment::Dir(s) => s.order_key_bound(descending),
        }
    }

    fn load_columns(&self, columns: &[String]) -> std::result::Result<Arc<Batch>, VmError> {
        match self {
            AnySegment::File(s) => s.load_columns(columns),
            AnySegment::Dir(s) => s.load_columns(columns),
        }
    }

    fn gather_at(
        &self,
        columns: &[String],
        positions: &[u32],
    ) -> Option<std::result::Result<Batch, VmError>> {
        match self {
            AnySegment::File(s) => s.gather_at(columns, positions),
            AnySegment::Dir(s) => s.gather_at(columns, positions),
        }
    }
}

/// A table as one query reads it: a Parquet file, or a table directory's
/// snapshot with its files opened.
enum Opened<'h> {
    File(&'h ParquetFile<'h>),
    Dir(&'h Snapshot, Vec<ParquetFile<'h>>),
}

impl<'h> Opened<'h> {
    /// A table directory's snapshot, its files opened.
    fn dir(snapshot: &'h Snapshot) -> Result<Self> {
        Ok(Opened::Dir(snapshot, snapshot_files(snapshot)?))
    }

    /// The table's column names: a file's schema leaves (as loaded), a
    /// directory's union of every file's and page's columns.
    fn column_names(&self) -> Result<Vec<String>> {
        Ok(match self {
            Opened::File(file) => file
                .metadata()
                .schema
                .iter()
                .skip(1)
                .map(|e| e.name.clone())
                .collect(),
            Opened::Dir(snapshot, _) => snapshot
                .schema()
                .map_err(|e| QueryError::TableDirectory(e.to_string()))?
                .into_iter()
                .map(|c| c.name)
                .collect(),
        })
    }

    /// One segment per row group (and, for a directory, per page) over
    /// `names`, running `program`'s late materialization: a filtered
    /// query decodes its other columns only at the matching rows
    /// (db-core#699).
    fn segments<'s>(
        &'s self,
        names: &[String],
        where_clause: Option<&Expr>,
        program: &'s Program,
    ) -> Result<Vec<AnySegment<'s, 's>>> {
        Ok(match self {
            Opened::File(file) => file_segments(file, names, where_clause, program)?
                .into_iter()
                .map(AnySegment::File)
                .collect(),
            Opened::Dir(snapshot, files) => snapshot_segments(snapshot, files, names, program)?
                .into_iter()
                .map(AnySegment::Dir)
                .collect(),
        })
    }

    /// The whole table's `names`, as one batch.
    fn whole(&self, names: &[String]) -> Result<Batch> {
        match self {
            Opened::File(file) => {
                read_whole_table(file, &resolve_columns(&leaf_columns(file), names)?)
            }
            Opened::Dir(snapshot, _) => Ok(read_snapshot(snapshot, names)?),
        }
    }

    /// Each column's type, by name, for `--pipe`'s typed output: a file's
    /// from its footer (timestamps in nanoseconds carry no converted type
    /// there and read as integers), a directory's from its schema.
    fn column_types(&self) -> HashMap<String, crate::pipe::ColType> {
        use crate::pipe::{ColType, TimeUnit};
        use db_core::storage::column::parquet::footer::ConvertedType;
        use db_core::storage::parquet::schema::ColumnType;
        match self {
            Opened::File(file) => file
                .metadata()
                .schema
                .iter()
                .skip(1)
                .filter_map(|e| {
                    let decimal = || ColType::Decimal {
                        precision: u8::try_from(e.precision.unwrap_or(0)).unwrap_or(0),
                        scale: u8::try_from(e.scale.unwrap_or(0)).unwrap_or(0),
                    };
                    let ty = match (e.physical_type?, e.converted_type) {
                        (_, Some(ConvertedType::Decimal)) => decimal(),
                        (_, Some(ConvertedType::TimestampMillis)) => {
                            ColType::Timestamp(TimeUnit::Millis)
                        }
                        (_, Some(ConvertedType::TimestampMicros)) => {
                            ColType::Timestamp(TimeUnit::Micros)
                        }
                        (
                            _,
                            Some(
                                ConvertedType::Uint8
                                | ConvertedType::Uint16
                                | ConvertedType::Uint32
                                | ConvertedType::Uint64,
                            ),
                        ) => ColType::UInt,
                        (PhysicalType::Int32 | PhysicalType::Int64 | PhysicalType::Int96, _) => {
                            ColType::Int
                        }
                        (PhysicalType::Float | PhysicalType::Double, _) => ColType::Double,
                        (PhysicalType::Boolean, _) => ColType::Bool,
                        (PhysicalType::ByteArray, None) | (PhysicalType::FixedLenByteArray, _) => {
                            ColType::Blob
                        }
                        (PhysicalType::ByteArray, Some(_)) => ColType::Text,
                        (PhysicalType::Unknown(_), _) => return None,
                    };
                    Some((e.name.clone(), ty))
                })
                .collect(),
            Opened::Dir(snapshot, _) => snapshot
                .schema()
                .unwrap_or_default()
                .into_iter()
                .map(|c| {
                    let ty = match c.column_type {
                        ColumnType::Boolean => ColType::Bool,
                        ColumnType::Int32 | ColumnType::Int64 => ColType::Int,
                        ColumnType::UInt8
                        | ColumnType::UInt16
                        | ColumnType::UInt32
                        | ColumnType::UInt64 => ColType::UInt,
                        ColumnType::Float | ColumnType::Double => ColType::Double,
                        ColumnType::Utf8 => ColType::Text,
                        ColumnType::Binary | ColumnType::FixedLenByteArray(_) => ColType::Blob,
                        ColumnType::TimestampMicros => ColType::Timestamp(TimeUnit::Micros),
                        ColumnType::TimestampNanos => ColType::Timestamp(TimeUnit::Nanos),
                        ColumnType::Decimal { precision, scale } => {
                            ColType::Decimal { precision, scale }
                        }
                        ColumnType::MapUtf8 => ColType::Map,
                    };
                    (c.name, ty)
                })
                .collect(),
        }
    }

    /// Row groups (plus a directory's pages) and rows, for `EXPLAIN`.
    fn stats(&self) -> TableStats {
        let (row_groups, rows) = match self {
            Opened::File(file) => (file.num_row_groups(), file.num_rows()),
            Opened::Dir(snapshot, files) => (
                files
                    .iter()
                    .map(ParquetFile::num_row_groups)
                    .sum::<usize>()
                    .saturating_add(snapshot.pages().len()),
                i64::try_from(snapshot.rows()).unwrap_or(i64::MAX),
            ),
        };
        TableStats {
            row_groups,
            rows,
            // column-rs is single-mode: every SCAN in the plan is a
            // Parquet file or a table directory's segments, so there's no
            // cross-mode join source label to carry (db-core#315,
            // ADR-0019 -- stream/row-driven joins only).
            source: None,
        }
    }
}

/// Read a whole table's worth of the given columns (concatenating every row
/// group) into a single in-memory batch, keyed by each column's display name
/// (see [`resolve_columns`]) -- used for join build/probe sides and window
/// queries, which need the whole table materialized rather than streamed
/// per row group.
fn read_whole_table(
    file: &ParquetFile,
    columns: &[(String, usize, PhysicalType)],
) -> Result<Batch> {
    // Accumulate per column, then hand each finished `Vec` to the batch
    // (db-core 0.76.2 shares columns as `Arc<Vec<Value>>`, so they are
    // built here and wrapped once, not extended in place).
    let mut merged_columns: Vec<(String, Vec<Value>)> = columns
        .iter()
        .map(|(name, _, _)| (name.clone(), Vec::new()))
        .collect();
    let mut num_rows = 0;
    for row_group_index in 0..file.num_row_groups() {
        let segment = RowGroupSegment {
            file,
            row_group_index,
            columns: columns.to_vec(),
        };
        let batch = segment.load()?;
        num_rows += batch.num_rows;
        for (name, values) in &mut merged_columns {
            // A decoded column lives in exactly one of `columns`/
            // `typed_columns` (db-core#467) -- `RowGroupSegment::load`
            // always uses the latter now, but this stays defensive
            // about which map since `Batch` itself makes no promise a
            // caller should rely on either way.
            if let Some(column) = batch.columns.get(name) {
                values.extend(column.iter().cloned());
            } else if let Some(column) = batch.typed_columns.get(name) {
                values.extend((0..batch.num_rows).map(|i| column.get(i)));
            }
        }
    }
    let mut merged = Batch::new(num_rows);
    for (name, values) in merged_columns {
        merged = merged.with_column(name, values);
    }
    Ok(merged)
}

/// Run a planned `program` over every row group of `file`: resolve its
/// `LoadColumn` columns against the file, hand one [`Segment`] per row
/// group to [`db_core::vm::engine::run`], which runs the body in parallel
/// and applies the trailing `Combine`/`Sort`/`Limit` phase (merge/`ORDER BY`/`LIMIT`) once.
/// The `codegen` subcommand's emitted binaries call this directly with
/// their `const PROGRAM`.
/// The plain table name a `SELECT` reads from. column-rs has no FROM-less
/// or subquery-in-FROM execution path, so anything else is reported as an
/// unknown table rather than silently mis-resolved.
fn from_table(select: &Select) -> Result<&str> {
    select
        .from
        .as_ref()
        .and_then(|f| f.first.name())
        .ok_or_else(|| QueryError::UnknownTable("<no table>".to_string()))
}

/// The first (and, for the batch planner, only) `JOIN` of a `SELECT`.
fn first_join(select: &Select) -> Option<&Join> {
    select.from.as_ref().and_then(|f| f.joins.first())
}

/// Refuse a second `JOIN` the batch planner would otherwise drop (tools#71).
fn one_join(select: &Select) -> Result<()> {
    match select.from.as_ref().map_or(0, |f| f.joins.len()) {
        0 | 1 => Ok(()),
        n => Err(QueryError::TooManyJoins(n)),
    }
}

/// A join's right-hand table name (see [`from_table`] for the same rule).
fn join_table(join: &Join) -> Result<&str> {
    join.table
        .name()
        .ok_or_else(|| QueryError::UnknownTable("<no table>".to_string()))
}

/// The `IN (SELECT ...)` subquery when the `WHERE` clause is exactly that
/// -- the shape `execute_semi_join` handles.
fn in_subquery(select: &Select) -> Option<&Select> {
    match &select.where_clause {
        Some(Expr {
            kind: ExprKind::InSubquery { subquery, .. },
            ..
        }) => Some(subquery),
        _ => None,
    }
}

/// A result column that is a window function (`f(...) OVER (...)`).
fn is_window_column(column: &ResultColumn) -> bool {
    match column {
        ResultColumn::Expr {
            expr:
                Expr {
                    kind: ExprKind::FunctionCall { tail, .. },
                    ..
                },
            ..
        } => matches!(tail.as_deref(), Some(t) if t.over.is_some()),
        _ => false,
    }
}

pub fn run_program(file: &ParquetFile, program: &[Opcode]) -> Result<QueryOutput> {
    run(
        &Opened::File(file),
        &Program::from_opcodes(program.iter().cloned()),
        None,
    )
}

fn run(table: &Opened<'_>, program: &Program, where_clause: Option<&Expr>) -> Result<QueryOutput> {
    let segments = table.segments(&program.columns_to_load(), where_clause, program)?;
    Ok(engine::run(&segments, program)?)
}

/// Execute a single-table `query` against `file`.
pub fn execute(file: &ParquetFile, query: &Select) -> Result<QueryOutput> {
    execute_on(&Opened::File(file), query)
}

fn execute_on(table: &Opened<'_>, query: &Select) -> Result<QueryOutput> {
    run(
        table,
        &planner::compile(query)?,
        query.where_clause.as_ref(),
    )
}

/// Streaming counterpart to [`run`] (db-core#456): hands each emitted
/// [`Chunk`] to `sink` as soon as it is ready instead of collecting the
/// whole [`QueryOutput`] first. `sink` never fails on its own account --
/// it is a printer, and matches [`stream_output::print_result_streaming`]'s
/// existing convention of writing best-effort and ignoring I/O errors --
/// so its `Result` only ever carries [`db_core::vm::engine::run_streaming`]'s
/// own [`VmError::NotStreamable`] back out, unmodified.
fn run_streaming(
    table: &Opened<'_>,
    program: &Program,
    where_clause: Option<&Expr>,
    sink: impl FnMut(Chunk),
) -> Result<()> {
    let segments = table.segments(&program.columns_to_load(), where_clause, program)?;
    let mut sink = sink;
    Ok(engine::run_streaming(&segments, program, move |chunk| {
        sink(chunk);
        Ok(())
    })?)
}

/// Streaming counterpart to [`execute`] (db-core#456).
pub fn execute_streaming(
    file: &ParquetFile,
    query: &Select,
    sink: impl FnMut(Chunk),
) -> Result<()> {
    run_streaming(
        &Opened::File(file),
        &planner::compile(query)?,
        query.where_clause.as_ref(),
        sink,
    )
}

/// Execute a query with exactly one `JOIN` (INNER or LEFT): the right
/// (build) side is materialized whole -- a hash join needs the entire
/// build table in memory regardless -- and the left (probe) side stays one
/// [`Segment`] per row group, so
/// [`db_core::vm::engine::run_join_segments`] builds the hash table once
/// and probes + runs the body per row group in parallel, never
/// materializing the joined table (column-rs#27; the parity `join` at 10M
/// rows ran single-threaded at 108x DuckDB and 4.4 GB when both sides
/// were read whole).
pub fn execute_joined(
    left_file: &ParquetFile,
    right_file: &ParquetFile,
    query: &Select,
) -> Result<QueryOutput> {
    execute_joined_on(&Opened::File(left_file), &Opened::File(right_file), query)
}

fn execute_joined_on(left: &Opened<'_>, right: &Opened<'_>, query: &Select) -> Result<QueryOutput> {
    let plan = planner::compile_join(query, planner::BuildSourceKind::InMemory)?;
    // The probe side decodes eagerly, as a file's row groups always have.
    let eager = Program::from_opcodes([]);
    let probe = left.segments(&plan.left_columns, None, &eager)?;
    let build = right.whole(&plan.right_columns)?;
    Ok(engine::run_join_segments(
        probe,
        ScanSource::InMemory(build),
        &plan,
        &NoResolver,
    )?)
}

/// Execute a query whose entire `WHERE` clause is `col IN (SELECT ...)`
/// (a semi-join): run the subquery against `sub_file`, keep the rows of
/// `main_file` whose `col` appears in its (single-column) result, and run
/// the rest of the query over the survivors.
pub fn execute_semi_join(
    main_file: &ParquetFile,
    sub_file: &ParquetFile,
    query: &Select,
) -> Result<QueryOutput> {
    execute_semi_join_on(&Opened::File(main_file), &Opened::File(sub_file), query)
}

fn execute_semi_join_on(
    main: &Opened<'_>,
    sub: &Opened<'_>,
    query: &Select,
) -> Result<QueryOutput> {
    let plan = planner::compile_semi_join(query)?;

    let sub_rows = execute_on(sub, &plan.subquery)?;
    if !sub_rows.is_empty() && sub_rows.num_columns() != 1 {
        return Err(QueryError::UnsupportedSemiJoin(
            "IN subquery must select exactly one column".to_string(),
        ));
    }
    let allowed: std::collections::HashSet<String> =
        sub_rows.rows().map(|row| row[0].to_string()).collect();

    let mut needed = plan.body.columns_to_load();
    if !needed.contains(&plan.key_column) {
        needed.push(plan.key_column.clone());
    }
    let batch = main.whole(&needed)?;
    let filtered = engine::semi_filter(&batch, &plan.key_column, &allowed)?;

    let segments = [InMemorySegment(filtered)];
    Ok(engine::run(&segments, &plan.body)?)
}

/// Execute a query whose `SELECT` list contains window functions: the
/// whole table is materialized (partitioning/sorting need every row) and
/// the planned window program runs over it as a single segment.
pub fn execute_windowed(file: &ParquetFile, query: &Select) -> Result<QueryOutput> {
    execute_windowed_on(&Opened::File(file), query)
}

fn execute_windowed_on(table: &Opened<'_>, query: &Select) -> Result<QueryOutput> {
    let program = planner::compile_window(query)?;
    let batch = table.whole(&program.columns_to_load())?;
    let segments = [InMemorySegment(batch)];
    Ok(engine::run(&segments, &program)?)
}

/// Query result with column names.
pub struct QueryResult {
    pub columns: Vec<String>,
    /// The result as db-core's chunked column-major `QueryOutput`
    /// (db-core#436): one chunk per row group, never concatenated, no
    /// per-row allocation. Consume it column by column via
    /// `output.chunks()`, or row by row via [`QueryResult::rows`].
    pub output: QueryOutput,
}

impl QueryResult {
    /// Rows, one at a time, across every chunk -- a transient `Vec<Value>`
    /// per yielded row, no materialized row table. The way a row-oriented
    /// consumer (a printer) should read a large result; use
    /// `output.into_rows()` only when every row is really needed at once.
    pub fn rows(&self) -> impl Iterator<Item = Vec<Value>> + '_ {
        self.output.rows()
    }
}

/// One loaded table: its file, memory-mapped (re-opened as a fresh
/// `ParquetFile` per query, matching the single-table engine's original
/// approach) rather than read eagerly into a `Vec<u8>` -- #108: with a
/// bounded scan reading only the row groups it needs, an eager full read
/// would make peak memory scale with file size regardless, undoing the
/// point. Plus its column names, cached so `.schema`/`.tables` don't need
/// to re-parse the footer.
struct Table {
    name: String,
    /// The file or directory it was loaded from.
    path: std::path::PathBuf,
    data: TableData,
    column_names: Vec<String>,
}

/// Where a table's rows are.
enum TableData {
    /// A Parquet file, memory-mapped.
    File(db_core::storage::MmapRegion),
    /// A table directory (tools#13), read as a fresh snapshot per query.
    Dir(std::path::PathBuf),
    /// A table in a SQLite file (tools#69), queried through db-core's row
    /// engine, or its cross-mode engine when joined to a Parquet file.
    Lookup,
}

/// The first 16 bytes of every SQLite database file.
const SQLITE_HEADER: &[u8; 16] = b"SQLite format 3\0";

/// Whether `path` is a SQLite database, by its header.
fn is_sqlite_file(path: &std::path::Path) -> bool {
    use std::io::Read;
    let mut header = [0u8; 16];
    std::fs::File::open(path)
        .and_then(|mut f| f.read_exact(&mut header))
        .is_ok_and(|()| &header == SQLITE_HEADER)
}

/// What a query holds for one table while it runs: a file's `ParquetFile`
/// over the engine's mapping, or a directory's snapshot.
enum Held<'d> {
    File(Box<ParquetFile<'d>>),
    Dir(Snapshot),
}

impl Held<'_> {
    fn open(&self) -> Result<Opened<'_>> {
        match self {
            Held::File(file) => Ok(Opened::File(file)),
            Held::Dir(snapshot) => Opened::dir(snapshot),
        }
    }
}

/// db-core's engine for a query that names a SQLite table (tools#69).
enum LookupEngine {
    /// Only SQLite tables.
    Row(RowEngine),
    /// One Parquet file joined to SQLite tables (db-core#598). Boxed: it
    /// holds a row engine and a batch engine.
    Cross(Box<CrossModeEngine>),
}

impl LookupEngine {
    fn as_engine(&self) -> &dyn Engine {
        match self {
            LookupEngine::Row(e) => e,
            LookupEngine::Cross(e) => e.as_ref(),
        }
    }

    fn as_engine_mut(&mut self) -> &mut dyn Engine {
        match self {
            LookupEngine::Row(e) => e,
            LookupEngine::Cross(e) => e.as_mut(),
        }
    }
}

/// db-core's result cell as column-rs's value. A blob becomes the hex
/// string column-rs uses for Parquet byte arrays.
fn cell_value(cell: Cell) -> Value {
    match cell {
        Cell::Null => Value::Null,
        Cell::Int(i) => Value::Int(i),
        Cell::Real(f) => Value::Float(f),
        Cell::Bool(b) => Value::Bool(b),
        Cell::Text(s) => Value::Str(s.into()),
        Cell::Blob(bytes) => Value::Str(
            bytes
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
                .into(),
        ),
        Cell::Decimal { unscaled, scale } => Value::Decimal { unscaled, scale },
    }
}

/// Every table a query names: its `FROM`, each `JOIN`, and an
/// `IN (SELECT ...)` subquery's `FROM`.
fn named_tables(query: &Select) -> Result<Vec<&str>> {
    let mut names = vec![from_table(query)?];
    if let Some(from) = &query.from {
        for join in &from.joins {
            names.push(join_table(join)?);
        }
    }
    if let Some(subquery) = in_subquery(query) {
        names.push(from_table(subquery)?);
    }
    Ok(names)
}

/// Whether `path` is a table directory: a directory holding a manifest.
fn is_table_dir(path: &std::path::Path) -> bool {
    path.is_dir() && path.join(MANIFEST_FILE).is_file()
}

fn open_snapshot(path: &std::path::Path) -> Result<Snapshot> {
    Snapshot::open(path).map_err(|e| QueryError::TableDirectory(format!("{}: {e}", path.display())))
}

/// High-level query engine wrapping one or more Parquet files or table
/// directories, addressed by table name (derived from each file's stem,
/// e.g. `orders.parquet` -> `orders`, or a directory's name) -- enough
/// tables loaded at once to run a `JOIN` or `IN (SELECT ...)` semi-join
/// across them.
#[derive(Default)]
pub struct QueryEngine {
    tables: Vec<Table>,
}

impl QueryEngine {
    /// Open one Parquet file for querying (table name derived from its
    /// file stem).
    pub fn open(path: &std::path::Path) -> Result<Self> {
        let mut engine = QueryEngine::default();
        engine.add_table(path, None)?;
        Ok(engine)
    }

    /// Open every path in order into a single session (table names derived
    /// from each file's stem). Errors on the first duplicate table name.
    pub fn open_many(paths: &[std::path::PathBuf]) -> Result<Self> {
        let mut engine = QueryEngine::default();
        for path in paths {
            engine.add_table(path, None)?;
        }
        Ok(engine)
    }

    /// Load an additional Parquet file or table directory into this
    /// session, under `name` (or its file stem, or the directory's name,
    /// if `None`). Errors if that table name is already loaded.
    pub fn add_table(&mut self, path: &std::path::Path, name: Option<String>) -> Result<()> {
        if is_table_dir(path) {
            let snapshot = open_snapshot(path)?;
            let column_names = Opened::dir(&snapshot)?.column_names()?;
            let table_name = name.unwrap_or_else(|| {
                path.file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("data")
                    .to_string()
            });
            if self.tables.iter().any(|t| t.name == table_name) {
                return Err(QueryError::DuplicateTable(table_name));
            }
            self.tables.push(Table {
                name: table_name,
                path: path.to_path_buf(),
                data: TableData::Dir(path.to_path_buf()),
                column_names,
            });
            return Ok(());
        }
        if is_sqlite_file(path) {
            return self.add_lookups(path, name);
        }
        let data = db_core::storage::PosixVfs
            .open(path)
            .and_then(|f| f.mmap())
            .map_err(|e| QueryError::Io(format!("{}: {e}", path.display())))?;
        let file = ParquetFile::open(&data)?;
        let table_name = name.unwrap_or_else(|| {
            path.file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("data")
                .to_string()
        });
        if self.tables.iter().any(|t| t.name == table_name) {
            return Err(QueryError::DuplicateTable(table_name));
        }
        // Skip first element (schema root).
        let column_names = file
            .metadata()
            .schema
            .iter()
            .skip(1)
            .map(|e| e.name.clone())
            .collect();
        self.tables.push(Table {
            name: table_name,
            path: path.to_path_buf(),
            data: TableData::File(data),
            column_names,
        });
        Ok(())
    }

    /// Loads every table of the SQLite file at `path` under its own name
    /// (tools#69). One SQLite file per session: db-core's cross-mode
    /// engine joins a Parquet file to the tables of one SQLite file.
    fn add_lookups(&mut self, path: &std::path::Path, name: Option<String>) -> Result<()> {
        if name.is_some() {
            return Err(QueryError::Lookup(
                ErrorKind::Unsupported,
                format!(
                    "{}: a SQLite file's tables keep their own names (drop AS)",
                    path.display()
                ),
            ));
        }
        if let Some(loaded) = self.lookup_file() {
            return Err(QueryError::Lookup(
                ErrorKind::Unsupported,
                format!(
                    "{}: one SQLite file per session, and {} is loaded",
                    path.display(),
                    loaded.display()
                ),
            ));
        }
        let tables = RowEngine::open(path)
            .and_then(|engine| engine.tables())
            .map_err(lookup_error)?;
        if let Some(t) = tables
            .iter()
            .find(|t| self.tables.iter().any(|loaded| loaded.name == t.name))
        {
            return Err(QueryError::DuplicateTable(t.name.clone()));
        }
        for t in tables {
            self.tables.push(Table {
                name: t.name,
                path: path.to_path_buf(),
                data: TableData::Lookup,
                column_names: t.columns.into_iter().map(|c| c.name).collect(),
            });
        }
        Ok(())
    }

    /// The SQLite file loaded into this session, if any.
    fn lookup_file(&self) -> Option<&std::path::Path> {
        self.tables
            .iter()
            .find(|t| matches!(t.data, TableData::Lookup))
            .map(|t| t.path.as_path())
    }

    fn table(&self, name: &str) -> Result<&Table> {
        self.tables
            .iter()
            .find(|t| t.name == name)
            .ok_or_else(|| QueryError::UnknownTable(name.to_string()))
    }

    /// Opens table `name` for one query: a file's footer, a directory's
    /// fresh snapshot.
    fn hold(&self, name: &str) -> Result<Held<'_>> {
        let table = self.table(name)?;
        Ok(match &table.data {
            TableData::File(data) => Held::File(Box::new(ParquetFile::open(data)?)),
            TableData::Dir(path) => Held::Dir(open_snapshot(path)?),
            // `lookup_engine` takes every query that names one first.
            TableData::Lookup => {
                return Err(QueryError::Lookup(
                    ErrorKind::Unsupported,
                    format!("{name} is a SQLite table; column-rs reads it through db-core"),
                ))
            }
        })
    }

    /// The engine for a query that names a SQLite table (tools#69), or
    /// `None` when it names none and column-rs runs it itself. Lookup
    /// tables alone go to db-core's row engine; joined to one Parquet file,
    /// to its cross-mode engine (db-core#598), which builds the SQLite
    /// side, probes the file row group by row group and prunes row groups
    /// from a filter on a lookup table.
    /// Parse `sql` in the column dialect, or, when that refuses it, as
    /// [`Self::parse_lookup`] reads it.
    pub fn parse(&self, sql: &str) -> Result<Select> {
        match db_core::parser::parse(sql) {
            Ok(query) => Ok(query),
            Err(e) => match self.parse_lookup(sql) {
                Some((Explain::None, query)) => Ok(query),
                _ => Err(QueryError::Parse(e)),
            },
        }
    }

    /// `sql` (`EXPLAIN [QUERY PLAN]` included) in sqlite3's own grammar, if
    /// every table it names is a SQLite table (tools#74). Such a query runs
    /// in db-core's row engine, which takes sqlite3's functions, so the
    /// column dialect's narrower list doesn't apply to it.
    pub fn parse_lookup(&self, sql: &str) -> Option<(Explain, Select)> {
        let (explain, query) = match db_core::parser::row::parse_explain(sql) {
            ParseOutcome::Accepted(explain) => match explain.body {
                ExplainBody::Select(query) if explain.query_plan => (Explain::QueryPlan, *query),
                ExplainBody::Select(query) => (Explain::Opcodes, *query),
                _ => return None,
            },
            _ => match db_core::parser::row::parse_select(sql) {
                ParseOutcome::Accepted(query) => (Explain::None, *query),
                _ => return None,
            },
        };
        let lookups_only = named_tables(&query)
            .ok()?
            .iter()
            .all(|name| matches!(self.table(name).map(|t| &t.data), Ok(TableData::Lookup)));
        lookups_only.then_some((explain, query))
    }

    fn lookup_engine(&self, query: &Select) -> Result<Option<LookupEngine>> {
        let mut named: Vec<&Table> = Vec::new();
        for name in named_tables(query)? {
            let table = self.table(name)?;
            if !named.iter().any(|t| t.name == table.name) {
                named.push(table);
            }
        }
        let Some(lookup) = named
            .iter()
            .find(|t| matches!(t.data, TableData::Lookup))
            .map(|t| t.path.as_path())
        else {
            return Ok(None);
        };
        let columns: Vec<&Table> = named
            .into_iter()
            .filter(|t| !matches!(t.data, TableData::Lookup))
            .collect();
        match columns.as_slice() {
            [] => Ok(Some(LookupEngine::Row(
                RowEngine::open(lookup).map_err(lookup_error)?,
            ))),
            [table] => match &table.data {
                TableData::File(_) => {
                    let stem = table.path.file_stem().and_then(|s| s.to_str());
                    if stem != Some(table.name.as_str()) {
                        return Err(QueryError::Lookup(
                            ErrorKind::Unsupported,
                            format!(
                                "{} is loaded as {}; joined to SQLite tables, a Parquet table \
                             is named after its file ({})",
                                table.path.display(),
                                table.name,
                                stem.unwrap_or("?")
                            ),
                        ));
                    }
                    Ok(Some(LookupEngine::Cross(Box::new(
                        CrossModeEngine::open_column_sqlite(&table.path, lookup)
                            .map_err(lookup_error)?,
                    ))))
                }
                _ => Err(QueryError::Lookup(
                    ErrorKind::Unsupported,
                    format!(
                        "joining the table directory {} to SQLite tables is not supported yet \
                     (t-rust-db/db-core#772); join a Parquet file",
                        table.name
                    ),
                )),
            },
            more => Err(QueryError::Lookup(
                ErrorKind::Unsupported,
                format!(
                    "a query joins one Parquet table to SQLite tables; this one names {}",
                    more.iter()
                        .map(|t| t.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            )),
        }
    }

    /// `EXPLAIN QUERY PLAN` for a query that names a SQLite table, or
    /// `None` when column-rs plans it itself ([`Self::explain`]). `sql` is
    /// the query without its `EXPLAIN` prefix. db-core's plan includes a
    /// `PRUNE ROW GROUPS` row (`N of M kept`) when a lookup filter prunes.
    pub fn explain_lookup(&self, query: &Select, sql: &str) -> Result<Option<Vec<PlanNode>>> {
        let Some(engine) = self.lookup_engine(query)? else {
            return Ok(None);
        };
        let rows = engine.as_engine().explain_plan(sql).map_err(lookup_error)?;
        Ok(Some(
            rows.into_iter()
                .map(|row| PlanNode {
                    id: u32::try_from(row.id).unwrap_or(u32::MAX),
                    parent: u32::try_from(row.parent).unwrap_or(u32::MAX),
                    detail: row.detail,
                })
                .collect(),
        ))
    }

    /// A bare `EXPLAIN`'s opcode listing for a query that names a SQLite
    /// table, as db-core labels it (build, probe and body sections), or
    /// `None` when column-rs plans it itself ([`Self::explain_opcodes`]).
    pub fn explain_opcodes_lookup(
        &self,
        query: &Select,
        sql: &str,
    ) -> Result<Option<Vec<db_core::engine::OpcodeSection>>> {
        let Some(engine) = self.lookup_engine(query)? else {
            return Ok(None);
        };
        Ok(Some(
            engine
                .as_engine()
                .explain_opcodes(sql)
                .map_err(lookup_error)?,
        ))
    }

    /// Streaming counterpart to [`Self::execute`] (db-core#456): only the
    /// plain single-table shape (no `JOIN`, `IN (SELECT ...)` semi-join, or
    /// window function) can stream -- each of those needs its build/probe
    /// or partition step to see every row before producing any output, the
    /// opposite of what streaming is for. `on_event` is called exactly
    /// once with [`StreamEvent::Columns`] (before any chunk), then once
    /// per emitted chunk with [`StreamEvent::Chunk`], in result order --
    /// one callback rather than two separate closures because both would
    /// otherwise need to borrow the same output writer mutably at once.
    /// Returns [`QueryError::NotStreamable`] for a join/semi-join/window
    /// query, or `db_core::vm::batch::VmError::NotStreamable` (as
    /// [`QueryError::Vm`]) for an aggregate, `DISTINCT`, `ORDER BY`, or
    /// `LIMIT` one -- callers (column-rs's `-c` one-shot mode) treat both
    /// as "fall back to `execute`", not as a real query error.
    pub fn execute_streaming(
        &self,
        sql: &str,
        mut on_event: impl FnMut(StreamEvent),
    ) -> Result<()> {
        let query = self.parse(sql)?;
        let from = from_table(&query)?;
        if first_join(&query).is_some() {
            return Err(QueryError::NotStreamable("query has a JOIN".to_string()));
        }
        if matches!(self.table(from)?.data, TableData::Lookup) {
            return Err(QueryError::NotStreamable(
                "query reads a SQLite table".to_string(),
            ));
        }
        if in_subquery(&query).is_some() {
            return Err(QueryError::NotStreamable(
                "query is an IN (SELECT ...) semi-join".to_string(),
            ));
        }
        if query.columns.iter().any(is_window_column) {
            return Err(QueryError::NotStreamable(
                "query has a window function".to_string(),
            ));
        }
        let held = self.hold(from)?;
        let table = held.open()?;
        let query = planner::expand_star(&query, &table.column_names()?)?;
        let program = planner::compile(&query)?;
        // Decided before `on_event` fires at all: an aggregate, DISTINCT,
        // ORDER BY, or LIMIT query is only discovered non-streamable by
        // `vm::engine::run_streaming` itself, after it may already have
        // started calling a sink -- too late for a caller (this one) that
        // is about to commit to printing a header. Check first instead.
        if !engine::is_streamable(&program) {
            return Err(QueryError::NotStreamable(
                "query has an aggregate, DISTINCT, ORDER BY, or LIMIT".to_string(),
            ));
        }
        on_event(StreamEvent::Columns(planner::output_column_names(&query)));
        run_streaming(&table, &program, query.where_clause.as_ref(), |chunk| {
            on_event(StreamEvent::Chunk(chunk))
        })
    }

    /// Execute a SQL query and return results. Dispatches to a `JOIN`,
    /// `IN (SELECT ...)` semi-join, windowed, or plain single-table
    /// execution path depending on the parsed query's shape.
    pub fn execute(&self, sql: &str) -> Result<QueryResult> {
        self.execute_typed(sql).map(|(result, _)| result)
    }

    /// [`Self::execute`], plus each result column's table type when it is
    /// a bare column reference (`None` for an expression, whose type the
    /// caller infers from its values): what `--pipe` encodes by.
    pub fn execute_typed(
        &self,
        sql: &str,
    ) -> Result<(QueryResult, Vec<Option<crate::pipe::ColType>>)> {
        let query = self.parse(sql)?;

        // A query that names a SQLite table runs in db-core (tools#69).
        // `--pipe` infers its column types from the values.
        if let Some(mut engine) = self.lookup_engine(&query)? {
            let result = engine
                .as_engine_mut()
                .run_query(sql)
                .map_err(lookup_error)?;
            let types = vec![None; result.columns.len()];
            let rows = result
                .rows
                .into_iter()
                .map(|row| row.into_iter().map(cell_value).collect())
                .collect();
            return Ok((
                QueryResult {
                    columns: result.columns,
                    output: QueryOutput::from_rows(rows),
                },
                types,
            ));
        }

        one_join(&query)?;

        // Resolve `*` against the queried table(s)' schema before anything
        // else touches `query.columns` -- a `JOIN` expands against both
        // sides, qualified `table.column` per the naming convention
        // `compile_join` already uses for its `left_columns`/`right_columns`.
        let from = from_table(&query)?;
        let main_held = self.hold(from)?;
        let main = main_held.open()?;
        let right_held = first_join(&query)
            .map(|join| join_table(join).and_then(|name| self.hold(name)))
            .transpose()?;
        let right = right_held.as_ref().map(Held::open).transpose()?;
        let schema: Vec<String> = match (first_join(&query), &right) {
            (Some(join), Some(right)) => {
                let right_name = join_table(join)?;
                main.column_names()?
                    .iter()
                    .map(|c| format!("{from}.{c}"))
                    .chain(
                        right
                            .column_names()?
                            .iter()
                            .map(|c| format!("{right_name}.{c}")),
                    )
                    .collect()
            }
            _ => main.column_names()?,
        };
        let query = planner::expand_star(&query, &schema)?;

        let has_window = query.columns.iter().any(is_window_column);
        let rows = if let Some(subquery) = in_subquery(&query) {
            let sub_held = self.hold(from_table(subquery)?)?;
            execute_semi_join_on(&main, &sub_held.open()?, &query)?
        } else if let Some(right) = &right {
            execute_joined_on(&main, right, &query)?
        } else if has_window {
            execute_windowed_on(&main, &query)?
        } else {
            execute_on(&main, &query)?
        };

        let main_types = main.column_types();
        let right_types = right.as_ref().map(Opened::column_types).unwrap_or_default();
        let right_name = first_join(&query).map(join_table).transpose()?;
        let types = query
            .columns
            .iter()
            .map(|column| match column {
                ResultColumn::Expr {
                    expr:
                        Expr {
                            kind: ExprKind::Column { table, name, .. },
                            ..
                        },
                    ..
                } => {
                    let in_right = table.as_deref().is_some_and(|t| Some(t) == right_name);
                    let (first, second) = if in_right {
                        (&right_types, &main_types)
                    } else {
                        (&main_types, &right_types)
                    };
                    first.get(name).or_else(|| second.get(name)).copied()
                }
                _ => None,
            })
            .collect();

        Ok((
            QueryResult {
                columns: planner::output_column_names(&query),
                output: rows,
            },
            types,
        ))
    }

    /// List loaded table names, in load order.
    pub fn table_names(&self) -> Vec<&str> {
        self.tables.iter().map(|t| t.name.as_str()).collect()
    }

    /// Get schema information: (table_name, columns), in load order. A
    /// table directory's columns are read fresh: appends may add some.
    pub fn schemas(&self) -> Vec<(&str, Vec<String>)> {
        self.tables
            .iter()
            .map(|t| {
                let fresh = match &t.data {
                    TableData::Dir(_) => self
                        .hold(&t.name)
                        .and_then(|held| held.open()?.column_names())
                        .ok(),
                    TableData::File(_) | TableData::Lookup => None,
                };
                (
                    t.name.as_str(),
                    fresh.unwrap_or_else(|| t.column_names.clone()),
                )
            })
            .collect()
    }

    /// Build a human-readable execution plan for `query` without running it
    /// (#99): the planner's [`db_core::codegen::batch::explain`], fed each
    /// referenced table's row-group/row counts from its Parquet footer.
    pub fn explain(&self, query: &Select) -> Result<Vec<PlanNode>> {
        one_join(query)?;
        let mut tables = vec![from_table(query)?];
        if let Some(subquery) = in_subquery(query) {
            tables.push(from_table(subquery)?);
        }
        if let Some(join) = first_join(query) {
            tables.push(join_table(join)?);
        }
        let mut stats: HashMap<&str, TableStats> = HashMap::new();
        for table in tables {
            let held = self.hold(table)?;
            stats.insert(table, held.open()?.stats());
        }
        Ok(planner::explain(query, |table| {
            stats
                .get(table)
                .cloned()
                .expect("every table the query references was opened above")
        })?)
    }

    /// Build a bare `EXPLAIN`'s opcode listing for `query` (#55): the
    /// planner's [`db_core::codegen::batch::explain_opcodes`], one section
    /// per phase the executor actually runs.
    pub fn explain_opcodes(&self, query: &Select) -> Result<Vec<OpcodeSection>> {
        one_join(query)?;
        Ok(planner::explain_opcodes(query)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use db_core::parser as sql;

    fn fixture_path(name: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name)
    }

    fn details(nodes: &[PlanNode]) -> Vec<&str> {
        nodes.iter().map(|n| n.detail.as_str()).collect()
    }

    /// db-core#747: the query's WHERE clause goes to `file_segments`, which
    /// leaves out row groups its footer statistics rule out. `production`
    /// has three, with `id` ascending (1..=2048, 2049..=4096, 4097..=5000);
    /// the counts are what every group would give.
    #[test]
    fn where_clause_prunes_row_groups_without_changing_results() {
        let mut engine = QueryEngine::default();
        engine
            .add_table(&fixture_path("production.parquet"), None)
            .unwrap();
        for (clause, expected) in [
            ("id < 100", 99),
            ("id > 4500", 500),
            ("id BETWEEN 2000 AND 2100", 101),
            ("id > 99999", 0),
            ("id >= 1", 5000),
        ] {
            let result = engine
                .execute(&format!("SELECT COUNT(*) FROM production WHERE {clause}"))
                .unwrap();
            let rows: Vec<_> = result.rows().collect();
            assert_eq!(rows, vec![vec![Value::Int(expected)]], "{clause}");
        }
    }

    #[test]
    fn explain_plain_filter_group_by_aggregate() {
        let mut engine = QueryEngine::default();
        engine
            .add_table(&fixture_path("production.parquet"), None)
            .unwrap();
        let query = sql::parse("SELECT region, SUM(amount), COUNT(*) FROM production WHERE id > 1000 GROUP BY region ORDER BY region").unwrap();
        let nodes = engine.explain(&query).unwrap();

        assert_eq!(nodes[0].detail, "QUERY PLAN");
        assert!(
            nodes[0].parent == nodes[0].id,
            "root's parent must equal its own id"
        );
        assert!(details(&nodes)
            .iter()
            .any(|d| d.starts_with("SCAN production (") && d.ends_with("row groups, ~5000 rows)")));
        assert!(details(&nodes).contains(&"LOAD COLUMNS: region, amount, id"));
        assert!(details(&nodes).contains(&"FILTER: id > 1000"));
        assert!(details(&nodes).contains(&"GROUP BY: region"));
        assert!(details(&nodes).contains(&"AGGREGATE: SUM(amount)"));
        assert!(details(&nodes).contains(&"AGGREGATE: COUNT(*)"));
        assert!(details(&nodes).contains(&"ORDER BY: region"));
        assert_eq!(
            nodes.last().unwrap().detail,
            "EMIT: region, SUM(amount), COUNT(*)"
        );

        // AGGREGATE nodes must nest under the GROUP BY node, not the root.
        let group_id = nodes
            .iter()
            .find(|n| n.detail == "GROUP BY: region")
            .unwrap()
            .id;
        let agg_parents: Vec<u32> = nodes
            .iter()
            .filter(|n| n.detail.starts_with("AGGREGATE"))
            .map(|n| n.parent)
            .collect();
        assert_eq!(agg_parents, vec![group_id, group_id]);
    }

    #[test]
    fn explain_limit_without_group_by() {
        let mut engine = QueryEngine::default();
        engine
            .add_table(&fixture_path("orders.parquet"), None)
            .unwrap();
        let query = sql::parse("SELECT id, region_key FROM orders LIMIT 3").unwrap();
        let nodes = engine.explain(&query).unwrap();
        assert!(details(&nodes).contains(&"LIMIT: 3"));
        assert!(!details(&nodes)
            .iter()
            .any(|d| d.starts_with("FILTER") || d.starts_with("GROUP BY")));
    }

    #[test]
    fn explain_join_describes_both_scans_and_condition() {
        let mut engine = QueryEngine::default();
        engine
            .add_table(&fixture_path("orders.parquet"), None)
            .unwrap();
        engine
            .add_table(&fixture_path("regions.parquet"), None)
            .unwrap();
        let query =
            sql::parse("SELECT orders.id, regions.budget FROM orders JOIN regions ON orders.region_key = regions.key ORDER BY orders.id")
                .unwrap();
        let nodes = engine.explain(&query).unwrap();

        assert!(details(&nodes)
            .iter()
            .any(|d| d.starts_with("SCAN orders (")));
        assert!(details(&nodes)
            .iter()
            .any(|d| d.starts_with("SCAN regions (")));
        assert!(details(&nodes).contains(&"LOAD COLUMNS: orders.id, orders.region_key"));
        assert!(details(&nodes).contains(&"LOAD COLUMNS: regions.budget, regions.key"));
        assert!(details(&nodes).contains(&"HASH JOIN: orders.region_key = regions.key"));
    }

    #[test]
    fn explain_semi_join_describes_subquery() {
        let mut engine = QueryEngine::default();
        engine
            .add_table(&fixture_path("orders.parquet"), None)
            .unwrap();
        engine
            .add_table(&fixture_path("regions.parquet"), None)
            .unwrap();
        let query = sql::parse(
            "SELECT id FROM orders WHERE region_key IN (SELECT key FROM regions) ORDER BY id",
        )
        .unwrap();
        let nodes = engine.explain(&query).unwrap();

        assert!(details(&nodes).contains(&"SEMI JOIN: region_key IN (SELECT key FROM regions)"));
        assert!(details(&nodes).contains(&"LOAD COLUMNS: id, region_key"));
        assert!(details(&nodes).contains(&"LOAD COLUMNS: key"));
        // No FILTER node -- the semi-join's WHERE isn't a VM predicate.
        assert!(!details(&nodes).iter().any(|d| d.starts_with("FILTER")));
    }

    #[test]
    fn explain_window_describes_partition_and_order() {
        let mut engine = QueryEngine::default();
        engine
            .add_table(&fixture_path("orders.parquet"), None)
            .unwrap();
        let query = sql::parse("SELECT id, region_key, ROW_NUMBER() OVER (PARTITION BY region_key ORDER BY id) FROM orders ORDER BY id").unwrap();
        let nodes = engine.explain(&query).unwrap();

        assert!(details(&nodes)
            .contains(&"WINDOW: ROW_NUMBER() OVER (PARTITION BY region_key ORDER BY id)"));
        assert_eq!(
            nodes.last().unwrap().detail,
            "EMIT: id, region_key, ROW_NUMBER()"
        );
    }

    #[test]
    fn parse_explain_distinguishes_opcodes_from_query_plan() {
        use db_core::parser::Explain;

        let (explain, query) = sql::parse_explain("EXPLAIN SELECT id FROM orders").unwrap();
        assert_eq!(explain, Explain::Opcodes);
        assert_eq!(from_table(&query).unwrap(), "orders");

        let (explain, _) = sql::parse_explain("EXPLAIN QUERY PLAN SELECT id FROM orders").unwrap();
        assert_eq!(explain, Explain::QueryPlan);

        let (explain, _) = sql::parse_explain("SELECT id FROM orders").unwrap();
        assert_eq!(explain, Explain::None);
    }
}
