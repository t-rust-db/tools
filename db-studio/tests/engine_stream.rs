//! db-studio#36: smoke test proving the stream `Engine` hookup works
//! end-to-end -- open a real `.log` fixture through `StreamEngine`, run
//! a query, get rows back. Mirrors `tests/engine_row.rs`'s shape for
//! the row engine.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test code fails fast -- see db-core's own test files for the same convention"
)]

use std::path::Path;

use db_core::engine::stream::StreamEngine;
use db_core::engine::Engine;

fn fixture() -> &'static Path {
    Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/sample.log"
    ))
}

#[test]
fn opens_a_real_log_file_and_runs_a_select() {
    let mut engine = StreamEngine::open(fixture()).expect("fixture file should open");
    let result = engine
        .run_query("SELECT severity_text, message FROM log WHERE severity_text = 'ERROR'")
        .expect("query should execute");
    assert_eq!(result.columns, vec!["severity_text", "message"]);
    assert_eq!(result.rows.len(), 1);
}

#[test]
fn reports_unsupported_for_a_join() {
    let mut engine = StreamEngine::open(fixture()).expect("fixture file should open");
    let err = engine
        .run_query("SELECT log.message FROM log JOIN log AS l2 ON log.message = l2.message")
        .expect_err("join against a stream's single table should be rejected");
    assert!(
        !err.to_string().is_empty(),
        "error message should be non-empty"
    );
}
