//! `column-rs --pipe` (tools#39): one process answering JSON Lines
//! requests with typed JSON Lines results.

use db_core::storage::ingest::{Ack, RecordValue, TableOptions, TableWriter};
use db_core::storage::parquet::schema::{ColumnSpec, ColumnType};
use serde_json::{json, Value as Json};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("column-rs-pipe-{}-{n}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

/// A running `column-rs --pipe`.
struct Pipe {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
}

impl Pipe {
    fn start(tables: &[&Path]) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_column-rs"))
            .arg("--pipe")
            .args(tables)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Pipe {
            child,
            stdin,
            stdout,
        }
    }

    fn send(&mut self, line: &str) {
        let stdin = self.stdin.as_mut().unwrap();
        writeln!(stdin, "{line}").unwrap();
        stdin.flush().unwrap();
    }

    fn read(&mut self) -> Json {
        let mut line = String::new();
        assert!(self.stdout.read_line(&mut line).unwrap() > 0, "pipe closed");
        serde_json::from_str(&line).unwrap()
    }

    /// Sends `sql` as request `id`; returns (header, rows, done) or the
    /// error object.
    fn query(&mut self, id: Json, sql: &str) -> Result<(Json, Vec<Json>, Json), Json> {
        self.send(&json!({"id": id, "sql": sql}).to_string());
        let first = self.read();
        assert_eq!(first["id"], id, "{first}");
        if first.get("error").is_some() {
            return Err(first);
        }
        let mut rows = Vec::new();
        loop {
            let line = self.read();
            assert_eq!(line["id"], id, "{line}");
            if line.get("done").is_some() {
                return Ok((first, rows, line));
            }
            rows.push(line["row"].clone());
        }
    }

    /// Closes stdin and waits: EOF exits cleanly.
    fn finish(mut self) {
        drop(self.stdin.take());
        assert!(self.child.wait().unwrap().success());
    }
}

