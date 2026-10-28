//! Table directories (tools#13): a directory written by db-core's ingest
//! (or `column-ingest`) is a table, named after the directory. Queries run
//! over its Parquet files and the records still in its WAL; joins, window
//! functions and semi-joins work as over a file.
//!
//! The oracle tests compact every record into `archive/` first, so DuckDB
//! over `archive/*/*.parquet` sees exactly the rows column-rs does.

use column_rs::query::QueryEngine;
use db_core::storage::ingest::{Ack, Layout, TableOptions, TableWriter, TierOptions};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};

const HOUR_NS: i64 = 3_600_000_000_000;
const DAY_NS: i64 = 24 * HOUR_NS;

/// The tables' clock: shared, so tests that move it hold [`serial`].
static NOW: AtomicI64 = AtomicI64::new(0);
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(PoisonError::into_inner)
}

fn clock() -> i64 {
    NOW.load(Ordering::SeqCst)
}

/// 2026-10-11 plus `hours`.
fn set_clock(hours: i64) {
    NOW.store(20_737 * DAY_NS + hours * HOUR_NS, Ordering::SeqCst);
}

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("column-rs-tabledir-{}-{n}", std::process::id()));
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

fn tiered() -> TableOptions {
    TableOptions {
        layout: Layout::Tiered(TierOptions {
            row_group_max_rows: 7,
            delete_grace_ns: 0,
            ..TierOptions::default()
        }),
        clock,
        ..TableOptions::default()
    }
}

/// A tiered table at `dir` holding `lines`, one an hour from day 0, every
/// day closed and merged into `archive/`.
fn archived_table(dir: &Path, lines: &[String]) {
    let mut w = TableWriter::open(dir, tiered()).unwrap();
    for (hour, line) in lines.iter().enumerate() {
        set_clock(hour as i64);
        w.append(line.as_bytes(), Ack::Durable).unwrap();
    }
    set_clock(lines.len() as i64 + 48);
    w.compact().unwrap();
    w.compact().unwrap(); // deletes the merged hours
    let areas: Vec<&str> = w.manifest().files.iter().map(|f| f.area.as_str()).collect();
    assert!(areas.iter().all(|a| *a == "archive"), "{areas:?}");
    assert!(w.snapshot().unwrap().pages().is_empty());
    w.close().unwrap();
}

