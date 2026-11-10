//! #110: tab-separated output, one row written as soon as it's formatted --
//! no box-drawing, and critically no pre-pass over the whole result to
//! compute column widths -- `-c` one-shot mode uses this instead of
//! `db_cli::render`'s box table, which needs the entire result collected
//! before it can print even the first line.
//! `BufWriter` matters here -- one `write!` per line through an unbuffered
//! stdout lock is itself real overhead at millions of rows.
//!
//! db-core#436: the result arrives as column-major chunks, so this walks
//! each chunk cell by cell **by reference** -- no row `Vec`, no `Value`
//! clone, per printed row. Asking the result for rows instead
//! (`QueryResult::rows`) would allocate one `Vec` per row on this single
//! thread, and at 5M rows that alone measured +15% wall-clock over the
//! old row-major output (whose per-row `Vec`s were at least built in
//! parallel inside `Emit`).

use std::io::{self, BufWriter, Write};

use column_rs::query::QueryResult;
use column_rs::vm::{Chunk, Value};
use db_core::engine::column::render_timestamp;

/// Formats a single cell straight into `out`, using `itoa`/`ryu` for the
/// numeric variants instead of going through `Value`'s `Display` impl.
/// db-core#473: those two variants dominate `filter_50pct`'s 20M-cell
/// output and `core::fmt`'s general float path is the largest single cost.
/// `ryu::Buffer::format_finite` produces the same shortest round-trip
/// digits as `f64`'s own `Display`, so output is byte-identical.
fn write_value(out: &mut impl Write, value: &Value) {
    match value {
        Value::Int(v) => {
            let mut buf = itoa::Buffer::new();
            let _ = out.write_all(buf.format(*v).as_bytes());
        }
        Value::Float(v) => {
            let mut buf = ryu::Buffer::new();
            let _ = out.write_all(buf.format_finite(*v).as_bytes());
        }
        Value::Bool(v) => {
            let _ = write!(out, "{v}");
        }
        Value::Str(v) => {
            let _ = out.write_all(v.as_bytes());
        }
        // db-core#868: a blob as `x'AB..'`, db-core's `Display`.
        Value::Blob(_) => {
            let _ = write!(out, "{value}");
        }
        // db-core#621: a MAP value, printed as DuckDB prints it ({k=v, ...});
        // db-core#632: a DECIMAL at its scale (12.30); db-core#801: a LIST
        // as a JSON array (["a",null]).
        Value::Map(_) | Value::List(_) | Value::Decimal { .. } => {
            let _ = write!(out, "{value}");
        }
        // `NULL`, and any value with no printed form of its own.
        _ => {
            let _ = out.write_all(b"NULL");
        }
    }
}

/// Writes one already-materialized [`Chunk`] (tab-separated, one line per
/// row) to `out`. Shared by [`print_result_streaming`] and db-core#456's
/// `-c` streaming path (`main::run_query`), which calls this once per
/// chunk as `Database::execute_streaming` produces them, instead of
/// collecting a whole [`QueryResult`] first. A column with units in
/// `timestamps` ([`QueryResult::timestamps`]) prints as ISO 8601.
pub fn print_chunk(out: &mut impl Write, chunk: &Chunk, timestamps: &[Option<i64>]) {
    let rows = chunk.first().map_or(0, |column| column.len());
    for r in 0..rows {
        for (i, column) in chunk.iter().enumerate() {
            if i > 0 {
                let _ = out.write_all(b"\t");
            }
            match (column.get(r), timestamps.get(i)) {
                (Some(Value::Int(v)), Some(Some(per_second))) => {
                    match render_timestamp(*v, *per_second) {
                        Some(iso) => {
                            let _ = out.write_all(iso.as_bytes());
                        }
                        None => write_value(out, &Value::Int(*v)),
                    }
                }
                (Some(value), _) => write_value(out, value),
                (None, _) => {}
            }
        }
        let _ = out.write_all(b"\n");
    }
}

pub fn print_result_streaming(result: &QueryResult) {
    let stdout = io::stdout();
    let mut out = BufWriter::with_capacity(4 * 1024 * 1024, stdout.lock());
    let _ = writeln!(out, "{}", result.columns.join("\t"));
    for chunk in result.output.chunks() {
        print_chunk(&mut out, chunk, &result.timestamps);
    }
}
