//! Parquet glue between `db_storage`'s `column::parquet` module and
//! db-core's planner ([`db_core::codegen::batch`]) and cross-segment
//! engine ([`db_core::vm::engine`]): resolve a program's column names
//! against a file's leaf schema, expose each row group as a
//! [`Segment`], materialize whole tables for the join/semi-join/window
//! paths, and dispatch a parsed query to the right shape.
//!
//! Nothing here plans or post-processes anything -- `compile()`,
//! `AggPart`, `post_process` (now the `Opcode::Combine`/`Sort`/`Limit` tail applied by
//! `db_core::vm::engine::run`) and the `EXPLAIN` tree all moved to db-core
//! (ADR 0007 there), because none of them ever touched `ParquetFile`.
//! What stays is exactly what does.

use crate::vm::{Batch, Opcode, Program, Segment, Value};
use db_core::codegen::batch::{self as planner, PlanError, TableStats};
use db_core::parser::ast::{Expr, ExprKind, Join, JoinOp, ResultColumn, Select};
use db_core::vm::engine::{self, InMemorySegment};
use db_storage::column::parquet::footer::PhysicalType;
use db_storage::{ParquetFile, Vfs, VfsFile};
use std::collections::HashMap;
use std::fmt;

pub use db_core::codegen::batch::{OpcodeRow, OpcodeSection, PlanNode};

#[derive(Debug)]
pub enum QueryError {
    UnknownColumn(String),
    UnknownTable(String),
    DuplicateTable(String),
    UnsupportedSemiJoin(String),
    /// A `SELECT`-list item the batch planner cannot compile.
    UnsupportedSelectItem(String),
    /// `Right`/`Full`/`Cross` are parseable (`db_core::parser::ast::JoinOp`) but
    /// only `Inner`/`Left` hash-join execution exists so far.
    UnsupportedJoinKind(JoinOp),
    /// `SELECT *` (or a mixed `SELECT col, *`) combined with `GROUP BY`, an
    /// aggregate, or a window function.
    StarWithAggregation,
    Vm(crate::vm::VmError),
    File(db_storage::FileError),
    Io(String),
}

impl fmt::Display for QueryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            QueryError::UnsupportedSemiJoin(msg) => write!(f, "unsupported semi-join: {msg}"),
            QueryError::UnsupportedSelectItem(msg) => write!(f, "unsupported SELECT item: {msg}"),
            QueryError::UnsupportedJoinKind(kind) => {
                write!(
                    f,
                    "join kind {kind:?} is not yet executable (only Inner/Left are implemented)"
                )
            }
            QueryError::StarWithAggregation => write!(
                f,
                "SELECT * cannot be combined with GROUP BY, an aggregate, or a window function"
            ),
            QueryError::UnknownColumn(name) => write!(f, "unknown column: {name}"),
            QueryError::UnknownTable(name) => write!(f, "unknown table: {name}"),
            QueryError::DuplicateTable(name) => write!(f, "table already loaded: {name}"),
            QueryError::Vm(e) => write!(f, "{e}"),
            QueryError::File(e) => write!(f, "{e}"),
            QueryError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for QueryError {}

impl From<crate::vm::VmError> for QueryError {
    fn from(e: crate::vm::VmError) -> Self {
        QueryError::Vm(e)
    }
}

impl From<db_storage::FileError> for QueryError {
    fn from(e: db_storage::FileError) -> Self {
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
        }
    }
}

pub type Result<T> = std::result::Result<T, QueryError>;

/// One row group of a Parquet file as a lazily-loaded [`Segment`]: decodes
/// exactly the listed leaf columns into a [`Batch`] on `load()`.
struct RowGroupSegment<'a, 'm> {
    file: &'m ParquetFile<'a>,
    row_group_index: usize,
    columns: Vec<(String, usize, PhysicalType)>,
}