fn lines_table(dir: &Path, n: usize) -> TableWriter {
    let mut w = TableWriter::open(dir, TableOptions::default()).unwrap();
    for i in 0..n {
        w.append(
            format!(r#"{{"i":{i},"k":"k{}"}}"#, i % 3).as_bytes(),
            Ack::Durable,
        )
        .unwrap();
    }
    w
}

#[test]
fn a_thousand_requests_in_one_process() {
    let t = TempDir::new();
    let dir = t.0.join("events");
    let mut w = lines_table(&dir, 10);
    w.flush_all().unwrap();
    w.close().unwrap();
    let mut pipe = Pipe::start(&[&dir]);
    for id in 0..1000 {
        let (header, rows, done) = pipe
            .query(
                json!(id),
                "SELECT k, COUNT(*) FROM events GROUP BY k ORDER BY k",
            )
            .unwrap();
        assert_eq!(
            header["columns"][0],
            json!({"name": "k", "type": "VARCHAR"})
        );
        assert_eq!(rows, [json!(["k0", 4]), json!(["k1", 3]), json!(["k2", 3])]);
        assert_eq!(done["done"], json!(true));
        assert_eq!(done["rows"], json!(3));
        assert!(done["elapsed_ms"].is_u64());
    }
    pipe.finish();
}

#[test]
fn a_bad_request_is_answered_and_the_next_one_works() {
    let t = TempDir::new();
    let dir = t.0.join("events");
    lines_table(&dir, 3).close().unwrap();
    let mut pipe = Pipe::start(&[&dir]);
    let err = pipe
        .query(json!("a"), "SELECT nope FROM events")
        .unwrap_err();
    assert_eq!(err["error"]["kind"], json!("unknown_column"));
    let err = pipe
        .query(json!("b"), "SELECT * FROM elsewhere")
        .unwrap_err();
    assert_eq!(err["error"]["kind"], json!("unknown_table"));
    pipe.send("this is not json");
    assert_eq!(pipe.read()["error"]["kind"], json!("request"));
    pipe.send(r#"{"id": 9}"#);
    let missing = pipe.read();
    assert_eq!(
        (missing["id"].clone(), missing["error"]["kind"].clone()),
        (json!(9), json!("request"))
    );
    let (_, rows, _) = pipe
        .query(json!("c"), "SELECT COUNT(*) FROM events")
        .unwrap();
    assert_eq!(rows, [json!([3])]);
    pipe.finish();
}

/// tools#74: SQL that doesn't parse is `parse`, a function the column
/// dialect lacks is `unsupported`, neither one `unknown_column`; and a
/// query over SQLite tables alone takes sqlite3's functions.
#[test]
fn parse_errors_have_their_own_kind() {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let mut pipe = Pipe::start(&[
        &fixtures.join("production.parquet"),
        &fixtures.join("lookups.db"),
    ]);
    let err = pipe.query(json!(1), "SELEC id FROM labels").unwrap_err();
    assert_eq!(err["error"]["kind"], json!("parse"), "{err}");
    let err = pipe
        .query(json!(2), "SELECT UPPER(region) FROM production")
        .unwrap_err();
    assert_eq!(err["error"]["kind"], json!("unsupported"), "{err}");
    let message = err["error"]["message"].as_str().unwrap();
    assert!(!message.starts_with("unknown column"), "{message}");
    let (_, rows, _) = pipe
        .query(json!(3), "SELECT UPPER(label) FROM labels WHERE id = 1")
        .unwrap();
    assert_eq!(rows, [json!(["ONE"])]);
    pipe.finish();
}

#[test]
fn values_are_encoded_by_their_column_type() {
    let t = TempDir::new();
    let dir = t.0.join("typed");
    let schema = vec![
        ColumnSpec::new("id", ColumnType::UInt64, false),
        ColumnSpec::new(
            "amount",
            ColumnType::Decimal {
                precision: 12,
                scale: 2,
            },
            false,
        ),
        ColumnSpec::new("payload", ColumnType::Binary, true),
        ColumnSpec::new("at", ColumnType::TimestampMicros, false),
        ColumnSpec::new("note", ColumnType::Utf8, true),
    ];
    let options = TableOptions {
        schema: Some(schema),
        ..TableOptions::default()
    };
    let mut w = TableWriter::open(&dir, options).unwrap();
    let row = |id: u64, cents: i128, payload: Option<Vec<u8>>, at: i64, note: Option<&str>| {
        vec![
            RecordValue::UInt(id),
            RecordValue::Decimal(cents),
            payload.map_or(RecordValue::Null, RecordValue::Bytes),
            RecordValue::Int(at),
            note.map_or(RecordValue::Null, |n| RecordValue::Str(n.to_string())),
        ]
    };
    w.append_row(
        row(
            (1 << 53) + 1,
            123_456,
            Some(vec![0x80, 0xff, 0]),
            1_791_676_800_000_001,
            None,
        ),
        Ack::Durable,
    )
    .unwrap();
    w.append_row(row(7, -1, None, 0, Some("x")), Ack::Durable)
        .unwrap();
    let expected = [
        json!([
            "9007199254740993",
            "1234.56",
            "gP8A",
            "2026-10-11T00:00:00.000001Z",
            null
        ]),
        json!([7, "-0.01", null, "1970-01-01T00:00:00Z", "x"]),
    ];
    let sql = "SELECT id, amount, payload, at, note FROM typed ORDER BY at DESC";
    // The same answer from WAL pages and, after a flush, from Parquet.
    for flushed in [false, true] {
        if flushed {
            w.flush_all().unwrap();
        }
        let mut pipe = Pipe::start(&[&dir]);
        let (header, rows, _) = pipe.query(json!(1), sql).unwrap();
        let types: Vec<Json> = header["columns"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["type"].clone())
            .collect();
        assert_eq!(
            types,
            [
                json!("UBIGINT"),
                json!("DECIMAL(12,2)"),
                json!("BLOB"),
                json!("TIMESTAMP"),
                json!("VARCHAR")
            ],
            "flushed: {flushed}"
        );
        assert_eq!(rows, expected, "flushed: {flushed}");
        pipe.finish();
    }
    w.close().unwrap();
}

#[test]
fn each_request_sees_what_was_acknowledged_before_it() {
    let t = TempDir::new();
    let dir = t.0.join("growing");
    let mut w = lines_table(&dir, 1);
    let mut pipe = Pipe::start(&[&dir]);
    for n in 1..=20 {
        let (_, rows, _) = pipe
            .query(json!(n), "SELECT COUNT(*) FROM growing")
            .unwrap();
        assert_eq!(rows, [json!([n])]);
        w.append(format!(r#"{{"i":{n}}}"#).as_bytes(), Ack::Durable)
            .unwrap();
        if n % 7 == 0 {
            w.flush_all().unwrap();
        }
    }
    pipe.finish();
    w.close().unwrap();
}
