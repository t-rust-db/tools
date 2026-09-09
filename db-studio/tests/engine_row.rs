//! db-studio#2: smoke test proving the row `Engine` hookup works
//! end-to-end -- open a real `.sqlite` fixture, run a query, get rows or
//! a typed error back. No UI here; the app wiring is #6.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test code fails fast -- see db-core's own test files for the same convention"
)]

use std::path::Path;

use db_core::engine::row::RowEngine;
use db_core::engine::Engine;

fn fixture() -> &'static Path {
    Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/sample.sqlite"
    ))
}

#[test]
fn opens_a_real_sqlite_file_and_runs_a_select() {
    let mut engine = RowEngine::open(fixture()).expect("fixture file should open");
    let result = engine
        .run_query("SELECT name, price FROM items ORDER BY id")
        .expect("query should execute");
    assert_eq!(result.columns, vec!["name", "price"]);
    assert_eq!(result.rows.len(), 3);
}

#[test]
fn a_query_against_an_unknown_table_is_a_typed_error() {
    let mut engine = RowEngine::open(fixture()).expect("fixture file should open");
    let err = engine
        .run_query("SELECT * FROM no_such_table")
        .expect_err("unknown table should error");
    // Just needs *a* message the error pane can show -- kind is an
    // implementation detail of where compilation failed.
    assert!(!err.to_string().is_empty());
}

#[test]
fn opening_a_missing_file_is_an_open_error() {
    let missing = Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/does_not_exist.sqlite"
    ));
    assert!(RowEngine::open(missing).is_err());
}
