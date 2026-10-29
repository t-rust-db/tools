//! A Parquet file joined to the tables of a SQLite file (tools#69): the
//! SQLite file loads like any other path, its tables under their own
//! names, and a query that names one runs in db-core's cross-mode engine
//! (db-core#598). `production.parquet` has ids 1..=5000 in three row
//! groups; `lookups.db` has `labels` (ids 1, 2, 3, 4500) and `tiers` (ids
//! 1, 2), so a filter on a lookup table keeps one row group.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use column_rs::query::QueryEngine;
use column_rs::vm::Value;
use std::path::{Path, PathBuf};
use std::process::Command;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn engine() -> QueryEngine {
    QueryEngine::open_many(&[fixture("production.parquet"), fixture("lookups.db")]).unwrap()
}

fn rows(engine: &QueryEngine, sql: &str) -> Vec<Vec<Value>> {
    engine
        .execute(sql)
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
        .rows()
        .collect()
}

fn error(engine: &QueryEngine, sql: &str) -> String {
    match engine.execute(sql) {
        Ok(_) => panic!("{sql}: expected an error"),
        Err(e) => e.to_string(),
    }
}

fn int(i: i64) -> Value {
    Value::Int(i)
}

fn text(s: &str) -> Value {
    Value::Str(s.to_string().into())
}

#[test]
fn a_sqlite_files_tables_load_under_their_own_names() {
    let engine = engine();
    assert_eq!(engine.table_names(), ["production", "labels", "tiers"]);
    let schemas = engine.schemas();
    let labels = schemas.iter().find(|(t, _)| *t == "labels").unwrap();
    assert_eq!(
        labels.1,
        [
            ("id".into(), "INTEGER".into()),
            ("label".into(), "TEXT".into())
        ]
    );
}

#[test]
fn inner_join_resolves_lookup_columns() {
    assert_eq!(
        rows(
            &engine(),
            "SELECT production.id, labels.label FROM production \
             JOIN labels ON production.id = labels.id ORDER BY production.id"
        ),
        [
            vec![int(1), text("one")],
            vec![int(2), text("two")],
            vec![int(3), text("three")],
            vec![int(4500), text("late")],
        ]
    );
}

#[test]
fn using_and_either_side_first_in_on() {
    let engine = engine();
    let expected = rows(
        &engine,
        "SELECT production.id FROM production JOIN labels ON production.id = labels.id ORDER BY 1",
    );
    for sql in [
        "SELECT production.id FROM production JOIN labels USING (id) ORDER BY 1",
        "SELECT production.id FROM production JOIN labels ON labels.id = production.id ORDER BY 1",
    ] {
        assert_eq!(rows(&engine, sql), expected, "{sql}");
    }
}

#[test]
fn left_join_keeps_every_parquet_row() {
    assert_eq!(
        rows(
            &engine(),
            "SELECT COUNT(*), COUNT(labels.label) FROM production \
             LEFT JOIN labels ON production.id = labels.id"
        ),
        [vec![int(5000), int(4)]]
    );
}

#[test]
fn star_join_to_two_lookup_tables() {
    assert_eq!(
        rows(
            &engine(),
            "SELECT production.id, labels.label, tiers.tier FROM production \
             JOIN labels ON production.id = labels.id \
             JOIN tiers ON production.id = tiers.id ORDER BY production.id"
        ),
        [
            vec![int(1), text("one"), text("gold")],
            vec![int(2), text("two"), text("silver")],
        ]
    );
}

#[test]
fn group_by_a_lookup_column() {
    assert_eq!(
        rows(
            &engine(),
            "SELECT tiers.tier, COUNT(*) FROM production \
             JOIN tiers ON production.id = tiers.id GROUP BY tiers.tier ORDER BY tiers.tier"
        ),
        [vec![text("gold"), int(1)], vec![text("silver"), int(1)]]
    );
}

#[test]
fn a_lookup_left_join_parquet_is_refused_with_db_cores_message() {
    let message = error(
        &engine(),
        "SELECT COUNT(*) FROM labels LEFT JOIN production ON production.id = labels.id",
    );
    assert!(
        message.contains("write this as `production LEFT JOIN ...`"),
        "{message}"
    );
}

