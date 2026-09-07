//! Epic #98's sub-ticket 4 (see #101): actually compile an emitted `.rs`
//! file (`db_core::codegen::batch::emit::generate`, the AOT emitter column-rs's
//! `codegen` subcommand wraps) with `rustc` (linking against the
//! already-built `column_rs` rlib) and run the resulting binary, rather
//! than only asserting on the generated source text (that's what the
//! emitter's own unit tests in db-core do).
//! Confirms the generated binary's output matches `QueryEngine::execute`
//! for the same query, against a real fixture with more than one row group
//! (`production.parquet`) so `GROUP BY` merging across segments is
//! actually exercised end-to-end, not just structurally present in the
//! source.

use std::path::PathBuf;
use std::process::Command;

fn fixture_path(name: &str) -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
        .to_string_lossy()
        .into_owned()
}

/// Locate the already-built `libcolumn_rs-*.rlib` in `target/<profile>/deps`
/// (guaranteed to exist -- this very test binary links against it) so
/// `rustc` can compile a freshly-generated file against the same library,
/// without needing a separate `cargo build`.
fn find_column_rs_rlib(deps_dir: &std::path::Path) -> PathBuf {
    std::fs::read_dir(deps_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("libcolumn_rs-") && n.ends_with(".rlib")))
        .max_by_key(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok())
        .expect("libcolumn_rs-*.rlib not found in target deps -- expected `cargo test` to have already built it")
}

