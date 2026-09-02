//! #110: tab-separated output, one row written as soon as it's formatted --
//! no box-drawing, and critically no pre-pass over the whole result to
//! compute column widths -- `-c` one-shot mode uses this instead of
//! `db_cli::render`'s box table, which needs the entire result collected
//! before it can print even the first line.
//! `BufWriter` matters here -- one `write!` per line through an unbuffered
//! stdout lock is itself real overhead at millions of rows.

use std::io::{self, BufWriter, Write};

use column_rs::vm::Value;

pub fn print_rows_streaming(headers: &[String], rows: impl Iterator<Item = Vec<Value>>) {
    let stdout = io::stdout();
    let mut out = BufWriter::with_capacity(256 * 1024, stdout.lock());
    let _ = writeln!(out, "{}", headers.join("\t"));
    for row in rows {
        for (i, value) in row.iter().enumerate() {
            if i > 0 {
                let _ = out.write_all(b"\t");
            }
            let _ = write!(out, "{value}");
        }
        let _ = out.write_all(b"\n");
    }
}