#[test]
fn a_lookup_filter_prunes_parquet_row_groups() {
    let engine = engine();
    let sql = "SELECT production.id FROM production JOIN labels ON production.id = labels.id \
               WHERE labels.label = 'late'";
    assert_eq!(rows(&engine, sql), [vec![int(4500)]]);
    let query = column_rs::sql::parse(sql).unwrap();
    let plan = engine.explain_lookup(&query, sql).unwrap().unwrap();
    assert!(
        plan.iter().any(|n| n.detail.ends_with("(1 of 3 kept)")),
        "{:?}",
        plan.iter().map(|n| &n.detail).collect::<Vec<_>>()
    );

    let unfiltered =
        "SELECT production.id FROM production JOIN labels ON production.id = labels.id";
    let query = column_rs::sql::parse(unfiltered).unwrap();
    let plan = engine.explain_lookup(&query, unfiltered).unwrap().unwrap();
    assert!(!plan.iter().any(|n| n.detail.starts_with("PRUNE")));
}

#[test]
fn a_lookup_table_alone_is_queried_in_the_row_engine() {
    assert_eq!(
        rows(&engine(), "SELECT id, tier FROM tiers ORDER BY id"),
        [vec![int(1), text("gold")], vec![int(2), text("silver")]]
    );
}

/// tools#74: a query over SQLite tables alone is read as sqlite3 reads it,
/// functions the column dialect lacks included; one that names a Parquet
/// table is still held to the column dialect.
#[test]
fn a_lookup_only_query_takes_sqlite3_functions() {
    let engine = engine();
    assert_eq!(
        rows(
            &engine,
            "SELECT UPPER(label), ROUND(id * 1.5, 1) FROM labels WHERE id <= 2 ORDER BY id"
        ),
        [
            vec![text("ONE"), Value::Float(1.5)],
            vec![text("TWO"), Value::Float(3.0)],
        ]
    );
    for sql in [
        "SELECT UPPER(region) FROM production",
        "SELECT UPPER(labels.label) FROM production JOIN labels ON production.id = labels.id",
    ] {
        // db-core 0.201 words it as an unsupported expression form, not
        // "unknown function UPPER"; refused at the call either way.
        let message = error(&engine, sql);
        assert!(
            message.starts_with("unsupported at 1:8: ") && message.contains("UPPER"),
            "{sql}: {message}"
        );
    }
}

#[test]
fn cli_reads_a_lookup_only_query_and_its_plan() {
    for (sql, expected) in [
        ("SELECT UPPER(label) FROM labels WHERE id = 1", "ONE"),
        (
            "EXPLAIN QUERY PLAN SELECT UPPER(label) FROM labels",
            "SCAN labels",
        ),
    ] {
        let out = Command::new(env!("CARGO_BIN_EXE_column-rs"))
            .arg("-c")
            .arg(sql)
            .arg(fixture("production.parquet"))
            .arg(fixture("lookups.db"))
            .output()
            .expect("spawn column-rs");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success(),
            "{sql}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(stdout.contains(expected), "{sql}: {stdout}");
    }
}

#[test]
fn queries_without_a_lookup_table_are_unchanged() {
    let engine = engine();
    assert_eq!(
        rows(&engine, "SELECT COUNT(*) FROM production"),
        [vec![int(5000)]]
    );
    let query = column_rs::sql::parse("SELECT COUNT(*) FROM production").unwrap();
    assert!(engine
        .explain_lookup(&query, "SELECT COUNT(*) FROM production")
        .unwrap()
        .is_none());
}

/// db-core#782: a star join around a lookup table may name a second
/// Parquet table. Expected rows from DuckDB over the same files.
#[test]
fn a_star_join_names_two_parquet_tables_and_a_lookup() {
    let engine = QueryEngine::open_many(&[
        fixture("production.parquet"),
        fixture("orders.parquet"),
        fixture("lookups.db"),
    ])
    .unwrap();
    assert_eq!(
        rows(
            &engine,
            "SELECT production.id, labels.label, orders.region_key FROM production \
             JOIN labels ON production.id = labels.id \
             JOIN orders ON orders.id = production.id ORDER BY production.id"
        ),
        [
            vec![int(1), text("one"), text("west")],
            vec![int(2), text("two"), text("north")],
            vec![int(3), text("three"), text("east")],
        ]
    );
}

