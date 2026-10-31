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

/// tools#103 as the issue writes it: a session table in SQLite joined
/// SQLite-first to a table directory, and filtered by `IN (SELECT ...)`
/// over it, in `-c` and `--pipe`; `--pipe` sees rows appended since the
/// session opened.
#[test]
fn a_session_table_joins_and_semi_joins_a_table_directory() {
    use db_core::storage::ingest::{Ack, TableOptions, TableWriter};
    let root = std::env::temp_dir().join(format!("column-rs-sessions-{}", std::process::id()));
    std::fs::remove_dir_all(&root).ok();
    let dir = root.join("event");
    let mut w = TableWriter::open(&dir, TableOptions::default()).unwrap();
    let mut append = |rows: &[(i64, i64)]| {
        for (session_id, event_id) in rows {
            w.append(
                format!(r#"{{"session_id":{session_id},"event_id":{event_id}}}"#).as_bytes(),
                Ack::Durable,
            )
            .unwrap();
        }
    };
    append(&[(1, 7), (2, 8), (3, 7)]);
    let sessions = ScratchDb::new(
        "session_live.db",
        "CREATE TABLE session_live(session_id INTEGER PRIMARY KEY, name TEXT);\
         INSERT INTO session_live VALUES (1, 'a');\
         INSERT INTO session_live VALUES (2, 'b');\
         INSERT INTO session_live VALUES (3, 'c');\
         INSERT INTO session_live VALUES (4, 'd');",
    );
    let engine = QueryEngine::open_many(&[sessions.0.clone(), dir.clone()]).unwrap();
    let join = "SELECT session_live.name FROM session_live \
                JOIN event ON event.session_id = session_live.session_id \
                WHERE event.event_id = 7 AND session_live.session_id > 0 \
                ORDER BY session_live.name DESC LIMIT 5";
    let semi = "SELECT name FROM session_live \
                WHERE session_id IN (SELECT session_id FROM event WHERE event_id = 7) \
                ORDER BY name";
    assert_eq!(rows(&engine, join), [vec![text("c")], vec![text("a")]]);
    assert_eq!(rows(&engine, semi), [vec![text("a")], vec![text("c")]]);

    append(&[(4, 7)]);
    let requests = format!(
        "{}\n{}\n",
        serde_json::json!({"id": 1, "sql": join}),
        serde_json::json!({"id": 2, "sql": semi}),
    );
    let mut out = Vec::new();
    column_rs::pipe::run(&engine, requests.as_bytes(), &mut out).unwrap();
    let out = String::from_utf8(out).unwrap();
    drop(append);
    w.close().unwrap();
    std::fs::remove_dir_all(&root).ok();
    let rows_of = |id: i64| -> Vec<String> {
        out.lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter(|v| v["id"] == id && v.get("row").is_some())
            .map(|v| v["row"][0].as_str().unwrap_or_default().to_string())
            .collect()
    };
    assert_eq!(rows_of(1), ["d", "c", "a"], "{out}");
    assert_eq!(rows_of(2), ["a", "c", "d"], "{out}");
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

/// A scratch SQLite file `name` holding `schema`, removed on drop.
struct ScratchDb(PathBuf);

impl ScratchDb {
    fn new(name: &str, schema: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("column-rs-sqlite-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::remove_file(&path).ok();
        db_core::engine::row::RowEngine::create(&path, Default::default(), schema).unwrap();
        ScratchDb(path)
    }
}

impl Drop for ScratchDb {
    fn drop(&mut self) {
        std::fs::remove_file(&self.0).ok();
    }
}

/// tools#103: several SQLite files load side by side; one query reads the
/// tables of one of them.
#[test]
fn several_sqlite_files_load_and_a_query_reads_one_of_them() {
    let machines = ScratchDb::new(
        "machines.db",
        "CREATE TABLE machines(id INTEGER, machine TEXT);\
         INSERT INTO machines VALUES (2, 'm2');\
         INSERT INTO machines VALUES (4500, 'm4500');",
    );
    let engine = QueryEngine::open_many(&[
        fixture("production.parquet"),
        fixture("lookups.db"),
        machines.0.clone(),
    ])
    .unwrap();
    assert_eq!(
        engine.table_names(),
        ["production", "labels", "tiers", "machines"]
    );
    assert_eq!(
        rows(
            &engine,
            "SELECT production.id, machines.machine FROM production \
             JOIN machines ON production.id = machines.id ORDER BY production.id"
        ),
        [vec![int(2), text("m2")], vec![int(4500), text("m4500")]]
    );
    assert_eq!(rows(&engine, "SELECT COUNT(*) FROM labels"), [vec![int(4)]]);
    let across = error(
        &engine,
        "SELECT COUNT(*) FROM machines JOIN labels ON machines.id = labels.id",
    );
    assert!(
        across.contains("one query reads one SQLite file"),
        "{across}"
    );
}

#[test]
fn a_table_name_in_two_sqlite_files_and_an_alias_are_refused() {
    let again = ScratchDb::new("again.db", "CREATE TABLE labels(id INTEGER, label TEXT);");
    let mut engine = engine();
    let second = engine.add_table(&again.0, None).unwrap_err();
    assert!(second.to_string().contains("labels"), "{second}");

    let mut fresh = QueryEngine::default();
    let alias = fresh
        .add_table(&fixture("lookups.db"), Some("x".to_string()))
        .unwrap_err();
    assert!(
        alias.to_string().contains("keep their own names"),
        "{alias}"
    );
}

/// tools#103: an `IN (SELECT ...)` over the other kind of table runs as
/// the list of values the subquery returns, from either side.
#[test]
fn in_select_across_a_sqlite_table_and_a_parquet_file() {
    let engine = engine();
    for (sql, expected) in [
        (
            "SELECT label FROM labels WHERE id IN (SELECT id FROM production WHERE id < 3) \
             ORDER BY label",
            vec![vec![text("one")], vec![text("two")]],
        ),
        (
            "SELECT label FROM labels WHERE id NOT IN (SELECT id FROM production WHERE id < 3) \
             AND id > 0 ORDER BY id",
            vec![vec![text("three")], vec![text("late")]],
        ),
        (
            "SELECT COUNT(*) FROM labels WHERE id IN (SELECT id FROM production WHERE id < 0)",
            vec![vec![int(0)]],
        ),
        (
            "SELECT COUNT(*) FROM labels WHERE id NOT IN (SELECT id FROM production WHERE id < 0)",
            vec![vec![int(4)]],
        ),
        (
            "SELECT COUNT(*) FROM production WHERE id IN (SELECT id FROM labels)",
            vec![vec![int(4)]],
        ),
        (
            "SELECT COUNT(*) FROM production WHERE id IN (SELECT id FROM labels WHERE id > 9999)",
            vec![vec![int(0)]],
        ),
    ] {
        assert_eq!(rows(&engine, sql), expected, "{sql}");
    }
}

/// tools#103 (db-core#919): a join written SQLite-first returns what the
/// swapped form returns, with a filter on the Parquet side.
#[test]
fn a_join_written_sqlite_first_filters_either_side() {
    let engine = engine();
    for (sqlite_first, parquet_first) in [
        (
            "SELECT production.id, labels.label FROM labels \
             JOIN production ON production.id = labels.id \
             WHERE production.id > 1 AND labels.id < 4000 ORDER BY labels.label LIMIT 1",
            "SELECT production.id, labels.label FROM production \
             JOIN labels ON production.id = labels.id \
             WHERE production.id > 1 AND labels.id < 4000 ORDER BY labels.label LIMIT 1",
        ),
        (
            "SELECT COUNT(*) FROM labels JOIN production USING (id) WHERE production.id < 3",
            "SELECT COUNT(*) FROM production JOIN labels USING (id) WHERE production.id < 3",
        ),
    ] {
        let got = rows(&engine, sqlite_first);
        assert!(!got.is_empty(), "{sqlite_first}");
        assert_eq!(got, rows(&engine, parquet_first), "{sqlite_first}");
    }
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
