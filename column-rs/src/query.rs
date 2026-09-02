//! Glue between [`sql_expr`]/[`sql_parser`], [`crate::vm`] and
//! [`db_parquet`]: compiles a parsed `Query` into a VM program, executes it
//! across a Parquet file's row groups in parallel, and merges partial
//! per-segment aggregates.
//!
//! [`execute`] supports a single table, an optional `WHERE` clause, optional
//! `GROUP BY` with `COUNT`/`SUM`/`AVG`/`MIN`/`MAX`, and `ORDER BY`/`LIMIT`
//! on the final result. [`execute_joined`] runs a two-table `INNER`/`LEFT`
//! hash join (equi-join only, one join clause); [`execute_semi_join`] runs
//! `WHERE col IN (SELECT ...)`; [`execute_windowed`] runs `SELECT`s whose
//! items are window functions -- all three bypass the VM for the parts of
//! the query the register-machine model doesn't fit (materializing full
//! tables and computing directly over them instead).

use db_parquet::footer::PhysicalType;
use db_parquet::ParquetFile;
use db_storage::{Vfs, VfsFile};
use sql_expr::{AggFunc, BinOp, Expr, JoinKind, OrderBy, Query, SelectItem, WindowFunc, WindowSpec};
use sql_types::Literal;
use crate::vm::{Batch, MapOp, Opcode, Segment, Value};
use std::collections::HashMap;
use std::fmt;

#[derive(Debug)]
pub enum QueryError {
    UnknownColumn(String),
    UnknownTable(String),
    DuplicateTable(String),
    UnsupportedSemiJoin(String),
    Vm(crate::vm::VmError),
    File(db_parquet::FileError),
    Io(String),
}

impl fmt::Display for QueryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            QueryError::UnsupportedSemiJoin(msg) => write!(f, "unsupported semi-join: {msg}"),
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

impl From<db_parquet::FileError> for QueryError {
    fn from(e: db_parquet::FileError) -> Self {
        QueryError::File(e)
    }
}

pub type Result<T> = std::result::Result<T, QueryError>;

struct RowGroupSegment<'a, 'm> {
    file: &'m ParquetFile<'a>,
    row_group_index: usize,
    columns: Vec<(String, usize, PhysicalType)>,
}