/// db-core#772: a table directory joins the lookup tables, rows in its
/// live pages included.
#[test]
fn a_table_directory_joins_a_lookup_table() {
    use db_core::storage::ingest::{Ack, TableOptions, TableWriter};
    let root = std::env::temp_dir().join(format!("column-rs-sqlite-join-{}", std::process::id()));
    std::fs::remove_dir_all(&root).ok();
    let dir = root.join("events");
    let mut w = TableWriter::open(&dir, TableOptions::default()).unwrap();
    for id in [1, 2] {
        w.append(format!(r#"{{"id":{id}}}"#).as_bytes(), Ack::Durable)
            .unwrap();
    }
    w.flush_all().unwrap();
    for id in [3, 4] {
        w.append(format!(r#"{{"id":{id}}}"#).as_bytes(), Ack::Durable)
            .unwrap();
    }
    let engine = QueryEngine::open_many(&[dir.clone(), fixture("lookups.db")]).unwrap();
    let got = rows(
        &engine,
        "SELECT events.id, labels.label FROM events \
         JOIN labels ON events.id = labels.id ORDER BY events.id",
    );
    w.close().unwrap();
    std::fs::remove_dir_all(&root).ok();
    assert_eq!(
        got,
        [
            vec![int(1), text("one")],
            vec![int(2), text("two")],
            vec![int(3), text("three")],
        ]
    );
}

/// db-core 0.184/0.186: ROUND, ABS and CASE run over a Parquet table
/// joined to a lookup. Expected rows from DuckDB.
#[test]
fn round_abs_and_case_over_a_cross_mode_join() {
    assert_eq!(
        rows(
            &engine(),
            "SELECT production.id, CASE WHEN production.id < 3 THEN 'low' ELSE 'high' END, \
             ROUND(production.id / 4.0, 1), ABS(production.id - 4) FROM production \
             JOIN labels ON production.id = labels.id ORDER BY production.id"
        ),
        [
            vec![int(1), text("low"), Value::Float(0.3), int(3)],
            vec![int(2), text("low"), Value::Float(0.5), int(2)],
            vec![int(3), text("high"), Value::Float(0.8), int(1)],
            vec![int(4500), text("high"), Value::Float(1125.0), int(4496)],
        ]
    );
}

#[test]
fn one_sqlite_file_per_session_and_no_alias() {
    let mut engine = engine();
    let second = engine.add_table(&fixture("lookups.db"), None).unwrap_err();
    assert!(
        second.to_string().contains("one SQLite file per session"),
        "{second}"
    );

    let mut fresh = QueryEngine::default();
    let alias = fresh
        .add_table(&fixture("lookups.db"), Some("x".to_string()))
        .unwrap_err();
    assert!(
        alias.to_string().contains("keep their own names"),
        "{alias}"
    );
}

#[test]
fn a_parquet_table_under_another_name_is_refused() {
    let mut engine = QueryEngine::default();
    engine
        .add_table(&fixture("production.parquet"), Some("prod".to_string()))
        .unwrap();
    engine.add_table(&fixture("lookups.db"), None).unwrap();
    let message = error(
        &engine,
        "SELECT COUNT(*) FROM prod JOIN labels ON prod.id = labels.id",
    );
    assert!(
        message.contains("named after its file or directory (production)"),
        "{message}"
    );
}

#[test]
fn pipe_reports_a_refused_join_as_unsupported() {
    let engine = engine();
    let request = r#"{"sql":"SELECT COUNT(*) FROM labels LEFT JOIN production ON production.id = labels.id"}"#;
    let mut out = Vec::new();
    column_rs::pipe::run(&engine, request.as_bytes(), &mut out).unwrap();
    let out = String::from_utf8(out).unwrap();
    assert!(out.contains(r#""kind":"unsupported""#), "{out}");
}

#[test]
fn cli_explain_query_plan_shows_the_pruning() {
    let out = Command::new(env!("CARGO_BIN_EXE_column-rs"))
        .arg("-c")
        .arg(
            "EXPLAIN QUERY PLAN SELECT production.id FROM production \
             JOIN labels ON production.id = labels.id WHERE labels.label = 'late'",
        )
        .arg(fixture("production.parquet"))
        .arg(fixture("lookups.db"))
        .output()
        .expect("spawn column-rs");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains("PRUNE ROW GROUPS"), "{stdout}");
    assert!(stdout.contains("(1 of 3 kept)"), "{stdout}");
}
