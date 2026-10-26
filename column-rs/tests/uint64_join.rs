//! A `UINT_64` join key above `i64::MAX` (db-core#786): both sides of a
//! join read it as its signed bit pattern, so equal keys match, while a
//! `UINT_64` read outside a join key is still an error (db-core#595).

use column_rs::file::ParquetFile;
use column_rs::query::{self, QueryEngine};
use column_rs::sql;
use db_core::storage::column::parquet::schema::{ColumnSpec, ColumnType, ColumnValues};
use db_core::storage::column::parquet::writer::{ParquetWriter, WriterOptions};
use db_core::storage::ingest::{Ack, RecordValue, TableOptions, TableWriter};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

/// Session ids on both sides of 2^63.
const KEYS: [u64; 4] = [5, i64::MAX as u64, 1 << 63, u64::MAX];
const LABELS: [&str; 4] = ["five", "max_i64", "two63", "max_u64"];

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("column-rs-u64-{}-{n}", std::process::id()));
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

fn parquet(columns: Vec<(ColumnSpec, ColumnValues)>) -> Vec<u8> {
    let (specs, values): (Vec<_>, Vec<_>) = columns.into_iter().unzip();
    let mut w = ParquetWriter::new(Vec::new(), specs, WriterOptions::default()).unwrap();
    w.write_columns(values).unwrap();
    w.finish().unwrap().0
}

/// The `sess` file (`sid`, `n`) and the `names` file (`sid`, `label`).
fn files() -> (Vec<u8>, Vec<u8>) {
    let sess = parquet(vec![
        (
            ColumnSpec::new("sid", ColumnType::UInt64, false),
            ColumnValues::UInt64(KEYS.iter().map(|k| Some(*k)).collect()),
        ),
        (
            ColumnSpec::new("n", ColumnType::Int64, false),
            ColumnValues::Int64((0..4).map(Some).collect()),
        ),
    ]);
    let names = parquet(vec![
        (
            ColumnSpec::new("sid", ColumnType::UInt64, false),
            ColumnValues::UInt64(KEYS.iter().map(|k| Some(*k)).collect()),
        ),
        (
            ColumnSpec::new("label", ColumnType::Utf8, false),
            ColumnValues::Utf8(LABELS.iter().map(|l| Some((*l).to_string())).collect()),
        ),
    ]);
    (sess, names)
}

#[test]
fn a_file_join_on_a_uint64_key_above_i64_max_matches_on_both_sides() {
    let (sess, names) = files();
    let sess = ParquetFile::open(&sess).unwrap();
    let names = ParquetFile::open(&names).unwrap();
    let parsed = sql::parse(
        "SELECT names.label FROM sess JOIN names ON sess.sid = names.sid ORDER BY sess.n",
    )
    .unwrap();
    let rows = query::execute_joined(&sess, &names, &parsed)
        .unwrap()
        .into_rows();
    let got: Vec<String> = rows.iter().map(|r| r[0].to_string()).collect();
    assert_eq!(got, LABELS);

    // Outside a join key the value is still an error (db-core#595).
    let plain = sql::parse("SELECT sid FROM sess").unwrap();
    assert!(query::execute(&sess, &plain).is_err());
}

fn table_dir(root: &TempDir, name: &str, schema: Vec<ColumnSpec>, rows: Vec<Vec<RecordValue>>) {
    let mut w = TableWriter::open(
        &root.0.join(name),
        TableOptions {
            schema: Some(schema),
            ..TableOptions::default()
        },
    )
    .unwrap();
    let flushed = rows.len() / 2;
    for (i, row) in rows.into_iter().enumerate() {
        w.append_row(row, Ack::Durable).unwrap();
        if i + 1 == flushed {
            w.flush_all().unwrap(); // the first half in a row group, the rest in a page
        }
    }
    w.close().unwrap();
}

#[test]
fn a_table_directory_join_on_a_uint64_key_above_i64_max_matches_on_both_sides() {
    let root = TempDir::new();
    table_dir(
        &root,
        "sess",
        vec![
            ColumnSpec::new("sid", ColumnType::UInt64, false),
            ColumnSpec::new("n", ColumnType::Int64, false),
        ],
        KEYS.iter()
            .zip(0..)
            .map(|(k, n)| vec![RecordValue::UInt(*k), RecordValue::Int(n)])
            .collect(),
    );
    table_dir(
        &root,
        "names",
        vec![
            ColumnSpec::new("sid", ColumnType::UInt64, false),
            ColumnSpec::new("label", ColumnType::Utf8, false),
        ],
        KEYS.iter()
            .zip(LABELS)
            .map(|(k, l)| vec![RecordValue::UInt(*k), RecordValue::Str(l.to_string())])
            .collect(),
    );
    let engine = QueryEngine::open_many(&[root.0.join("sess"), root.0.join("names")]).unwrap();
    let result = engine
        .execute("SELECT names.label FROM sess JOIN names ON sess.sid = names.sid ORDER BY sess.n")
        .unwrap();
    let got: Vec<String> = result.rows().map(|r| r[0].to_string()).collect();
    assert_eq!(got, LABELS);

    assert!(engine.execute("SELECT sid FROM sess").is_err());
}