impl<'a, 'm> Segment for RowGroupSegment<'a, 'm> {
    fn load(&self) -> Batch {
        let rg = self.file.row_group(self.row_group_index).expect("row group index within range");
        let num_rows = rg.num_rows() as usize;
        let mut batch = Batch::new(num_rows);
        for (name, index, physical_type) in &self.columns {
            let values = match physical_type {
                PhysicalType::Int64 => {
                    rg.read_int64_column(*index).map(|col| col.into_iter().map(|v| v.map_or(Value::Null, Value::Int)).collect())
                }
                PhysicalType::Int32 => rg
                    .read_int32_column(*index)
                    .map(|col| col.into_iter().map(|v| v.map_or(Value::Null, |i| Value::Int(i as i64))).collect()),
                PhysicalType::Double => {
                    rg.read_double_column(*index).map(|col| col.into_iter().map(|v| v.map_or(Value::Null, Value::Float)).collect())
                }
                PhysicalType::Float => rg
                    .read_float_column(*index)
                    .map(|col| col.into_iter().map(|v| v.map_or(Value::Null, |f| Value::Float(f as f64))).collect()),
                PhysicalType::Boolean => {
                    rg.read_boolean_column(*index).map(|col| col.into_iter().map(|v| v.map_or(Value::Null, Value::Bool)).collect())
                }
                _ => rg.read_string_column(*index).map(|col| col.into_iter().map(|v| v.map_or(Value::Null, |s| Value::Str(s.into()))).collect()),
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

fn map_bin_op(op: BinOp) -> MapOp {
    match op {
        BinOp::Add => MapOp::Add,
        BinOp::Sub => MapOp::Sub,
        BinOp::Mul => MapOp::Mul,
        BinOp::Div => MapOp::Div,
        BinOp::Eq => MapOp::Eq,
        BinOp::Ne => MapOp::Ne,
        BinOp::Lt => MapOp::Lt,
        BinOp::Le => MapOp::Le,
        BinOp::Gt => MapOp::Gt,
        BinOp::Ge => MapOp::Ge,
        BinOp::And => MapOp::And,
        BinOp::Or => MapOp::Or,
    }
}

fn literal_value(lit: &Literal) -> Value {
    match lit {
        Literal::Int(v) => Value::Int(*v),
        Literal::Float(v) => Value::Float(*v),
        Literal::Str(v) => Value::Str(v.clone().into()),
    }
}

/// Compiles the plan once (columns to load, program) so it can be reused
/// across every segment.
pub(crate) struct Plan {
    pub(crate) columns_to_load: Vec<String>,
    pub(crate) program: Vec<Opcode>,
    /// Registers emitted, in order: group-by key columns first, then one
    /// per requested aggregate part (see [`AggPart`]).
    pub(crate) agg_parts: Vec<AggPart>,
    /// `query.group_by.len()` -- how many of the emitted row's leading
    /// columns are group-by keys (0 when there's no `GROUP BY`).
    pub(crate) num_group_keys: usize,
    /// `(output column index, descending)`, derived from `query.order_by`
    /// -- fully static once compiled, so codegen (#98, #101) can emit it
    /// as a const alongside `PROGRAM`.
    pub(crate) order_by: Option<(usize, bool)>,
    pub(crate) limit: Option<usize>,
}

/// One `GROUP BY`/aggregate part of an emitted row, in emit order -- public
/// so codegen'd binaries (#98, #101) can pass a `const` slice of these to
/// [`post_process`] alongside their `const PROGRAM`.
#[derive(Debug, Clone, Copy)]
pub enum AggPart {
    GroupKey,
    Sum,
    Count,
    Min,
    Max,
    /// `(sum_index, count_index)` into the emitted row, combined at the end.
    Avg(usize, usize),
}

pub(crate) fn compile(query: &Query) -> Plan {
    let mut next_reg = 0usize;
    let mut column_regs: HashMap<String, usize> = HashMap::new();
    let mut program = Vec::new();
    let mut columns_to_load = Vec::new();

    let load_column = |name: &str, program: &mut Vec<Opcode>, column_regs: &mut HashMap<String, usize>, next_reg: &mut usize, columns_to_load: &mut Vec<String>| -> usize {
        if let Some(reg) = column_regs.get(name) {
            return *reg;
        }
        let reg = *next_reg;
        *next_reg += 1;
        program.push(Opcode::LoadColumn { reg, column: name.to_string().into() });
        column_regs.insert(name.to_string(), reg);
        columns_to_load.push(name.to_string());
        reg
    };

    fn compile_expr(
        expr: &Expr,
        program: &mut Vec<Opcode>,
        column_regs: &mut HashMap<String, usize>,
        next_reg: &mut usize,
        columns_to_load: &mut Vec<String>,
    ) -> usize {
        match expr {
            Expr::Column(name) => {
                if let Some(reg) = column_regs.get(name) {
                    return *reg;
                }
                let reg = *next_reg;
                *next_reg += 1;
                program.push(Opcode::LoadColumn { reg, column: name.clone().into() });
                column_regs.insert(name.clone(), reg);
                columns_to_load.push(name.clone());
                reg
            }
            Expr::Literal(lit) => {
                let reg = *next_reg;
                *next_reg += 1;
                program.push(Opcode::LoadConst { reg, value: literal_value(lit) });
                reg
            }
            Expr::InSubquery { .. } => {
                // `execute_semi_join` handles `IN (subquery)` itself and
                // strips it from `where_clause` before ever calling
                // `compile` -- reaching this arm means `IN (subquery)` was
                // used via the regular single-table `execute`, which can't
                // run a subquery. Compile to an always-false predicate
                // (no rows) rather than panicking.
                let reg = *next_reg;
                *next_reg += 1;
                program.push(Opcode::LoadConst { reg, value: Value::Bool(false) });
                reg
            }
            Expr::BinaryOp(lhs, op, rhs) => {
                let a = compile_expr(lhs, program, column_regs, next_reg, columns_to_load);
                let b = compile_expr(rhs, program, column_regs, next_reg, columns_to_load);
                let dst = *next_reg;
                *next_reg += 1;
                program.push(Opcode::Map { dst, op: map_bin_op(*op), a, b });
                dst
            }
        }
    }

    // Load every column the group-by keys and select-list aggregates need
    // *before* compiling WHERE/Filter: Filter only shrinks registers that
    // are already live, so anything loaded afterwards would keep the
    // batch's full (pre-filter) length and desync from filtered registers.
    let mut group_by_regs = Vec::new();
    for name in &query.group_by {
        group_by_regs.push(load_column(name, &mut program, &mut column_regs, &mut next_reg, &mut columns_to_load));
    }
    let mut agg_srcs = Vec::new();
    for item in &query.columns {
        match item {
            SelectItem::Agg(_, Some(name)) => {
                agg_srcs.push(load_column(name, &mut program, &mut column_regs, &mut next_reg, &mut columns_to_load));
            }
            // Plain projected columns are emitted (not aggregated), but they
            // must be loaded here for the same reason as the keys above: a
            // column first loaded below the Filter keeps its full pre-filter
            // length while the filtered registers shrink, and Emit then
            // indexes past the end of the short ones. `load_column` memoizes,
            // so the projection code further down reuses these registers
            // instead of emitting a second LoadColumn.
            SelectItem::Column(name) if query.group_by.is_empty() => {
                load_column(name, &mut program, &mut column_regs, &mut next_reg, &mut columns_to_load);
                agg_srcs.push(0);
            }
            _ => agg_srcs.push(0),
        }
    }

    if let Some(where_clause) = &query.where_clause {
        let predicate = compile_expr(where_clause, &mut program, &mut column_regs, &mut next_reg, &mut columns_to_load);
        program.push(Opcode::Filter { predicate });
    }

    let mut agg_parts = Vec::new();
    for _ in &query.group_by {
        agg_parts.push(AggPart::GroupKey);
    }

    let mut aggs: Vec<(AggFunc, Option<usize>)> = Vec::new();
    let mut agg_dst = Vec::new();
    let alloc_dst = |next_reg: &mut usize| {
        let dst = *next_reg;
        *next_reg += 1;
        dst
    };

    let mut emit_regs = group_by_regs.clone();

    for (i, item) in query.columns.iter().enumerate() {
        if let SelectItem::Agg(func, arg) = item {
            let src = arg.as_ref().map(|_| agg_srcs[i]);
            match func {
                AggFunc::Avg => {
                    let sum_dst = alloc_dst(&mut next_reg);
                    let count_dst = alloc_dst(&mut next_reg);
                    aggs.push((AggFunc::Sum, src));
                    agg_dst.push(sum_dst);
                    aggs.push((AggFunc::Count, src));
                    agg_dst.push(count_dst);
                    agg_parts.push(AggPart::Avg(emit_regs.len(), emit_regs.len() + 1));
                    emit_regs.push(sum_dst);
                    emit_regs.push(count_dst);
                }
                other => {
                    let dst = alloc_dst(&mut next_reg);
                    aggs.push((*other, src));
                    agg_dst.push(dst);
                    agg_parts.push(match other {
                        AggFunc::Sum => AggPart::Sum,
                        AggFunc::Count => AggPart::Count,
                        AggFunc::Min => AggPart::Min,
                        AggFunc::Max => AggPart::Max,
                        AggFunc::Avg => unreachable!(),
                    });
                    emit_regs.push(dst);
                }
            }
        } else if let SelectItem::Column(name) = item {
            // A plain column in the SELECT list: if there's no GROUP BY,
            // it isn't loaded/emitted anywhere else yet, so load and emit
            // it directly here. With a GROUP BY, it's expected to already
            // be one of the group-by columns (already in `emit_regs` via
            // `group_by_regs` above) -- SQL requires non-aggregated SELECT
            // columns to be group-by keys, so this doesn't double-emit.
            if query.group_by.is_empty() {
                let reg = load_column(name, &mut program, &mut column_regs, &mut next_reg, &mut columns_to_load);
                emit_regs.push(reg);
            }
        }
    }

    if !aggs.is_empty() || !group_by_regs.is_empty() {
        program.push(Opcode::GroupReduce { group_by: group_by_regs.into(), aggs: aggs.into(), agg_dst: agg_dst.into() });
    }

    program.push(Opcode::Emit { registers: emit_regs.into() });

    let order_by = query.order_by.as_ref().and_then(|OrderBy { column, descending }| select_output_index(query, column).map(|pos| (pos, *descending)));

    Plan { columns_to_load, program, agg_parts, num_group_keys: query.group_by.len(), order_by, limit: query.limit }
}

/// Combine two emitted rows for the same group key, applying the
/// associative merge appropriate to each [`AggPart`].
fn merge_rows(parts: &[AggPart], into: &mut [Value], from: &[Value]) {
    for (i, part) in parts.iter().enumerate() {
        match part {
            AggPart::GroupKey => {}
            AggPart::Sum | AggPart::Count => {
                into[i] = Value::Float(into[i].as_f64().unwrap_or(0.0) + from[i].as_f64().unwrap_or(0.0));
            }
            AggPart::Min => {
                if let (Some(a), Some(b)) = (into[i].as_f64(), from[i].as_f64()) {
                    into[i] = Value::Float(a.min(b));
                } else if matches!(into[i], Value::Null) {
                    into[i] = from[i].clone();
                }
            }
            AggPart::Max => {
                if let (Some(a), Some(b)) = (into[i].as_f64(), from[i].as_f64()) {
                    into[i] = Value::Float(a.max(b));
                } else if matches!(into[i], Value::Null) {
                    into[i] = from[i].clone();
                }
            }
            AggPart::Avg(_, _) => {}
        }
    }
}

fn finalize_row(parts: &[AggPart], row: Vec<Value>) -> Vec<Value> {
    let mut out = Vec::with_capacity(parts.len());
    let mut skip: Option<usize> = None;
    for (i, part) in parts.iter().enumerate() {
        if skip == Some(i) {
            continue;
        }
        match part {
            AggPart::Avg(sum_i, count_i) => {
                let (sum, count) = (row[*sum_i].as_f64().unwrap_or(0.0), row[*count_i].as_f64().unwrap_or(0.0));
                out.push(if count == 0.0 { Value::Null } else { Value::Float(sum / count) });
                skip = Some(*count_i);
            }
            _ => out.push(row[i].clone()),
        }
    }
    out
}

/// Execute `query` against `file`, running the WHERE/GroupReduce pipeline in
/// parallel across row groups and merging partial aggregates.
pub fn execute(file: &ParquetFile, query: &Query) -> Result<Vec<Vec<Value>>> {
    let plan = compile(query);

    // #108: with just a `LIMIT` -- no `WHERE`, `ORDER BY`, `GROUP BY`, or
    // aggregate, all of which need to see the whole table (or at least
    // evaluate every row) before the first `limit` output rows are
    // determined -- the first `limit` rows of the first however-many row
    // groups *are* the answer. Scan row groups in file order and stop as
    // soon as enough rows are collected, instead of decoding every row
    // group up front like `run_program`/`run_parallel` does.
    if let Some(limit) = bounded_scan_limit(query) {
        return bounded_scan(file, &plan.columns_to_load, &plan.program, limit);
    }

    let has_group_by = !query.group_by.is_empty();
    let has_aggs = query.columns.iter().any(|c| matches!(c, SelectItem::Agg(..)));

    // `ORDER BY ... LIMIT ...` with no `GROUP BY`/aggregate can be resolved
    // as a bounded top-N during the parallel scan itself (#109), instead of
    // materializing every row and fully sorting in `post_process`.
    let top_n_spec = match (&query.order_by, query.limit, has_group_by || has_aggs) {
        (Some(OrderBy { column, descending }), Some(limit), false) => {
            select_output_index(query, column).map(|col| crate::vm::TopN { col, descending: *descending, limit })
        }
        _ => None,
    };

    let rows = match &top_n_spec {
        Some(spec) => run_program_top_n(file, &plan.columns_to_load, &plan.program, spec)?,
        None => run_program(file, &plan.columns_to_load, &plan.program)?,
    };
    Ok(post_process(&plan.agg_parts, plan.num_group_keys, plan.order_by, plan.limit, rows))
}

/// Load `columns_to_load` from every row group of `file` and run the
/// compiled VM `program` against them in parallel, with no further
/// post-processing (no `GROUP BY` merge across segments, no `ORDER BY`, no
/// `LIMIT`) -- for callers that already know their program doesn't need
/// it. [`execute`] is built on this plus [`post_process`]; codegen'd
/// binaries (`codegen.rs`, #98) call this directly since their `program`
/// is a `const` baked in ahead of time, not compiled from a live `Query`.
pub fn run_program(file: &ParquetFile, columns_to_load: &[impl AsRef<str>], program: &[Opcode]) -> Result<Vec<Vec<Value>>> {
    let columns = resolve_program_columns(file, columns_to_load)?;

    let segments: Vec<Box<dyn Segment + '_>> = (0..file.num_row_groups())
        .map(|i| Box::new(RowGroupSegment { file, row_group_index: i, columns: columns.clone() }) as Box<dyn Segment + '_>)
        .collect();

    Ok(crate::vm::run_parallel(&segments, program)?)
}

/// Resolve `columns_to_load` (bare column names) against `file`'s leaf
/// schema into `(name, column_index, physical_type)` triples -- shared by
/// [`run_program`] and [`bounded_scan`].
fn resolve_program_columns(file: &ParquetFile, columns_to_load: &[impl AsRef<str>]) -> Result<Vec<(String, usize, PhysicalType)>> {
    let leaves = leaf_columns(file);
    let column_lookup: HashMap<&str, (usize, PhysicalType)> = leaves.iter().map(|(n, i, t)| (n.as_str(), (*i, *t))).collect();

    let mut columns = Vec::new();
    for name in columns_to_load {
        let name = name.as_ref();
        let (index, physical_type) = *column_lookup.get(name).ok_or_else(|| QueryError::UnknownColumn(name.to_string()))?;
        columns.push((name.to_string(), index, physical_type));
    }
    Ok(columns)
}

/// `query.limit` when it's safe to satisfy via [`bounded_scan`]'s
/// sequential prefix scan: no `WHERE`, no `ORDER BY`, no `GROUP BY`, and
/// no aggregate/window `SELECT` item.
fn bounded_scan_limit(query: &Query) -> Option<usize> {
    if query.where_clause.is_some() || query.order_by.is_some() || !query.group_by.is_empty() {
        return None;
    }
    if query.columns.iter().any(|c| matches!(c, SelectItem::Agg(..) | SelectItem::Window(_))) {
        return None;
    }
    query.limit
}

/// Sequentially scan row groups in file order, running `program` against
/// each one's freshly-loaded batch, stopping (and truncating to exactly
/// `limit` rows) as soon as enough have been collected -- row groups past
/// that point are never read or decoded.
fn bounded_scan(file: &ParquetFile, columns_to_load: &[impl AsRef<str>], program: &[Opcode], limit: usize) -> Result<Vec<Vec<Value>>> {
    let columns = resolve_program_columns(file, columns_to_load)?;
    let mut rows = Vec::with_capacity(limit);
    for row_group_index in 0..file.num_row_groups() {
        if rows.len() >= limit {
            break;
        }
        let segment = RowGroupSegment { file, row_group_index, columns: columns.to_vec() };
        let batch = segment.load();
        let mut vm = crate::vm::Vm::new();
        vm.execute(&batch, program)?;
        rows.extend(vm.take_output());
    }
    rows.truncate(limit);
    Ok(rows)
}

/// Like [`run_program`], but for `ORDER BY ... LIMIT ...` queries: bounds
/// each segment (and the final merge) to `spec.limit` rows via
/// [`crate::vm::run_parallel_top_n`] (#109) instead of materializing every
/// row before sorting in `post_process`.
fn run_program_top_n(file: &ParquetFile, columns_to_load: &[impl AsRef<str>], program: &[Opcode], spec: &crate::vm::TopN) -> Result<Vec<Vec<Value>>> {
    let columns = resolve_program_columns(file, columns_to_load)?;

    let segments: Vec<Box<dyn Segment + '_>> = (0..file.num_row_groups())
        .map(|i| Box::new(RowGroupSegment { file, row_group_index: i, columns: columns.clone() }) as Box<dyn Segment + '_>)
        .collect();

    Ok(crate::vm::run_parallel_top_n(&segments, program, spec)?)
}

/// Apply `GROUP BY` merging, `ORDER BY`, and `LIMIT` to a flat row list --
/// shared by the single-table and joined execution paths, and callable
/// directly by codegen'd binaries (#98, #101), which have `agg_parts`/
/// `num_group_keys`/`order_by`/`limit` as `const`s rather than a live
/// [`Query`].
pub fn post_process(agg_parts: &[AggPart], num_group_keys: usize, order_by: Option<(usize, bool)>, limit: Option<usize>, rows: Vec<Vec<Value>>) -> Vec<Vec<Value>> {
    let mut result_rows = if !agg_parts.is_empty() {
        let mut groups: Vec<(Vec<Value>, Vec<Value>)> = Vec::new();
        let mut index: HashMap<String, usize> = HashMap::new();
        for row in rows {
            let key: Vec<Value> = row[..num_group_keys].to_vec();
            let key_str = key.iter().map(Value::to_string).collect::<Vec<_>>().join("\u{0}");
            match index.get(&key_str) {
                Some(&i) => merge_rows(agg_parts, &mut groups[i].1, &row),
                None => {
                    index.insert(key_str, groups.len());
                    groups.push((key, row));
                }
            }
        }
        groups.into_iter().map(|(_, row)| finalize_row(agg_parts, row)).collect()
    } else {
        rows
    };

    if let Some((pos, descending)) = order_by {
        result_rows.sort_by(|a, b| crate::vm::compare_for_order(&a[pos], &b[pos], descending));
    }

    if let Some(limit) = limit {
        result_rows.truncate(limit);
    }

    result_rows
}

/// Split a (possibly qualified) column name into `(table_prefix, column)`.
fn split_qualified(name: &str) -> (Option<&str>, &str) {
    match name.find('.') {
        Some(idx) => (Some(&name[..idx]), &name[idx + 1..]),
        None => (None, name),
    }
}

/// Resolve `names` against a file's leaf columns, keeping each name's
/// original (possibly-qualified) display form as the batch column key.
fn resolve_columns(leaves: &[(String, usize, PhysicalType)], names: &[String]) -> Result<Vec<(String, usize, PhysicalType)>> {
    let lookup: HashMap<&str, (usize, PhysicalType)> = leaves.iter().map(|(n, i, t)| (n.as_str(), (*i, *t))).collect();
    names
        .iter()
        .map(|name| {
            let (_, col) = split_qualified(name);
            let (index, physical_type) = *lookup.get(col).ok_or_else(|| QueryError::UnknownColumn(name.clone()))?;
            Ok((name.clone(), index, physical_type))
        })
        .collect()
}

/// Read a whole table's worth of the given columns (concatenating every row
/// group) into a single in-memory batch, keyed by each column's display name
/// (see [`resolve_columns`]) -- used for join build/probe sides, which need
/// the whole table materialized rather than streamed per row group.
fn read_whole_table(file: &ParquetFile, columns: &[(String, usize, PhysicalType)]) -> Batch {
    let mut merged = Batch::new(0);
    for (name, _, _) in columns {
        merged.columns.insert(name.clone(), Vec::new());
    }
    for row_group_index in 0..file.num_row_groups() {
        let segment = RowGroupSegment { file, row_group_index, columns: columns.to_vec() };
        let batch = segment.load();
        merged.num_rows += batch.num_rows;
        for (name, _, _) in columns {
            if let Some(values) = batch.columns.get(name) {
                merged.columns.get_mut(name).unwrap().extend(values.iter().cloned());
            }
        }
    }
    merged
}

struct InMemorySegment(Batch);

impl Segment for InMemorySegment {
    fn load(&self) -> Batch {
        self.0.clone()
    }
}

/// Execute a query with exactly one `JOIN` (INNER or LEFT; only the first
/// join clause is honored -- chained/multi-way joins aren't supported).
/// Unlike [`execute`], this always materializes both tables fully rather
/// than streaming row groups in parallel: joins need the whole build side
/// (and, here, the whole probe side) in memory regardless, so there's no
/// parallel-scan win to preserve. An unqualified column name is assumed to
/// belong to the `FROM` table; a right-table column must be qualified
/// (`table.column`) to disambiguate.
pub fn execute_joined(left_file: &ParquetFile, right_file: &ParquetFile, query: &Query) -> Result<Vec<Vec<Value>>> {
    let join = query.joins.first().expect("execute_joined requires at least one join");
    let plan = compile(query);

    let mut needed: Vec<String> = plan.columns_to_load.clone();
    for extra in [&join.left_col, &join.right_col] {
        if !needed.contains(extra) {
            needed.push(extra.clone());
        }
    }

    let mut left_names = Vec::new();
    let mut right_names = Vec::new();
    for name in &needed {
        let (prefix, _) = split_qualified(name);
        match prefix {
            None => left_names.push(name.clone()),
            Some(p) if p == query.from => left_names.push(name.clone()),
            Some(p) if p == join.table => right_names.push(name.clone()),
            Some(_) => return Err(QueryError::UnknownColumn(name.clone())),
        }
    }

    let left_leaves = leaf_columns(left_file);
    let right_leaves = leaf_columns(right_file);
    let left_columns = resolve_columns(&left_leaves, &left_names)?;
    let right_columns = resolve_columns(&right_leaves, &right_names)?;

    let left_batch = read_whole_table(left_file, &left_columns);
    let right_batch = read_whole_table(right_file, &right_columns);

    let right_key = right_batch.columns.get(&join.right_col).ok_or_else(|| QueryError::UnknownColumn(join.right_col.clone()))?;
    let mut right_index: HashMap<String, Vec<usize>> = HashMap::new();
    for (row, value) in right_key.iter().enumerate() {
        right_index.entry(value.to_string()).or_default().push(row);
    }

    let left_key = left_batch.columns.get(&join.left_col).ok_or_else(|| QueryError::UnknownColumn(join.left_col.clone()))?;

    // (left_row, right_row) pairs -- `right_row = None` for an unmatched LEFT JOIN row.
    let mut pairs: Vec<(usize, Option<usize>)> = Vec::new();
    for (left_row, key) in left_key.iter().enumerate() {
        match right_index.get(&key.to_string()) {
            Some(right_rows) => pairs.extend(right_rows.iter().map(|&r| (left_row, Some(r)))),
            None if join.kind == JoinKind::Left => pairs.push((left_row, None)),
            None => {}
        }
    }

    let mut joined = Batch::new(pairs.len());
    for name in &left_names {
        let column = &left_batch.columns[name];
        joined.columns.insert(name.clone(), pairs.iter().map(|(l, _)| column[*l].clone()).collect());
    }
    for name in &right_names {
        let column = &right_batch.columns[name];
        joined.columns.insert(name.clone(), pairs.iter().map(|(_, r)| r.map_or(Value::Null, |r| column[r].clone())).collect());
    }

    let segments: Vec<Box<dyn Segment>> = vec![Box::new(InMemorySegment(joined))];
    let rows = crate::vm::run_parallel(&segments, &plan.program)?;
    Ok(post_process(&plan.agg_parts, plan.num_group_keys, plan.order_by, plan.limit, rows))
}

/// Execute a query whose entire `WHERE` clause is `col IN (SELECT ...)` (a
/// semi-join): rows of `main_file` are kept only when `col`'s value appears
/// anywhere in the subquery's (single-column) result, run against
/// `sub_file`. Combining the `IN` clause with other conditions via `AND`/`OR`
/// isn't supported -- the semi-join must be the whole `WHERE` clause.
pub fn execute_semi_join(main_file: &ParquetFile, sub_file: &ParquetFile, query: &Query) -> Result<Vec<Vec<Value>>> {
    let Some(Expr::InSubquery { expr, subquery }) = &query.where_clause else {
        return Err(QueryError::UnsupportedSemiJoin("WHERE clause must be exactly `col IN (SELECT ...)`".to_string()));
    };
    let Expr::Column(col_name) = expr.as_ref() else {
        return Err(QueryError::UnsupportedSemiJoin("IN's left-hand side must be a bare column".to_string()));
    };

    let sub_rows = execute(sub_file, subquery)?;
    if sub_rows.first().is_some_and(|row| row.len() != 1) {
        return Err(QueryError::UnsupportedSemiJoin("IN subquery must select exactly one column".to_string()));
    }
    let allowed: std::collections::HashSet<String> = sub_rows.into_iter().map(|row| row[0].to_string()).collect();

    let mut stripped = query.clone();
    stripped.where_clause = None;
    let plan = compile(&stripped);

    let mut needed = plan.columns_to_load.clone();
    if !needed.contains(col_name) {
        needed.push(col_name.clone());
    }

    let leaves = leaf_columns(main_file);
    let columns = resolve_columns(&leaves, &needed)?;
    let batch = read_whole_table(main_file, &columns);

    let key = batch.columns.get(col_name).ok_or_else(|| QueryError::UnknownColumn(col_name.clone()))?;
    let keep: Vec<usize> = (0..batch.num_rows).filter(|&i| allowed.contains(&key[i].to_string())).collect();

    let mut filtered = Batch::new(keep.len());
    for name in &needed {
        let column = &batch.columns[name];
        filtered.columns.insert(name.clone(), keep.iter().map(|&i| column[i].clone()).collect());
    }

    let segments: Vec<Box<dyn Segment>> = vec![Box::new(InMemorySegment(filtered))];
    let rows = crate::vm::run_parallel(&segments, &plan.program)?;
    Ok(post_process(&plan.agg_parts, plan.num_group_keys, plan.order_by, plan.limit, rows))
}

/// Execute a query whose `SELECT` list contains one or more window functions
/// (`ROW_NUMBER`/`RANK`/`DENSE_RANK`, `LAG`/`LEAD`, `FIRST_VALUE`/
/// `LAST_VALUE`, `SUM`/`AVG`/`COUNT OVER`). Window functions need the whole
/// table materialized (partitioning and sorting happen entirely outside the
/// VM's register-machine model), so this bypasses `compile`/`Vm` and
/// computes each window column directly. `WHERE` and plain aggregates
/// aren't supported combined with window functions in this minimal
/// implementation -- only plain columns and window items in `SELECT`.
///
/// `LAST_VALUE`'s default frame (`RANGE UNBOUNDED PRECEDING .. CURRENT ROW`,
/// per the SQL standard when `ORDER BY` is present in `OVER`) makes it
/// return the *current* row's value, not the partition's true last row --
/// that's implemented literally here, ignoring `RANGE` peer-group ties.
pub fn execute_windowed(file: &ParquetFile, query: &Query) -> Result<Vec<Vec<Value>>> {
    let mut needed: Vec<String> = Vec::new();
    let push_needed = |name: &str, needed: &mut Vec<String>| {
        if !needed.iter().any(|n| n == name) {
            needed.push(name.to_string());
        }
    };
    for item in &query.columns {
        match item {
            SelectItem::Column(name) => push_needed(name, &mut needed),
            SelectItem::Window(spec) => {
                if let Some(arg) = &spec.arg {
                    push_needed(arg, &mut needed);
                }
                for p in &spec.partition_by {
                    push_needed(p, &mut needed);
                }
                for (o, _) in &spec.order_by {
                    push_needed(o, &mut needed);
                }
            }
            SelectItem::Agg(..) => {}
        }
    }

    let leaves = leaf_columns(file);
    let columns = resolve_columns(&leaves, &needed)?;
    let batch = read_whole_table(file, &columns);
    let num_rows = batch.num_rows;

    let window_outputs: Vec<Option<Vec<Value>>> =
        query.columns.iter().map(|item| if let SelectItem::Window(spec) = item { Some(compute_window(&batch, spec, num_rows)) } else { None }).collect();

    let mut rows: Vec<Vec<Value>> = (0..num_rows)
        .map(|row| {
            query
                .columns
                .iter()
                .enumerate()
                .map(|(i, item)| match item {
                    SelectItem::Column(name) => batch.columns[name][row].clone(),
                    SelectItem::Window(_) => window_outputs[i].as_ref().unwrap()[row].clone(),
                    SelectItem::Agg(..) => Value::Null,
                })
                .collect()
        })
        .collect();

    if let Some(OrderBy { column, descending }) = &query.order_by {
        if let Some(pos) = select_output_index(query, column) {
            rows.sort_by(|a, b| crate::vm::compare_for_order(&a[pos], &b[pos], *descending));
        }
    }
    if let Some(limit) = query.limit {
        rows.truncate(limit);
    }
    Ok(rows)
}

/// Compute one window function's value for every row (indexed by original
/// row order), partitioning by `spec.partition_by` and, within each
/// partition, sorting by `spec.order_by`.
fn compute_window(batch: &Batch, spec: &WindowSpec, num_rows: usize) -> Vec<Value> {
    let mut partitions: HashMap<String, Vec<usize>> = HashMap::new();
    let mut partition_order: Vec<String> = Vec::new();
    for row in 0..num_rows {
        let key = spec.partition_by.iter().map(|p| batch.columns[p][row].to_string()).collect::<Vec<_>>().join("\u{0}");
        if !partitions.contains_key(&key) {
            partition_order.push(key.clone());
        }
        partitions.entry(key).or_default().push(row);
    }

    let mut output = vec![Value::Null; num_rows];
    for key in &partition_order {
        let mut indices = partitions[key].clone();
        indices.sort_by(|&a, &b| {
            for (col, desc) in &spec.order_by {
                let ord = crate::vm::compare_for_order(&batch.columns[col][a], &batch.columns[col][b], *desc);
                if ord != std::cmp::Ordering::Equal {
                    return ord;
                }
            }
            std::cmp::Ordering::Equal
        });

        match spec.func {
            WindowFunc::RowNumber => {
                for (pos, &row) in indices.iter().enumerate() {
                    output[row] = Value::Int((pos + 1) as i64);
                }
            }
            WindowFunc::Rank | WindowFunc::DenseRank => {
                let mut rank = 0i64;
                let mut dense = 0i64;
                let mut prev: Option<usize> = None;
                for (pos, &row) in indices.iter().enumerate() {
                    let is_new = match prev {
                        None => true,
                        Some(prev_row) => spec.order_by.iter().any(|(col, _)| batch.columns[col][row].to_string() != batch.columns[col][prev_row].to_string()),
                    };
                    if is_new {
                        rank = (pos + 1) as i64;
                        dense += 1;
                    }
                    output[row] = Value::Int(if spec.func == WindowFunc::Rank { rank } else { dense });
                    prev = Some(row);
                }
            }
            WindowFunc::Lag | WindowFunc::Lead => {
                let offset = spec.offset.unwrap_or(1);
                let arg = spec.arg.as_ref().expect("LAG/LEAD always have an argument column");
                let n = indices.len() as i64;
                for (pos, &row) in indices.iter().enumerate() {
                    let target = if spec.func == WindowFunc::Lag { pos as i64 - offset } else { pos as i64 + offset };
                    output[row] = if target >= 0 && target < n { batch.columns[arg][indices[target as usize]].clone() } else { Value::Null };
                }
            }
            WindowFunc::FirstValue => {
                let arg = spec.arg.as_ref().expect("FIRST_VALUE always has an argument column");
                if let Some(&first) = indices.first() {
                    let v = batch.columns[arg][first].clone();
                    for &row in &indices {
                        output[row] = v.clone();
                    }
                }
            }
            WindowFunc::LastValue => {
                let arg = spec.arg.as_ref().expect("LAST_VALUE always has an argument column");
                for &row in &indices {
                    output[row] = batch.columns[arg][row].clone();
                }
            }
            WindowFunc::Sum | WindowFunc::Avg | WindowFunc::Count => {
                let arg = spec.arg.as_deref();
                if spec.order_by.is_empty() {
                    let agg = whole_partition_aggregate(spec.func, batch, arg, &indices);
                    for &row in &indices {
                        output[row] = agg.clone();
                    }
                } else {
                    let mut running_sum = 0.0;
                    let mut running_count = 0i64;
                    for &row in &indices {
                        let counted = match arg {
                            Some(a) => !matches!(batch.columns[a][row], Value::Null),
                            None => true,
                        };
                        if counted {
                            running_count += 1;
                            if let Some(a) = arg {
                                if let Some(v) = batch.columns[a][row].as_f64() {
                                    running_sum += v;
                                }
                            }
                        }
                        output[row] = match spec.func {
                            WindowFunc::Count => Value::Int(running_count),
                            WindowFunc::Sum => {
                                if running_count > 0 {
                                    Value::Float(running_sum)
                                } else {
                                    Value::Null
                                }
                            }
                            WindowFunc::Avg => {
                                if running_count > 0 {
                                    Value::Float(running_sum / running_count as f64)
                                } else {
                                    Value::Null
                                }
                            }
                            _ => unreachable!(),
                        };
                    }
                }
            }
        }
    }
    output
}

/// `SUM`/`AVG`/`COUNT OVER (PARTITION BY ... )` with no `ORDER BY`: the
/// default frame is the whole partition, so every row in it gets the same
/// aggregate value.
fn whole_partition_aggregate(func: WindowFunc, batch: &Batch, arg: Option<&str>, indices: &[usize]) -> Value {
    if func == WindowFunc::Count {
        let count = match arg {
            Some(a) => indices.iter().filter(|&&r| !matches!(batch.columns[a][r], Value::Null)).count(),
            None => indices.len(),
        };
        return Value::Int(count as i64);
    }
    let values: Vec<f64> = indices.iter().filter_map(|&r| arg.and_then(|a| batch.columns[a][r].as_f64())).collect();
    if values.is_empty() {
        return Value::Null;
    }
    match func {
        WindowFunc::Sum => Value::Float(values.iter().sum()),
        WindowFunc::Avg => Value::Float(values.iter().sum::<f64>() / values.len() as f64),
        _ => unreachable!(),
    }
}

fn select_output_index(query: &Query, column: &str) -> Option<usize> {
    query.columns.iter().position(|item| matches!(item, SelectItem::Column(name) if name == column))
}

/// Derive each `SELECT`-list item's output column header (e.g. `SUM(amount)`,
/// `ROW_NUMBER()`) -- shared by [`QueryEngine::execute`] and `codegen.rs`
/// (#98), which both need the same naming for a compiled query's results.
pub(crate) fn output_column_names(query: &Query) -> Vec<String> {
    query.columns.iter().map(select_item_label).collect()
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
        let table_name = name.unwrap_or_else(|| path.file_stem().and_then(|s| s.to_str()).unwrap_or("data").to_string());
        if self.tables.iter().any(|t| t.name == table_name) {
            return Err(QueryError::DuplicateTable(table_name));
        }
        // Skip first element (schema root).
        let column_names = file.metadata().schema.iter().skip(1).map(|e| e.name.clone()).collect();
        self.tables.push(Table { name: table_name, data, column_names });
        Ok(())
    }

    fn table_data(&self, name: &str) -> Result<&[u8]> {
        self.tables.iter().find(|t| t.name == name).map(|t| &t.data[..]).ok_or_else(|| QueryError::UnknownTable(name.to_string()))
    }

    /// Execute a SQL query and return results. Dispatches to a `JOIN`,
    /// `IN (SELECT ...)` semi-join, windowed, or plain single-table
    /// execution path depending on the parsed query's shape.
    pub fn execute(&self, sql: &str) -> Result<QueryResult> {
        let query = sql_parser::parse(sql).map_err(|e| QueryError::UnknownColumn(e.to_string()))?;
        let main_data = self.table_data(&query.from)?;
        let main_file = ParquetFile::open(main_data)?;

        let has_window = query.columns.iter().any(|c| matches!(c, SelectItem::Window(_)));
        let rows = if let Some(Expr::InSubquery { subquery, .. }) = &query.where_clause {
            let sub_data = self.table_data(&subquery.from)?;
            let sub_file = ParquetFile::open(sub_data)?;
            execute_semi_join(&main_file, &sub_file, &query)?
        } else if let Some(join) = query.joins.first() {
            let right_data = self.table_data(&join.table)?;
            let right_file = ParquetFile::open(right_data)?;
            execute_joined(&main_file, &right_file, &query)?
        } else if has_window {
            execute_windowed(&main_file, &query)?
        } else {
            execute(&main_file, &query)?
        };

        Ok(QueryResult { columns: output_column_names(&query), rows })
    }

    /// List loaded table names, in load order.
    pub fn table_names(&self) -> Vec<&str> {
        self.tables.iter().map(|t| t.name.as_str()).collect()
    }

    /// Get schema information: (table_name, columns), in load order.
    pub fn schemas(&self) -> Vec<(&str, Vec<String>)> {
        self.tables.iter().map(|t| (t.name.as_str(), t.column_names.clone())).collect()
    }

    /// Build a human-readable execution plan for `query` without running it
    /// (#99). Mirrors [`QueryEngine::execute`]'s dispatch (semi-join, join,
    /// windowed, or plain single-table) but only inspects file metadata and
    /// the compiled VM program instead of actually running it.
    pub fn explain(&self, query: &Query) -> Result<Vec<PlanNode>> {
        let main_data = self.table_data(&query.from)?;
        let main_file = ParquetFile::open(main_data)?;
        let mut b = PlanBuilder::new("QUERY PLAN");

        let has_window = query.columns.iter().any(|c| matches!(c, SelectItem::Window(_)));
        let is_semi_join = matches!(query.where_clause, Some(Expr::InSubquery { .. }));
        let join = query.joins.first();

        // Semi-joins compile with `where_clause` stripped, mirroring
        // `execute_semi_join` (the `IN` subquery isn't a VM predicate).
        let plan = if has_window {
            None
        } else if is_semi_join {
            let mut stripped = query.clone();
            stripped.where_clause = None;
            Some(compile(&stripped))
        } else {
            Some(compile(query))
        };

        let mut main_cols: Vec<String> = match (&plan, join) {
            (Some(p), Some(_)) => p.columns_to_load.iter().filter(|n| split_qualified(n).0.is_none_or(|t| t == query.from)).cloned().collect(),
            (Some(p), None) => p.columns_to_load.clone(),
            (None, _) => referenced_columns(query),
        };
        if let Some(j) = join {
            push_unique(&mut main_cols, j.left_col.clone());
        }
        if let Some(Expr::InSubquery { expr, .. }) = &query.where_clause {
            if let Expr::Column(col_name) = expr.as_ref() {
                push_unique(&mut main_cols, col_name.clone());
            }
        }
        let scan = b.push(0, scan_detail(&query.from, &main_file));
        if !main_cols.is_empty() {
            b.push(scan, format!("LOAD COLUMNS: {}", main_cols.join(", ")));
        }

        if is_semi_join {
            if let Some(Expr::InSubquery { expr, subquery }) = &query.where_clause {
                let sub_data = self.table_data(&subquery.from)?;
                let sub_file = ParquetFile::open(sub_data)?;
                let sub_scan = b.push(0, scan_detail(&subquery.from, &sub_file));
                let sub_cols = referenced_columns(subquery);
                if !sub_cols.is_empty() {
                    b.push(sub_scan, format!("LOAD COLUMNS: {}", sub_cols.join(", ")));
                }
                let sub_select: Vec<String> = subquery.columns.iter().map(select_item_label).collect();
                b.push(0, format!("SEMI JOIN: {} IN (SELECT {} FROM {})", expr_to_string(expr), sub_select.join(", "), subquery.from));
            }
        } else if let Some(join) = join {
            let right_data = self.table_data(&join.table)?;
            let right_file = ParquetFile::open(right_data)?;
            let mut right_cols: Vec<String> = plan
                .as_ref()
                .map(|p| p.columns_to_load.iter().filter(|n| split_qualified(n).0 == Some(join.table.as_str())).cloned().collect())
                .unwrap_or_default();
            push_unique(&mut right_cols, join.right_col.clone());
            let right_scan = b.push(0, scan_detail(&join.table, &right_file));
            if !right_cols.is_empty() {
                b.push(right_scan, format!("LOAD COLUMNS: {}", right_cols.join(", ")));
            }
            let kind = match join.kind {
                JoinKind::Inner => "HASH JOIN",
                JoinKind::Left => "LEFT HASH JOIN",
            };
            b.push(0, format!("{kind}: {} = {}", join.left_col, join.right_col));
        }

        if has_window {
            for item in &query.columns {
                if let SelectItem::Window(spec) = item {
                    b.push(0, format!("WINDOW: {}", window_detail(spec)));
                }
            }
        } else if let Some(plan) = &plan {
            if plan.program.iter().any(|op| matches!(op, Opcode::Filter { .. })) {
                if let Some(where_clause) = &query.where_clause {
                    b.push(0, format!("FILTER: {}", expr_to_string(where_clause)));
                }
            }
            if !query.group_by.is_empty() {
                let group_node = b.push(0, format!("GROUP BY: {}", query.group_by.join(", ")));
                for item in &query.columns {
                    if matches!(item, SelectItem::Agg(..)) {
                        b.push(group_node, format!("AGGREGATE: {}", select_item_label(item)));
                    }
                }
            } else {
                for item in &query.columns {
                    if matches!(item, SelectItem::Agg(..)) {
                        b.push(0, format!("AGGREGATE: {}", select_item_label(item)));
                    }
                }
            }
        }

        if let Some(OrderBy { column, descending }) = &query.order_by {
            b.push(0, format!("ORDER BY: {column}{}", if *descending { " DESC" } else { "" }));
        }
        if let Some(limit) = query.limit {
            b.push(0, format!("LIMIT: {limit}"));
        }

        let emit_labels: Vec<String> = query.columns.iter().map(select_item_label).collect();
        b.push(0, format!("EMIT: {}", emit_labels.join(", ")));

        Ok(b.finish())
    }
}

/// One node in an [`QueryEngine::explain`] plan tree: `parent == id` marks
/// the root.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanNode {
    pub id: u32,
    pub parent: u32,
    pub detail: String,
}

struct PlanBuilder {
    nodes: Vec<PlanNode>,
    next_id: u32,
}

impl PlanBuilder {
    fn new(root_detail: impl Into<String>) -> Self {
        PlanBuilder { nodes: vec![PlanNode { id: 0, parent: 0, detail: root_detail.into() }], next_id: 1 }
    }

    fn push(&mut self, parent: u32, detail: impl Into<String>) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        self.nodes.push(PlanNode { id, parent, detail: detail.into() });
        id
    }

    fn finish(self) -> Vec<PlanNode> {
        self.nodes
    }
}

fn scan_detail(table: &str, file: &ParquetFile) -> String {
    let groups = file.num_row_groups();
    format!("SCAN {table} ({groups} row group{}, ~{} rows)", if groups == 1 { "" } else { "s" }, file.num_rows())
}

fn agg_func_name(func: AggFunc) -> &'static str {
    match func {
        AggFunc::Count => "COUNT",
        AggFunc::Sum => "SUM",
        AggFunc::Avg => "AVG",
        AggFunc::Min => "MIN",
        AggFunc::Max => "MAX",
    }
}

fn window_func_name(func: WindowFunc) -> &'static str {
    match func {
        WindowFunc::RowNumber => "ROW_NUMBER",
        WindowFunc::Rank => "RANK",
        WindowFunc::DenseRank => "DENSE_RANK",
        WindowFunc::Lag => "LAG",
        WindowFunc::Lead => "LEAD",
        WindowFunc::FirstValue => "FIRST_VALUE",
        WindowFunc::LastValue => "LAST_VALUE",
        WindowFunc::Sum => "SUM",
        WindowFunc::Avg => "AVG",
        WindowFunc::Count => "COUNT",
    }
}

/// Output column label for one `SELECT` item, e.g. `amount`, `SUM(amount)`,
/// `COUNT(*)`, or `ROW_NUMBER()`.
fn select_item_label(item: &SelectItem) -> String {
    match item {
        SelectItem::Column(name) => name.clone(),
        SelectItem::Agg(func, arg) => match arg {
            Some(col) => format!("{}({col})", agg_func_name(*func)),
            None => format!("{}(*)", agg_func_name(*func)),
        },
        SelectItem::Window(spec) => format!("{}()", window_func_name(spec.func)),
    }
}

fn window_detail(spec: &WindowSpec) -> String {
    let mut detail = format!("{}({})", window_func_name(spec.func), spec.arg.as_deref().unwrap_or(""));
    let mut over = Vec::new();
    if !spec.partition_by.is_empty() {
        over.push(format!("PARTITION BY {}", spec.partition_by.join(", ")));
    }
    if !spec.order_by.is_empty() {
        let cols: Vec<String> =
            spec.order_by.iter().map(|(col, desc)| if *desc { format!("{col} DESC") } else { col.clone() }).collect();
        over.push(format!("ORDER BY {}", cols.join(", ")));
    }
    if !over.is_empty() {
        detail.push_str(" OVER (");
        detail.push_str(&over.join(" "));
        detail.push(')');
    }
    detail
}

fn literal_to_string(lit: &Literal) -> String {
    match lit {
        Literal::Int(v) => v.to_string(),
        Literal::Float(v) => v.to_string(),
        Literal::Str(v) => format!("'{v}'"),
    }
}

fn bin_op_str(op: BinOp) -> &'static str {
    match op {
        BinOp::Add => "+",
        BinOp::Sub => "-",
        BinOp::Mul => "*",
        BinOp::Div => "/",
        BinOp::Eq => "=",
        BinOp::Ne => "!=",
        BinOp::Lt => "<",
        BinOp::Le => "<=",
        BinOp::Gt => ">",
        BinOp::Ge => ">=",
        BinOp::And => "AND",
        BinOp::Or => "OR",
    }
}