fn deps_dir() -> PathBuf {
    // The test binary itself lives at target/<profile>/deps/<test>-<hash>;
    // its own directory is exactly the deps dir we need.
    std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

/// Compile `src` (a codegen'd `.rs` file) into a standalone binary linked
/// against `column_rs`, run it with `args`, and return its captured stdout.
fn compile_and_run(src: &str, args: &[&str]) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let deps = deps_dir();
    let rlib = find_column_rs_rlib(&deps);

    let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "column_rs_codegen_e2e_{}_{unique}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let src_path = dir.join("generated.rs");
    let bin_path = dir.join("generated_bin");
    std::fs::write(&src_path, src).unwrap();

    let status = Command::new("rustc")
        .args(["--edition", "2021", "--crate-type", "bin"])
        .arg("-L")
        .arg(&deps)
        .arg("--extern")
        .arg(format!("column_rs={}", rlib.display()))
        .arg("-o")
        .arg(&bin_path)
        .arg(&src_path)
        .status()
        .expect("failed to spawn rustc");
    assert!(
        status.success(),
        "rustc failed to compile generated source:\n{src}"
    );

    let output = Command::new(&bin_path)
        .args(args)
        .output()
        .expect("failed to run generated binary");
    assert!(
        output.status.success(),
        "generated binary exited non-zero: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let _ = std::fs::remove_dir_all(&dir);
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn codegen_group_by_matches_query_engine_across_multiple_row_groups() {
    let sql = "SELECT region, SUM(amount) FROM production GROUP BY region ORDER BY region";
    let src = db_core::codegen::batch::emit::generate("column_rs", sql).unwrap();
    let fixture = fixture_path("production.parquet");

    let generated_output = compile_and_run(&src, &[&fixture]);

    let engine = column_rs::query::QueryEngine::open(std::path::Path::new(&fixture)).unwrap();
    let result = engine.execute(sql).unwrap();
    let mut expected = format!("{}\n", result.columns.join("\t"));
    for row in &result.rows {
        let line: Vec<String> = row.iter().map(|v| v.to_string()).collect();
        expected.push_str(&line.join("\t"));
        expected.push('\n');
    }

    assert_eq!(generated_output, expected);
}

#[test]
fn codegen_flat_filter_matches_query_engine() {
    let sql = "SELECT id, name FROM mixed WHERE id > 995";
    let src = db_core::codegen::batch::emit::generate("column_rs", sql).unwrap();
    let fixture = fixture_path("mixed.parquet");

    let generated_output = compile_and_run(&src, &[&fixture]);

    let engine = column_rs::query::QueryEngine::open(std::path::Path::new(&fixture)).unwrap();
    let result = engine.execute(sql).unwrap();
    let mut expected = format!("{}\n", result.columns.join("\t"));
    for row in &result.rows {
        let line: Vec<String> = row.iter().map(|v| v.to_string()).collect();
        expected.push_str(&line.join("\t"));
        expected.push('\n');
    }

    assert_eq!(generated_output, expected);
}

/// #103: a generated `JOIN` binary reconstructs the parsed `Query` as a
/// literal Rust value at startup (not by parsing SQL text) and calls
/// `execute_joined` with it -- confirms that round-trip actually works,
/// not just that the source text looks plausible.
#[test]
fn codegen_join_matches_query_engine() {
    let sql = "SELECT orders.id, regions.budget FROM orders JOIN regions ON orders.region_key = regions.key ORDER BY orders.id";
    let src = db_core::codegen::batch::emit::generate("column_rs", sql).unwrap();
    let orders = fixture_path("orders.parquet");
    let regions = fixture_path("regions.parquet");

    let generated_output = compile_and_run(&src, &[&orders, &regions]);

    let engine = column_rs::query::QueryEngine::open_many(&[
        std::path::PathBuf::from(&orders),
        std::path::PathBuf::from(&regions),
    ])
    .unwrap();
    let result = engine.execute(sql).unwrap();
    let mut expected = format!("{}\n", result.columns.join("\t"));
    for row in &result.rows {
        let line: Vec<String> = row.iter().map(|v| v.to_string()).collect();
        expected.push_str(&line.join("\t"));
        expected.push('\n');
    }

    assert_eq!(generated_output, expected);
}

/// #1: `ROW_NUMBER() OVER (PARTITION BY ... ORDER BY ...)` -- the window
/// shape reconstructs `Query` as a literal the same way the `JOIN` shape
/// does (see `render_windowed` in `db_core::codegen::batch::emit`) and calls
/// `execute_windowed` at runtime.
#[test]
fn codegen_row_number_window_matches_query_engine() {
    let sql =
        "SELECT id, region_key, ROW_NUMBER() OVER (PARTITION BY region_key ORDER BY id) FROM orders ORDER BY id";
    let src = db_core::codegen::batch::emit::generate("column_rs", sql).unwrap();
    let fixture = fixture_path("orders.parquet");

    let generated_output = compile_and_run(&src, &[&fixture]);

    let engine = column_rs::query::QueryEngine::open(std::path::Path::new(&fixture)).unwrap();
    let result = engine.execute(sql).unwrap();
    let mut expected = format!("{}\n", result.columns.join("\t"));
    for row in &result.rows {
        let line: Vec<String> = row.iter().map(|v| v.to_string()).collect();
        expected.push_str(&line.join("\t"));
        expected.push('\n');
    }

    assert_eq!(generated_output, expected);
}

/// #1: `LAG`/`LEAD`, with and without an explicit offset -- confirms
/// `WindowSpec::offset` round-trips through the literal reconstruction
/// (`None` defaults to 1 at runtime in `compute_window`, so this also
/// exercises that default).
#[test]
fn codegen_lag_lead_window_matches_query_engine() {
    let sql = "SELECT id, region_key, LAG(id) OVER (PARTITION BY region_key ORDER BY id), LEAD(id, 2) OVER (PARTITION BY region_key ORDER BY id) FROM orders ORDER BY id";
    let src = db_core::codegen::batch::emit::generate("column_rs", sql).unwrap();
    let fixture = fixture_path("orders.parquet");

    let generated_output = compile_and_run(&src, &[&fixture]);

    let engine = column_rs::query::QueryEngine::open(std::path::Path::new(&fixture)).unwrap();
    let result = engine.execute(sql).unwrap();
    let mut expected = format!("{}\n", result.columns.join("\t"));
    for row in &result.rows {
        let line: Vec<String> = row.iter().map(|v| v.to_string()).collect();
        expected.push_str(&line.join("\t"));
        expected.push('\n');
    }

    assert_eq!(generated_output, expected);
}

/// #1: an aggregate window function (`SUM(...) OVER (PARTITION BY ...)`,
/// no `ORDER BY` inside `OVER` -- the whole-partition-aggregate path in
/// `compute_window`).
#[test]
fn codegen_sum_over_window_matches_query_engine() {
    let sql = "SELECT region, amount, SUM(amount) OVER (PARTITION BY region) FROM production ORDER BY region";
    let src = db_core::codegen::batch::emit::generate("column_rs", sql).unwrap();
    let fixture = fixture_path("production.parquet");

    let generated_output = compile_and_run(&src, &[&fixture]);

    let engine = column_rs::query::QueryEngine::open(std::path::Path::new(&fixture)).unwrap();
    let result = engine.execute(sql).unwrap();
    let mut expected = format!("{}\n", result.columns.join("\t"));
    for row in &result.rows {
        let line: Vec<String> = row.iter().map(|v| v.to_string()).collect();
        expected.push_str(&line.join("\t"));
        expected.push('\n');
    }

    assert_eq!(generated_output, expected);
}

/// #103: same round-trip, for an `IN (SELECT ...)` semi-join.
#[test]
fn codegen_semi_join_matches_query_engine() {
    let sql = "SELECT id FROM orders WHERE region_key IN (SELECT key FROM regions)";
    let src = db_core::codegen::batch::emit::generate("column_rs", sql).unwrap();
    let orders = fixture_path("orders.parquet");
    let regions = fixture_path("regions.parquet");

    let generated_output = compile_and_run(&src, &[&orders, &regions]);

    let engine = column_rs::query::QueryEngine::open_many(&[
        std::path::PathBuf::from(&orders),
        std::path::PathBuf::from(&regions),
    ])
    .unwrap();
    let result = engine.execute(sql).unwrap();
    let mut expected = format!("{}\n", result.columns.join("\t"));
    for row in &result.rows {
        let line: Vec<String> = row.iter().map(|v| v.to_string()).collect();
        expected.push_str(&line.join("\t"));
        expected.push('\n');
    }

    assert_eq!(generated_output, expected);
}