impl<'a, 'm> Segment for RowGroupSegment<'a, 'm> {
    fn load(&self) -> Batch {
        let rg = self
            .file
            .row_group(self.row_group_index)
            .expect("row group index within range");
        let num_rows = rg.num_rows() as usize;
        let mut batch = Batch::new(num_rows);
        for (name, index, physical_type) in &self.columns {
            let values = match physical_type {
                PhysicalType::Int64 => rg.read_int64_column(*index).map(|col| {
                    col.into_iter()
                        .map(|v| v.map_or(Value::Null, Value::Int))
                        .collect()
                }),
                PhysicalType::Int32 => rg.read_int32_column(*index).map(|col| {
                    col.into_iter()
                        .map(|v| v.map_or(Value::Null, |i| Value::Int(i as i64)))
                        .collect()
                }),
                PhysicalType::Double => rg.read_double_column(*index).map(|col| {
                    col.into_iter()
                        .map(|v| v.map_or(Value::Null, Value::Float))
                        .collect()
                }),
                PhysicalType::Float => rg.read_float_column(*index).map(|col| {
                    col.into_iter()
                        .map(|v| v.map_or(Value::Null, |f| Value::Float(f as f64)))
                        .collect()
                }),
                PhysicalType::Boolean => rg.read_boolean_column(*index).map(|col| {
                    col.into_iter()
                        .map(|v| v.map_or(Value::Null, Value::Bool))
                        .collect()
                }),
                _ => rg.read_string_column(*index).map(|col| {
                    col.into_iter()
                        .map(|v| v.map_or(Value::Null, |s| Value::Str(s.into())))
                        .collect()
                }),
            }
            .unwrap_or_else(|_| vec![Value::Null; num_rows]);
            batch = batch.with_column(name.clone(), values);
        }
        batch
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

/// Every row group of `file` as a [`Segment`] over `columns`.
fn row_group_segments<'f>(
    file: &'f ParquetFile<'f>,
    columns: &[(String, usize, PhysicalType)],
) -> Vec<RowGroupSegment<'f, 'f>> {
    (0..file.num_row_groups())
        .map(|i| RowGroupSegment {
            file,
            row_group_index: i,
            columns: columns.to_vec(),
        })
        .collect()
}

/// Read a whole table's worth of the given columns (concatenating every row
/// group) into a single in-memory batch, keyed by each column's display name
/// (see [`resolve_columns`]) -- used for join build/probe sides and window
/// queries, which need the whole table materialized rather than streamed
/// per row group.
fn read_whole_table(file: &ParquetFile, columns: &[(String, usize, PhysicalType)]) -> Batch {
    let mut merged = Batch::new(0);
    for (name, _, _) in columns {
        merged.columns.insert(name.clone(), Vec::new());
    }
    for row_group_index in 0..file.num_row_groups() {
        let segment = RowGroupSegment {
            file,
            row_group_index,
            columns: columns.to_vec(),
        };
        let batch = segment.load();
        merged.num_rows += batch.num_rows;
        for (name, _, _) in columns {
            if let Some(values) = batch.columns.get(name) {
                merged
                    .columns
                    .get_mut(name)
                    .unwrap()
                    .extend(values.iter().cloned());
            }
        }
    }
    merged
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

pub fn run_program(file: &ParquetFile, program: &[Opcode]) -> Result<Vec<Vec<Value>>> {
    run(file, &Program::from_opcodes(program.iter().cloned()))
}

fn run(file: &ParquetFile, program: &Program) -> Result<Vec<Vec<Value>>> {
    let columns = resolve_columns(&leaf_columns(file), &program.columns_to_load())?;
    let segments = row_group_segments(file, &columns);
    Ok(engine::run(&segments, program)?)
}

/// Execute a single-table `query` against `file`.
pub fn execute(file: &ParquetFile, query: &Select) -> Result<Vec<Vec<Value>>> {
    run(file, &planner::compile(query))
}

/// Execute a query with exactly one `JOIN` (INNER or LEFT): materialize
/// both tables fully (joins need the whole build side in memory
/// regardless), then hand them to [`db_core::vm::engine::run_join`].
pub fn execute_joined(
    left_file: &ParquetFile,
    right_file: &ParquetFile,
    query: &Select,
) -> Result<Vec<Vec<Value>>> {
    let plan = planner::compile_join(query)?;
    let left_columns = resolve_columns(&leaf_columns(left_file), &plan.left_columns)?;
    let right_columns = resolve_columns(&leaf_columns(right_file), &plan.right_columns)?;
    let left = read_whole_table(left_file, &left_columns);
    let right = read_whole_table(right_file, &right_columns);
    Ok(engine::run_join(&left, &right, &plan)?)
}

/// Execute a query whose entire `WHERE` clause is `col IN (SELECT ...)`
/// (a semi-join): run the subquery against `sub_file`, keep the rows of
/// `main_file` whose `col` appears in its (single-column) result, and run
/// the rest of the query over the survivors.
pub fn execute_semi_join(
    main_file: &ParquetFile,
    sub_file: &ParquetFile,
    query: &Select,
) -> Result<Vec<Vec<Value>>> {
    let plan = planner::compile_semi_join(query)?;

    let sub_rows = execute(sub_file, &plan.subquery)?;
    if sub_rows.first().is_some_and(|row| row.len() != 1) {
        return Err(QueryError::UnsupportedSemiJoin(
            "IN subquery must select exactly one column".to_string(),
        ));
    }
    let allowed: std::collections::HashSet<String> =
        sub_rows.into_iter().map(|row| row[0].to_string()).collect();

    let mut needed = plan.body.columns_to_load();
    if !needed.contains(&plan.key_column) {
        needed.push(plan.key_column.clone());
    }
    let columns = resolve_columns(&leaf_columns(main_file), &needed)?;
    let batch = read_whole_table(main_file, &columns);
    let filtered = engine::semi_filter(&batch, &plan.key_column, &allowed)?;

    let segments = [InMemorySegment(filtered)];
    Ok(engine::run(&segments, &plan.body)?)
}

/// Execute a query whose `SELECT` list contains window functions: the
/// whole table is materialized (partitioning/sorting need every row) and
/// the planned window program runs over it as a single segment.
pub fn execute_windowed(file: &ParquetFile, query: &Select) -> Result<Vec<Vec<Value>>> {
    let program = planner::compile_window(query);
    let columns = resolve_columns(&leaf_columns(file), &program.columns_to_load())?;
    let batch = read_whole_table(file, &columns);
    let segments = [InMemorySegment(batch)];
    Ok(engine::run(&segments, &program)?)
}

/// Query result with column names.
pub struct QueryResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Value>>,
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
    data: db_storage::MmapRegion,
    column_names: Vec<String>,
}

/// High-level query engine wrapping one or more Parquet files, addressed by
/// table name (derived from each file's stem, e.g. `orders.parquet` ->
/// `orders`) -- enough tables loaded at once to run a `JOIN` or `IN
/// (SELECT ...)` semi-join across them.
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