/// Renders an `Expr` back to SQL-ish text for plan details, e.g.
/// `amount > 100`.
fn expr_to_string(expr: &Expr) -> String {
    match expr {
        Expr::Column(name) => name.clone(),
        Expr::Literal(lit) => literal_to_string(lit),
        Expr::BinaryOp(lhs, op, rhs) => format!("{} {} {}", expr_to_string(lhs), bin_op_str(*op), expr_to_string(rhs)),
        Expr::InSubquery { expr, subquery } => format!("{} IN (SELECT ... FROM {})", expr_to_string(expr), subquery.from),
    }
}

fn push_unique(out: &mut Vec<String>, name: String) {
    if !out.contains(&name) {
        out.push(name);
    }
}

fn collect_expr_columns(expr: &Expr, out: &mut Vec<String>) {
    match expr {
        Expr::Column(name) => push_unique(out, name.clone()),
        Expr::Literal(_) => {}
        Expr::BinaryOp(lhs, _, rhs) => {
            collect_expr_columns(lhs, out);
            collect_expr_columns(rhs, out);
        }
        Expr::InSubquery { expr, .. } => collect_expr_columns(expr, out),
    }
}

/// Every column name `query` references, in first-seen order (used for the
/// `EXPLAIN` `LOAD COLUMNS` detail on paths that don't go through
/// [`compile`], namely windowed queries).
fn referenced_columns(query: &Query) -> Vec<String> {
    let mut out = Vec::new();
    for name in &query.group_by {
        push_unique(&mut out, name.clone());
    }
    for item in &query.columns {
        match item {
            SelectItem::Column(name) => push_unique(&mut out, name.clone()),
            SelectItem::Agg(_, Some(name)) => push_unique(&mut out, name.clone()),
            SelectItem::Agg(_, None) => {}
            SelectItem::Window(spec) => {
                if let Some(arg) = &spec.arg {
                    push_unique(&mut out, arg.clone());
                }
                for name in &spec.partition_by {
                    push_unique(&mut out, name.clone());
                }
                for (name, _) in &spec.order_by {
                    push_unique(&mut out, name.clone());
                }
            }
        }
    }
    if let Some(where_clause) = &query.where_clause {
        collect_expr_columns(where_clause, &mut out);
    }
    for join in &query.joins {
        push_unique(&mut out, join.left_col.clone());
        push_unique(&mut out, join.right_col.clone());
    }
    if let Some(order_by) = &query.order_by {
        push_unique(&mut out, order_by.column.clone());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use sql_parser as sql;

    #[test]
    fn bounded_scan_limit_accepts_bare_limit() {
        let query = sql::parse("SELECT id FROM t LIMIT 10").unwrap();
        assert_eq!(bounded_scan_limit(&query), Some(10));
    }

    #[test]
    fn bounded_scan_limit_rejects_where_order_by_group_by_and_aggregates() {
        assert_eq!(bounded_scan_limit(&sql::parse("SELECT id FROM t WHERE id > 1 LIMIT 10").unwrap()), None);
        assert_eq!(bounded_scan_limit(&sql::parse("SELECT id FROM t ORDER BY id LIMIT 10").unwrap()), None);
        assert_eq!(bounded_scan_limit(&sql::parse("SELECT id, SUM(amount) FROM t GROUP BY id LIMIT 10").unwrap()), None);
        assert_eq!(bounded_scan_limit(&sql::parse("SELECT COUNT(*) FROM t LIMIT 10").unwrap()), None);
        assert_eq!(bounded_scan_limit(&sql::parse("SELECT id FROM t").unwrap()), None);
    }

    #[test]
    fn compile_where_and_group_by_builds_expected_program_shape() {
        let query = sql::parse("SELECT region, SUM(amount) FROM t WHERE amount > 10 GROUP BY region").unwrap();
        let plan = compile(&query);
        assert!(plan.columns_to_load.contains(&"region".to_string()));
        assert!(plan.columns_to_load.contains(&"amount".to_string()));
        assert!(matches!(plan.program.last(), Some(Opcode::Emit { .. })));
        assert!(plan.program.iter().any(|op| matches!(op, Opcode::GroupReduce { .. })));
        assert!(plan.program.iter().any(|op| matches!(op, Opcode::Filter { .. })));
    }

    fn fixture_path(name: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
    }

    fn details(nodes: &[PlanNode]) -> Vec<&str> {
        nodes.iter().map(|n| n.detail.as_str()).collect()
    }

    #[test]
    fn explain_plain_filter_group_by_aggregate() {
        let mut engine = QueryEngine::default();
        engine.add_table(&fixture_path("production.parquet"), None).unwrap();
        let query = sql::parse("SELECT region, SUM(amount), COUNT(*) FROM production WHERE id > 1000 GROUP BY region ORDER BY region").unwrap();
        let nodes = engine.explain(&query).unwrap();

        assert_eq!(nodes[0].detail, "QUERY PLAN");
        assert!(nodes[0].parent == nodes[0].id, "root's parent must equal its own id");
        assert!(details(&nodes).iter().any(|d| d.starts_with("SCAN production (") && d.ends_with("row groups, ~5000 rows)")));
        assert!(details(&nodes).contains(&"LOAD COLUMNS: region, amount, id"));
        assert!(details(&nodes).contains(&"FILTER: id > 1000"));
        assert!(details(&nodes).contains(&"GROUP BY: region"));
        assert!(details(&nodes).contains(&"AGGREGATE: SUM(amount)"));
        assert!(details(&nodes).contains(&"AGGREGATE: COUNT(*)"));
        assert!(details(&nodes).contains(&"ORDER BY: region"));
        assert_eq!(nodes.last().unwrap().detail, "EMIT: region, SUM(amount), COUNT(*)");

        // AGGREGATE nodes must nest under the GROUP BY node, not the root.
        let group_id = nodes.iter().find(|n| n.detail == "GROUP BY: region").unwrap().id;
        let agg_parents: Vec<u32> = nodes.iter().filter(|n| n.detail.starts_with("AGGREGATE")).map(|n| n.parent).collect();
        assert_eq!(agg_parents, vec![group_id, group_id]);
    }

    #[test]
    fn explain_limit_without_group_by() {
        let mut engine = QueryEngine::default();
        engine.add_table(&fixture_path("orders.parquet"), None).unwrap();
        let query = sql::parse("SELECT id, region_key FROM orders LIMIT 3").unwrap();
        let nodes = engine.explain(&query).unwrap();
        assert!(details(&nodes).contains(&"LIMIT: 3"));
        assert!(!details(&nodes).iter().any(|d| d.starts_with("FILTER") || d.starts_with("GROUP BY")));
    }

    #[test]
    fn explain_join_describes_both_scans_and_condition() {
        let mut engine = QueryEngine::default();
        engine.add_table(&fixture_path("orders.parquet"), None).unwrap();
        engine.add_table(&fixture_path("regions.parquet"), None).unwrap();
        let query =
            sql::parse("SELECT orders.id, regions.budget FROM orders JOIN regions ON orders.region_key = regions.key ORDER BY orders.id")
                .unwrap();
        let nodes = engine.explain(&query).unwrap();

        assert!(details(&nodes).iter().any(|d| d.starts_with("SCAN orders (")));
        assert!(details(&nodes).iter().any(|d| d.starts_with("SCAN regions (")));
        assert!(details(&nodes).contains(&"LOAD COLUMNS: orders.id, orders.region_key"));
        assert!(details(&nodes).contains(&"LOAD COLUMNS: regions.budget, regions.key"));
        assert!(details(&nodes).contains(&"HASH JOIN: orders.region_key = regions.key"));
    }

    #[test]
    fn explain_semi_join_describes_subquery() {
        let mut engine = QueryEngine::default();
        engine.add_table(&fixture_path("orders.parquet"), None).unwrap();
        engine.add_table(&fixture_path("regions.parquet"), None).unwrap();
        let query = sql::parse("SELECT id FROM orders WHERE region_key IN (SELECT key FROM regions) ORDER BY id").unwrap();
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
        engine.add_table(&fixture_path("orders.parquet"), None).unwrap();
        let query = sql::parse("SELECT id, region_key, ROW_NUMBER() OVER (PARTITION BY region_key ORDER BY id) FROM orders ORDER BY id").unwrap();
        let nodes = engine.explain(&query).unwrap();

        assert!(details(&nodes).contains(&"WINDOW: ROW_NUMBER() OVER (PARTITION BY region_key ORDER BY id)"));
        assert_eq!(nodes.last().unwrap().detail, "EMIT: id, region_key, ROW_NUMBER()");
    }

    #[test]
    fn parse_explain_strips_prefix_and_query_plan_variant() {
        let (explain, query) = sql::parse_explain("EXPLAIN SELECT id FROM orders").unwrap();
        assert!(explain);
        assert_eq!(query.from, "orders");

        let (explain, _) = sql::parse_explain("EXPLAIN QUERY PLAN SELECT id FROM orders").unwrap();
        assert!(explain);

        let (explain, _) = sql::parse_explain("SELECT id FROM orders").unwrap();
        assert!(!explain);
    }
}