fn events(n: usize) -> Vec<String> {
    (0..n)
        .map(|i| {
            let kind = ["view", "buy", "exit"][i % 3];
            format!(r#"{{"user":"u{}","kind":"{kind}","n":{i}}}"#, i % 5)
        })
        .collect()
}

fn users() -> Vec<String> {
    ["gold", "silver", "gold", "bronze"]
        .iter()
        .enumerate()
        .map(|(i, tier)| format!(r#"{{"user":"u{i}","tier":"{tier}"}}"#))
        .collect()
}

/// `sql` through column-rs, each row's cells as text.
fn ours(engine: &QueryEngine, sql: &str) -> Vec<Vec<String>> {
    let result = engine.execute(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    result
        .rows()
        .map(|row| row.iter().map(ToString::to_string).collect())
        .collect()
}

/// `sql` through DuckDB, `None` if it is not installed.
fn duckdb(sql: &str) -> Option<Vec<Vec<String>>> {
    let output = Command::new("duckdb")
        .args(["-csv", "-c", sql])
        .output()
        .ok()?;
    assert!(
        output.status.success(),
        "duckdb: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout).unwrap();
    Some(
        text.lines()
            .skip(1)
            .map(|line| line.split(',').map(str::to_string).collect())
            .collect(),
    )
}

/// DuckDB's view of a table directory's archive.
fn archive(dir: &Path) -> String {
    format!(
        "read_parquet('{}/archive/*/*.parquet', hive_partitioning = false, union_by_name = true)",
        dir.display()
    )
}

/// Asserts column-rs over the table directories and DuckDB over their
/// archives agree; `tables` replaces each `{name}` in `duck_sql`.
fn agree(engine: &QueryEngine, sql: &str, duck_sql: &str) {
    let Some(expected) = duckdb(duck_sql) else {
        eprintln!("duckdb not found on PATH; skipping oracle comparison");
        return;
    };
    assert!(
        !expected.is_empty(),
        "an empty oracle proves nothing: {duck_sql}"
    );
    assert_eq!(ours(engine, sql), expected, "{sql}");
}

#[test]
fn oracle_aggregates_over_a_table_directory() {
    let _serial = serial();
    let t = TempDir::new();
    let dir = t.0.join("events");
    archived_table(&dir, &events(60));
    let engine = QueryEngine::open(&dir).unwrap();
    agree(
        &engine,
        "SELECT kind, COUNT(*), MIN(n), MAX(n) FROM events GROUP BY kind ORDER BY kind",
        &format!(
            "SELECT kind, COUNT(*), MIN(n), MAX(n) FROM {} GROUP BY kind ORDER BY kind",
            archive(&dir)
        ),
    );
    agree(
        &engine,
        "SELECT user, n FROM events WHERE n > 50 ORDER BY n",
        &format!(
            "SELECT user, n FROM {} WHERE n > 50 ORDER BY n",
            archive(&dir)
        ),
    );
}

#[test]
fn oracle_a_join_between_two_table_directories() {
    let _serial = serial();
    let t = TempDir::new();
    let (ev, us) = (t.0.join("events"), t.0.join("users"));
    archived_table(&ev, &events(60));
    archived_table(&us, &users());
    let engine = QueryEngine::open_many(&[ev.clone(), us.clone()]).unwrap();
    agree(
        &engine,
        "SELECT users.tier, COUNT(*) FROM events JOIN users ON events.user = users.user \
         GROUP BY users.tier ORDER BY users.tier",
        &format!(
            "SELECT users.tier, COUNT(*) FROM {} events JOIN {} users ON events.user = users.user \
             GROUP BY users.tier ORDER BY users.tier",
            archive(&ev),
            archive(&us)
        ),
    );
}

#[test]
fn oracle_a_window_function_over_a_table_directory() {
    let _serial = serial();
    let t = TempDir::new();
    let dir = t.0.join("events");
    archived_table(&dir, &events(30));
    let engine = QueryEngine::open(&dir).unwrap();
    agree(
        &engine,
        "SELECT n, ROW_NUMBER() OVER (PARTITION BY user ORDER BY n) FROM events ORDER BY n",
        &format!(
            "SELECT n, ROW_NUMBER() OVER (PARTITION BY user ORDER BY n) FROM {} ORDER BY n",
            archive(&dir)
        ),
    );
}

#[test]
fn oracle_a_semi_join_into_a_table_directory() {
    let _serial = serial();
    let t = TempDir::new();
    let (ev, us) = (t.0.join("events"), t.0.join("users"));
    archived_table(&ev, &events(40));
    archived_table(&us, &users());
    let engine = QueryEngine::open_many(&[ev.clone(), us.clone()]).unwrap();
    agree(
        &engine,
        "SELECT COUNT(*) FROM events WHERE user IN (SELECT user FROM users WHERE tier = 'gold')",
        &format!(
            "SELECT COUNT(*) FROM {} WHERE user IN (SELECT user FROM {} WHERE tier = 'gold')",
            archive(&ev),
            archive(&us)
        ),
    );
}

#[test]
fn records_still_in_the_wal_are_read_and_a_query_sees_new_ones() {
    let _serial = serial();
    let t = TempDir::new();
    let dir = t.0.join("live");
    let mut w = TableWriter::open(&dir, TableOptions::default()).unwrap();
    w.append(br#"{"k":"a","n":1}"#, Ack::Durable).unwrap();
    w.flush_all().unwrap();
    w.append(br#"{"k":"b","n":2}"#, Ack::Durable).unwrap();
    // One record in a Parquet file, one only in the WAL.
    let engine = QueryEngine::open(&dir).unwrap();
    let rows = |sql| ours(&engine, sql);
    assert_eq!(
        rows("SELECT k, n FROM live ORDER BY n"),
        [["a", "1"], ["b", "2"]]
    );
    w.append(br#"{"k":"c","n":3}"#, Ack::Durable).unwrap();
    assert_eq!(
        rows("SELECT COUNT(*) FROM live"),
        [["3"]],
        "a fresh snapshot per query"
    );
    w.close().unwrap();
}

#[test]
fn files_with_different_columns_read_as_one_table() {
    let _serial = serial();
    let t = TempDir::new();
    let dir = t.0.join("mixed");
    let mut w = TableWriter::open(&dir, TableOptions::default()).unwrap();
    // `m` appears in the second file only; `n` is an integer in the first
    // and a float in the second, so the table's `n` is a float.
    w.append(br#"{"n":1}"#, Ack::Durable).unwrap();
    w.flush_all().unwrap();
    w.append(br#"{"n":2.5,"m":"x"}"#, Ack::Durable).unwrap();
    w.flush_all().unwrap();
    w.close().unwrap();
    let engine = QueryEngine::open(&dir).unwrap();
    let schema = &engine.schemas()[0];
    assert_eq!(schema.0, "mixed");
    assert!(
        schema.1.contains(&("m".into(), "VARCHAR".into())),
        "{:?}",
        schema.1
    );
    assert!(
        schema.1.contains(&("n".into(), "DOUBLE".into())),
        "{:?}",
        schema.1
    );
    assert_eq!(
        ours(&engine, "SELECT n, m FROM mixed ORDER BY n"),
        [["1", "NULL"], ["2.5", "x"]]
    );
}

#[test]
fn a_table_directory_is_named_after_it_and_can_be_renamed() {
    let _serial = serial();
    let t = TempDir::new();
    let dir = t.0.join("orders");
    let mut w = TableWriter::open(&dir, TableOptions::default()).unwrap();
    w.append(b"{}", Ack::Durable).unwrap();
    w.close().unwrap();
    let mut engine = QueryEngine::default();
    engine.add_table(&dir, None).unwrap();
    engine.add_table(&dir, Some("again".to_string())).unwrap();
    assert_eq!(engine.table_names(), ["orders", "again"]);
    let err = engine.add_table(&dir, None).unwrap_err();
    assert!(err.to_string().contains("already loaded"), "{err}");
    // A plain directory is not a table directory.
    let err = QueryEngine::open(&t.0.join("nope")).err().unwrap();
    assert!(!err.to_string().is_empty());
}
