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
use column_rs::vm::Chunk;

/// Writes one already-materialized [`Chunk`] (tab-separated, one line per
/// row) to `out`. Shared by [`print_result_streaming`] and db-core#456's
/// `-c` streaming path (`main::run_query`), which calls this once per
/// chunk as `Database::execute_streaming` produces them, instead of
/// collecting a whole [`QueryResult`] first.
pub fn print_chunk(out: &mut impl Write, chunk: &Chunk) {
    let rows = chunk.first().map_or(0, |column| column.len());
    for r in 0..rows {
        for (i, column) in chunk.iter().enumerate() {
            if i > 0 {
                let _ = out.write_all(b"\t");
            }
            if let Some(value) = column.get(r) {
                let _ = write!(out, "{value}");
            }
        }
        let _ = out.write_all(b"\n");
    }
}

pub fn print_result_streaming(result: &QueryResult) {
    let stdout = io::stdout();
    let mut out = BufWriter::with_capacity(256 * 1024, stdout.lock());
    let _ = writeln!(out, "{}", result.columns.join("\t"));
    for chunk in result.output.chunks() {
        print_chunk(&mut out, chunk);
    }
}