    /// Load an additional Parquet file into this session, under `name` (or
    /// its file stem if `None`). Errors if that table name is already loaded.
    pub fn add_table(&mut self, path: &std::path::Path, name: Option<String>) -> Result<()> {
        let data = db_storage::PosixVfs
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
            data,
            column_names,
        });
        Ok(())
    }

    fn table_data(&self, name: &str) -> Result<&[u8]> {
        self.tables
            .iter()
            .find(|t| t.name == name)
            .map(|t| &t.data[..])
            .ok_or_else(|| QueryError::UnknownTable(name.to_string()))
    }

    fn table_schema(&self, name: &str) -> Result<&[String]> {
        self.tables
            .iter()
            .find(|t| t.name == name)
            .map(|t| t.column_names.as_slice())
            .ok_or_else(|| QueryError::UnknownTable(name.to_string()))
    }

    /// Execute a SQL query and return results. Dispatches to a `JOIN`,
    /// `IN (SELECT ...)` semi-join, windowed, or plain single-table
    /// execution path depending on the parsed query's shape.
    pub fn execute(&self, sql: &str) -> Result<QueryResult> {
        let query =
            db_core::parser::parse(sql).map_err(|e| QueryError::UnknownColumn(e.to_string()))?;

        // Resolve `*` against the queried table(s)' schema before anything
        // else touches `query.columns` -- a `JOIN` expands against both
        // sides, qualified `table.column` per the naming convention
        // `compile_join` already uses for its `left_columns`/`right_columns`.
        let from = from_table(&query)?;
        let schema: Vec<String> = if let Some(join) = first_join(&query) {
            let right = join_table(join)?;
            let left_schema = self.table_schema(from)?;
            let right_schema = self.table_schema(right)?;
            left_schema
                .iter()
                .map(|c| format!("{from}.{c}"))
                .chain(right_schema.iter().map(|c| format!("{right}.{c}")))
                .collect()
        } else {
            self.table_schema(from)?.to_vec()
        };
        let query = planner::expand_star(&query, &schema)?;

        let main_data = self.table_data(from)?;
        let main_file = ParquetFile::open(main_data)?;

        let has_window = query.columns.iter().any(is_window_column);
        let rows = if let Some(subquery) = in_subquery(&query) {
            let sub_data = self.table_data(from_table(subquery)?)?;
            let sub_file = ParquetFile::open(sub_data)?;
            execute_semi_join(&main_file, &sub_file, &query)?
        } else if let Some(join) = first_join(&query) {
            let right_data = self.table_data(join_table(join)?)?;
            let right_file = ParquetFile::open(right_data)?;
            execute_joined(&main_file, &right_file, &query)?
        } else if has_window {
            execute_windowed(&main_file, &query)?
        } else {
            execute(&main_file, &query)?
        };

        Ok(QueryResult {
            columns: planner::output_column_names(&query),
            rows,
        })
    }

    /// List loaded table names, in load order.
    pub fn table_names(&self) -> Vec<&str> {
        self.tables.iter().map(|t| t.name.as_str()).collect()
    }

    /// Get schema information: (table_name, columns), in load order.
    pub fn schemas(&self) -> Vec<(&str, Vec<String>)> {
        self.tables
            .iter()
            .map(|t| (t.name.as_str(), t.column_names.clone()))
            .collect()
    }

    /// Build a human-readable execution plan for `query` without running it
    /// (#99): the planner's [`db_core::codegen::batch::explain`], fed each
    /// referenced table's row-group/row counts from its Parquet footer.
    pub fn explain(&self, query: &Select) -> Result<Vec<PlanNode>> {
        let mut tables = vec![from_table(query)?];
        if let Some(subquery) = in_subquery(query) {
            tables.push(from_table(subquery)?);
        }
        if let Some(join) = first_join(query) {
            tables.push(join_table(join)?);
        }
        let mut stats: HashMap<&str, TableStats> = HashMap::new();
        for table in tables {
            let file = ParquetFile::open(self.table_data(table)?)?;
            stats.insert(
                table,
                TableStats {
                    row_groups: file.num_row_groups(),
                    rows: file.num_rows(),
                },
            );
        }
        Ok(planner::explain(query, |table| {
            stats
                .get(table)
                .copied()
                .expect("every table the query references was opened above")
        }))
    }

    /// Build a bare `EXPLAIN`'s opcode listing for `query` (#55): the
    /// planner's [`db_core::codegen::batch::explain_opcodes`], one section
    /// per phase the executor actually runs.
    pub fn explain_opcodes(&self, query: &Select) -> Result<Vec<OpcodeSection>> {
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
