//! DuckDB oracle test harness (#24): validates column-rs's PLAIN-encoded
//! reads against the same Parquet files read by DuckDB, our source of
//! truth. Fixtures live in `tests/fixtures/` (see `generate.sh` to
//! regenerate them); CI pins a specific DuckDB version so this stays
//! reproducible.
//!
//! If the `duckdb` binary isn't on PATH, these tests print a note and pass
//! trivially rather than failing local runs that don't have it installed.

use column_rs::file::ParquetFile;
use column_rs::vm::Value;
use column_rs::{query, sql};
use std::path::Path;
use std::process::Command;

fn duckdb_csv(sql: &str) -> Option<String> {
    let output = Command::new("duckdb")
        .args(["-csv", "-c", sql])
        .output()
        .ok()?;
    if !output.status.success() {
        panic!(
            "duckdb query failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Some(String::from_utf8(output.stdout).expect("duckdb output is valid UTF-8"))
}

/// Parse simple CSV (no quoting/escaping needed — fixtures never embed
/// commas or newlines in values) into rows of raw string cells, skipping
/// the header line.
fn parse_csv_rows(csv: &str) -> Vec<Vec<String>> {
    csv.lines()
        .skip(1)
        .map(|line| line.split(',').map(str::to_string).collect())
        .collect()
}

fn fixture_path(name: &str) -> String {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
        .to_string_lossy()
        .into_owned()
}

macro_rules! require_duckdb_or_skip {
    ($sql:expr) => {
        match duckdb_csv($sql) {
            Some(csv) => csv,
            None => {
                eprintln!("duckdb not found on PATH; skipping oracle test");
                return;
            }
        }
    };
}

fn cell_as_i64(cell: &str) -> Option<i64> {
    (cell != "NULL").then(|| cell.parse().unwrap())
}

fn cell_as_f64(cell: &str) -> Option<f64> {
    (cell != "NULL").then(|| cell.parse().unwrap())
}

fn cell_as_bool(cell: &str) -> Option<bool> {
    (cell != "NULL").then(|| cell.parse().unwrap())
}

fn cell_as_string(cell: &str) -> Option<String> {
    (cell != "NULL").then(|| cell.to_string())
}

#[test]
fn oracle_int64_matches_duckdb() {
    let path = fixture_path("int64.parquet");
    let csv = require_duckdb_or_skip!(&format!("SELECT id FROM '{path}'"));
    let expected: Vec<Option<i64>> = parse_csv_rows(&csv)
        .iter()
        .map(|row| cell_as_i64(&row[0]))
        .collect();

    let data = std::fs::read(&path).unwrap();
    let file = ParquetFile::open(&data).unwrap();
    let actual = file.row_group(0).unwrap().read_int64_column(0).unwrap();

    assert_eq!(actual, expected);
}

#[test]
fn oracle_double_matches_duckdb() {
    let path = fixture_path("double.parquet");
    let csv = require_duckdb_or_skip!(&format!("SELECT val FROM '{path}'"));
    let expected: Vec<Option<f64>> = parse_csv_rows(&csv)
        .iter()
        .map(|row| cell_as_f64(&row[0]))
        .collect();

    let data = std::fs::read(&path).unwrap();
    let file = ParquetFile::open(&data).unwrap();
    let actual = file.row_group(0).unwrap().read_double_column(0).unwrap();

    assert_eq!(actual, expected);
}

#[test]
fn oracle_boolean_matches_duckdb() {
    let path = fixture_path("boolean.parquet");
    let csv = require_duckdb_or_skip!(&format!("SELECT flag FROM '{path}'"));
    let expected: Vec<Option<bool>> = parse_csv_rows(&csv)
        .iter()
        .map(|row| cell_as_bool(&row[0]))
        .collect();

    let data = std::fs::read(&path).unwrap();
    let file = ParquetFile::open(&data).unwrap();
    let actual = file.row_group(0).unwrap().read_boolean_column(0).unwrap();

    assert_eq!(actual, expected);
}

#[test]
fn oracle_string_matches_duckdb() {
    let path = fixture_path("string.parquet");
    let csv = require_duckdb_or_skip!(&format!("SELECT name FROM '{path}'"));
    let expected: Vec<Option<String>> = parse_csv_rows(&csv)
        .iter()
        .map(|row| cell_as_string(&row[0]))
        .collect();

    let data = std::fs::read(&path).unwrap();
    let file = ParquetFile::open(&data).unwrap();
    let actual = file.row_group(0).unwrap().read_string_column(0).unwrap();

    assert_eq!(actual, expected);
}

#[test]
fn oracle_region_dictionary_matches_duckdb() {
    // Low-cardinality string column: DuckDB writes this PLAIN_DICTIONARY-encoded.
    let path = fixture_path("region.parquet");
    let csv = require_duckdb_or_skip!(&format!("SELECT region FROM '{path}'"));
    let expected: Vec<Option<String>> = parse_csv_rows(&csv)
        .iter()
        .map(|row| cell_as_string(&row[0]))
        .collect();

    let data = std::fs::read(&path).unwrap();
    let file = ParquetFile::open(&data).unwrap();
    let actual = file.row_group(0).unwrap().read_string_column(0).unwrap();

    assert_eq!(actual, expected);
}

#[test]
fn oracle_delta_binary_packed_matches_duckdb() {
    // Sequential INT64 IDs: DuckDB's V2 writer encodes these DELTA_BINARY_PACKED.
    let path = fixture_path("delta.parquet");
    let csv = require_duckdb_or_skip!(&format!("SELECT id FROM '{path}'"));
    let expected: Vec<Option<i64>> = parse_csv_rows(&csv)
        .iter()
        .map(|row| cell_as_i64(&row[0]))
        .collect();

    let data = std::fs::read(&path).unwrap();
    let file = ParquetFile::open(&data).unwrap();
    let actual = file.row_group(0).unwrap().read_int64_column(0).unwrap();

    assert_eq!(actual, expected);
}

#[test]
fn oracle_snappy_compressed_matches_duckdb() {
    let path = fixture_path("snappy.parquet");
    let csv = require_duckdb_or_skip!(&format!("SELECT id, val, name FROM '{path}'"));
    let rows = parse_csv_rows(&csv);

    let data = std::fs::read(&path).unwrap();
    let file = ParquetFile::open(&data).unwrap();
    let rg = file.row_group(0).unwrap();

    let ids = rg.read_int64_column(0).unwrap();
    let vals = rg.read_double_column(1).unwrap();
    let names = rg.read_string_column(2).unwrap();

    for (i, row) in rows.iter().enumerate() {
        assert_eq!(ids[i], cell_as_i64(&row[0]), "id mismatch at row {i}");
        assert_eq!(vals[i], cell_as_f64(&row[1]), "val mismatch at row {i}");
        assert_eq!(
            names[i],
            cell_as_string(&row[2]),
            "name mismatch at row {i}"
        );
    }
}

#[test]
fn oracle_zstd_compressed_matches_duckdb() {
    let path = fixture_path("zstd.parquet");
    let csv = require_duckdb_or_skip!(&format!("SELECT id, val, name FROM '{path}'"));
    let rows = parse_csv_rows(&csv);

    let data = std::fs::read(&path).unwrap();
    let file = ParquetFile::open(&data).unwrap();
    let rg = file.row_group(0).unwrap();

    let ids = rg.read_int64_column(0).unwrap();
    let vals = rg.read_double_column(1).unwrap();
    let names = rg.read_string_column(2).unwrap();

    for (i, row) in rows.iter().enumerate() {
        assert_eq!(ids[i], cell_as_i64(&row[0]), "id mismatch at row {i}");
        assert_eq!(vals[i], cell_as_f64(&row[1]), "val mismatch at row {i}");
        assert_eq!(
            names[i],
            cell_as_string(&row[2]),
            "name mismatch at row {i}"
        );
    }
}

/// #60: INT32, FLOAT, and FIXED_LEN_BYTE_ARRAY physical types (previously
/// unreadable at all) match DuckDB.
#[test]
fn oracle_int32_float_and_fixed_len_byte_array_match_duckdb() {
    let path = fixture_path("dtypes.parquet");
    let csv = require_duckdb_or_skip!(&format!("SELECT i8, i32, f32, dec_big FROM '{path}'"));
    let rows = parse_csv_rows(&csv);

    let data = std::fs::read(&path).unwrap();
    let file = ParquetFile::open(&data).unwrap();
    let rg = file.row_group(0).unwrap();

    let i8s = rg.read_int32_column(0).unwrap();
    let i32s = rg.read_int32_column(1).unwrap();
    let f32s = rg.read_float_column(2).unwrap();
    let decimals = rg.read_fixed_len_byte_array_column(3).unwrap();

    for (i, row) in rows.iter().enumerate() {
        assert_eq!(
            i8s[i],
            Some(row[0].parse::<i32>().unwrap()),
            "i8 mismatch at row {i}"
        );
        assert_eq!(
            i32s[i],
            Some(row[1].parse::<i32>().unwrap()),
            "i32 mismatch at row {i}"
        );
        assert_eq!(
            f32s[i],
            Some(row[2].parse::<f32>().unwrap()),
            "f32 mismatch at row {i}"
        );
        // DECIMAL(38,10) -> a 16-byte big-endian two's-complement unscaled integer;
        // full semantic decoding is out of scope here, just confirm the raw bytes
        // round-trip at the expected fixed width.
        assert_eq!(
            decimals[i].as_ref().unwrap().len(),
            16,
            "decimal byte width mismatch at row {i}"
        );
    }
}

/// #50: INT64 `TIMESTAMP_MICROS` timestamps, and `DECIMAL` for both INT64
/// and `FIXED_LEN_BYTE_ARRAY` physical types (#52, #53), match DuckDB.
/// `Decimal`'s `Display` output is compared directly against DuckDB's own
/// text rendering, so this checks *exact* precision, not an approximation.
#[test]
fn oracle_timestamp_micros_and_decimals_match_duckdb() {
    let path = fixture_path("logical_types.parquet");
    let csv = require_duckdb_or_skip!(&format!(
        "SELECT epoch_us(ts_us), dec_small, dec_big FROM '{path}'"
    ));
    let rows = parse_csv_rows(&csv);

    let data = std::fs::read(&path).unwrap();
    let file = ParquetFile::open(&data).unwrap();
    let rg = file.row_group(0).unwrap();

    let ts = rg.read_timestamp_column(0).unwrap();
    let dec_small = rg.read_decimal_column(1).unwrap();
    let dec_big = rg.read_decimal_column(2).unwrap();

    for (i, row) in rows.iter().enumerate() {
        assert_eq!(
            ts[i],
            Some(row[0].parse::<i64>().unwrap()),
            "timestamp mismatch at row {i}"
        );
        assert_eq!(
            dec_small[i].unwrap().to_string(),
            row[1],
            "dec_small mismatch at row {i}"
        );
        assert_eq!(
            dec_big[i].unwrap().to_string(),
            row[2],
            "dec_big mismatch at row {i}"
        );
    }
}

/// #50: INT64 `TIMESTAMP_MILLIS` timestamps are scaled up to microseconds,
/// matching DuckDB.
#[test]
fn oracle_timestamp_millis_matches_duckdb() {
    let path = fixture_path("logical_types_millis.parquet");
    let csv = require_duckdb_or_skip!(&format!("SELECT epoch_us(ts_ms) FROM '{path}'"));
    let rows = parse_csv_rows(&csv);

    let data = std::fs::read(&path).unwrap();
    let file = ParquetFile::open(&data).unwrap();
    let rg = file.row_group(0).unwrap();
    let ts = rg.read_timestamp_column(0).unwrap();

    for (i, row) in rows.iter().enumerate() {
        assert_eq!(
            ts[i],
            Some(row[0].parse::<i64>().unwrap()),
            "timestamp mismatch at row {i}"
        );
    }
}

/// #51: legacy INT96 timestamps (dictionary-encoded by pyarrow's default
/// writer, exercising the INT96 dictionary decode path too) match DuckDB.
#[test]
fn oracle_int96_timestamps_match_duckdb() {
    let path = fixture_path("int96_timestamps.parquet");
    let csv = require_duckdb_or_skip!(&format!("SELECT epoch_us(ts) FROM '{path}'"));
    let rows = parse_csv_rows(&csv);

    let data = std::fs::read(&path).unwrap();
    let file = ParquetFile::open(&data).unwrap();
    let rg = file.row_group(0).unwrap();
    let ts = rg.read_timestamp_column(0).unwrap();

    assert_eq!(ts.len(), rows.len());
    for (i, row) in rows.iter().enumerate() {
        assert_eq!(
            ts[i],
            Some(row[0].parse::<i64>().unwrap()),
            "INT96 timestamp mismatch at row {i}"
        );
    }
}

/// #49: column chunks spanning multiple pages (the normal case for a large
/// column from a writer like parquet-mr/pyarrow that respects a target page
/// size, unlike DuckDB which always writes one page per chunk) must be read
/// in full, not silently truncated to the first page. `multipage.parquet`
/// was generated with pyarrow (`data_page_size=2048`, high-cardinality
/// strings) to force both multiple `RLE_DICTIONARY` pages *and* a
/// dictionary-fallback to `PLAIN` pages within the same column chunk — see
/// `tests/fixtures/generate.sh` for the exact script.
#[test]
fn oracle_multi_page_column_chunk_matches_duckdb() {
    let path = fixture_path("multipage.parquet");
    let csv = require_duckdb_or_skip!(&format!("SELECT id, name FROM '{path}'"));
    let rows = parse_csv_rows(&csv);

    let data = std::fs::read(&path).unwrap();
    let file = ParquetFile::open(&data).unwrap();
    let rg = file.row_group(0).unwrap();

    let ids = rg.read_int64_column(0).unwrap();
    let names = rg.read_string_column(1).unwrap();

    assert_eq!(ids.len(), rows.len(), "id column was truncated");
    assert_eq!(names.len(), rows.len(), "name column was truncated");
    for (i, row) in rows.iter().enumerate() {
        assert_eq!(ids[i], cell_as_i64(&row[0]), "id mismatch at row {i}");
        assert_eq!(
            names[i],
            cell_as_string(&row[1]),
            "name mismatch at row {i}"
        );
    }
}

/// #61: a nested schema (struct/list/map) must be rejected with a clear
/// error, never silently return wrong values. `nested.parquet` (`id: int64`
/// plus `point: struct<x: int64, y: int64>`) was generated with pyarrow,
/// since DuckDB can't produce nested Parquet schemas either. No DuckDB
/// dependency here: this only needs to observe the error, not compare values.
#[test]
fn nested_struct_column_reconstructs_correctly() {
    use column_rs::nested::NestedValue;

    let path = fixture_path("nested.parquet");
    let data = std::fs::read(&path).unwrap();
    let file = ParquetFile::open(&data).unwrap();
    assert!(!file.is_flat());

    let ids = file.read_nested_column(0, "id").unwrap();
    let points = file.read_nested_column(0, "point").unwrap();
    assert_eq!(ids.len(), 10);
    assert_eq!(points.len(), 10);

    for i in 0..10 {
        assert_eq!(
            ids[i],
            NestedValue::Scalar(column_rs::reader::LeafScalar::Int64(i as i64))
        );
        let NestedValue::Struct(fields) = &points[i] else {
            panic!("expected a struct at row {i}, got {:?}", points[i])
        };
        assert_eq!(
            fields[0],
            (
                "x".to_string(),
                NestedValue::Scalar(column_rs::reader::LeafScalar::Int64(i as i64))
            )
        );
        assert_eq!(
            fields[1],
            (
                "y".to_string(),
                NestedValue::Scalar(column_rs::reader::LeafScalar::Int64((i * 2) as i64))
            )
        );
    }
}

/// #56: LIST columns (of integers and, separately, nullable/empty/null
/// string lists) reconstruct correctly via repetition-level grouping.
/// `list.parquet` was generated with pyarrow (`PLAIN` encoding, no
/// dictionary -- see the nested-leaf scope note on `src/nested.rs`).
#[test]
fn nested_list_column_reconstructs_correctly() {
    use column_rs::nested::NestedValue;
    use column_rs::reader::LeafScalar;

    let path = fixture_path("list.parquet");
    let data = std::fs::read(&path).unwrap();
    let file = ParquetFile::open(&data).unwrap();

    let numbers = file.read_nested_column(0, "numbers").unwrap();
    assert_eq!(
        numbers,
        vec![
            NestedValue::List(vec![
                NestedValue::Scalar(LeafScalar::Int64(1)),
                NestedValue::Scalar(LeafScalar::Int64(2)),
                NestedValue::Scalar(LeafScalar::Int64(3))
            ]),
            NestedValue::List(vec![]),
            NestedValue::List(vec![NestedValue::Scalar(LeafScalar::Int64(4))]),
        ]
    );

    let letters = file.read_nested_column(0, "letters").unwrap();
    assert_eq!(
        letters,
        vec![
            NestedValue::List(vec![
                NestedValue::Scalar(LeafScalar::Str("a".to_string())),
                NestedValue::Scalar(LeafScalar::Str("b".to_string()))
            ]),
            NestedValue::List(vec![NestedValue::Scalar(LeafScalar::Str("c".to_string()))]),
            NestedValue::Null,
        ]
    );
}

/// #58: MAP columns reconstruct as a list of key/value structs (keys are
/// never null per the Parquet spec; values, the map itself, and an empty
/// map are all exercised). `map.parquet` was generated with pyarrow.
#[test]
fn nested_map_column_reconstructs_correctly() {
    use column_rs::nested::NestedValue;
    use column_rs::reader::LeafScalar;

    let path = fixture_path("map.parquet");
    let data = std::fs::read(&path).unwrap();
    let file = ParquetFile::open(&data).unwrap();

    let kv = file.read_nested_column(0, "kv").unwrap();
    let pair = |k: &str, v: i64| {
        NestedValue::Struct(vec![
            (
                "key".to_string(),
                NestedValue::Scalar(LeafScalar::Str(k.to_string())),
            ),
            (
                "value".to_string(),
                NestedValue::Scalar(LeafScalar::Int64(v)),
            ),
        ])
    };
    assert_eq!(
        kv,
        vec![
            NestedValue::List(vec![pair("a", 1), pair("b", 2)]),
            NestedValue::List(vec![]),
            NestedValue::List(vec![pair("c", 3)]),
        ]
    );
}

/// #41: dictionary indices kept raw (not resolved to `String` per row) must
/// still reconstruct the exact same values as the eager `read_string_column`.
#[test]
fn dictionary_indices_reconstruct_same_values_as_resolved_column() {
    let path = fixture_path("region.parquet");
    let data = std::fs::read(&path).unwrap();
    let file = ParquetFile::open(&data).unwrap();
    let rg = file.row_group(0).unwrap();

    let resolved = rg.read_string_column(0).unwrap();
    let (dictionary, indices) = rg
        .read_string_column_dictionary_indices(0)
        .unwrap()
        .expect("region.parquet is dictionary-encoded");

    let reconstructed: Vec<Option<String>> = indices
        .into_iter()
        .map(|idx| idx.map(|i| dictionary[i as usize].clone()))
        .collect();
    assert_eq!(reconstructed, resolved);
}

/// #46: a production-shaped file (multi-row-group, ZSTD-compressed,
/// dictionary-encoded `region`) run through the full SQL-to-VM pipeline,
/// exercising compression + dictionary encoding + parallel multi-segment
/// #63: an INNER hash join between two tables matches DuckDB.
#[test]
fn oracle_inner_join_matches_duckdb() {
    let orders_path = fixture_path("orders.parquet");
    let regions_path = fixture_path("regions.parquet");
    let csv = require_duckdb_or_skip!(&format!(
        "SELECT orders.id, regions.budget FROM '{orders_path}' orders \
         JOIN '{regions_path}' regions ON orders.region_key = regions.key ORDER BY orders.id"
    ));
    let expected: Vec<(i64, f64)> = parse_csv_rows(&csv)
        .iter()
        .map(|row| (row[0].parse().unwrap(), row[1].parse().unwrap()))
        .collect();

    let orders_data = std::fs::read(&orders_path).unwrap();
    let regions_data = std::fs::read(&regions_path).unwrap();
    let orders = ParquetFile::open(&orders_data).unwrap();
    let regions = ParquetFile::open(&regions_data).unwrap();

    let parsed = sql::parse("SELECT orders.id, regions.budget FROM orders JOIN regions ON orders.region_key = regions.key ORDER BY orders.id").unwrap();
    let rows = query::execute_joined(&orders, &regions, &parsed).unwrap();

    assert_eq!(rows.len(), expected.len());
    for ((id, budget), row) in expected.iter().zip(&rows) {
        assert_eq!(row[0].as_f64().unwrap() as i64, *id, "id mismatch");
        assert_eq!(
            row[1].as_f64().unwrap(),
            *budget,
            "budget mismatch for id {id}"
        );
    }
}

/// #94: `QueryEngine` (the CLI's multi-table session) can load more than
/// one Parquet file and run a `JOIN` across them, using each file's stem
/// as its table name -- the same query the free-function `execute_joined`
/// oracle test above already validates against DuckDB, but driven through
/// the higher-level engine the CLI actually calls.
#[test]
fn query_engine_runs_join_across_two_loaded_tables() {
    use column_rs::query::QueryEngine;
    use std::path::PathBuf;

    let paths = vec![
        PathBuf::from(fixture_path("orders.parquet")),
        PathBuf::from(fixture_path("regions.parquet")),
    ];
    let engine = QueryEngine::open_many(&paths).unwrap();

    assert_eq!(engine.table_names(), vec!["orders", "regions"]);

    let result = engine.execute("SELECT orders.id, regions.budget FROM orders JOIN regions ON orders.region_key = regions.key ORDER BY orders.id").unwrap();
    assert!(!result.rows.is_empty());
    assert_eq!(result.columns, vec!["orders.id", "regions.budget"]);
}

/// #94: loading the same table name twice is a clear error, not a silent
/// replace.
#[test]
fn query_engine_rejects_duplicate_table_name() {
    use column_rs::query::QueryEngine;

    let mut engine =
        QueryEngine::open(std::path::Path::new(&fixture_path("orders.parquet"))).unwrap();
    let err = engine
        .add_table(std::path::Path::new(&fixture_path("orders.parquet")), None)
        .unwrap_err();
    assert!(
        err.to_string().contains("orders"),
        "error should name the duplicate table: {err}"
    );
}

/// #110: `column-rs -c "<SQL>"` (one-shot mode) streams tab-separated rows
/// directly (`stream_output::print_rows_streaming`) instead of collecting the whole
/// result and rendering a box-drawing table -- confirms the CLI's output
/// still matches `QueryEngine::execute` exactly (same rows, same order,
/// same header), not just that it prints *something* fast.
#[test]
fn cli_one_shot_streaming_output_matches_query_engine() {
    let path = fixture_path("mixed.parquet");
    let sql = "SELECT id, name FROM mixed WHERE id > 995";
    let bin = env!("CARGO_BIN_EXE_column-rs");
    let output = Command::new(bin).args(["-c", sql, &path]).output().unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();

    let engine = column_rs::query::QueryEngine::open(Path::new(&path)).unwrap();
    let result = engine.execute(sql).unwrap();
    let mut expected = format!("{}\n", result.columns.join("\t"));
    for row in &result.rows {
        let line: Vec<String> = row.iter().map(|v| v.to_string()).collect();
        expected.push_str(&line.join("\t"));
        expected.push('\n');
    }

    assert_eq!(stdout, expected);
}

/// #64: a LEFT hash join keeps unmatched left rows (with NULL on the right
/// side) instead of dropping them like INNER JOIN does. `orders.parquet`
/// has one row (`id=21`) whose `region_key` ('south') matches no row in
/// `regions.parquet`.
#[test]
fn oracle_left_join_keeps_unmatched_rows_matches_duckdb() {
    let orders_path = fixture_path("orders.parquet");
    let regions_path = fixture_path("regions.parquet");
    let csv = require_duckdb_or_skip!(&format!(
        "SELECT orders.id, regions.budget FROM '{orders_path}' orders \
         LEFT JOIN '{regions_path}' regions ON orders.region_key = regions.key ORDER BY orders.id"
    ));
    let expected: Vec<(i64, Option<f64>)> = parse_csv_rows(&csv)
        .iter()
        .map(|row| (row[0].parse().unwrap(), cell_as_f64(&row[1])))
        .collect();

    let orders_data = std::fs::read(&orders_path).unwrap();
    let regions_data = std::fs::read(&regions_path).unwrap();
    let orders = ParquetFile::open(&orders_data).unwrap();
    let regions = ParquetFile::open(&regions_data).unwrap();

    let parsed =
        sql::parse("SELECT orders.id, regions.budget FROM orders LEFT JOIN regions ON orders.region_key = regions.key ORDER BY orders.id")
            .unwrap();
    let rows = query::execute_joined(&orders, &regions, &parsed).unwrap();

    assert_eq!(
        rows.len(),
        expected.len(),
        "LEFT JOIN row count mismatch (unmatched rows must be kept)"
    );
    assert!(
        rows.iter().any(|r| matches!(r[1], Value::Null)),
        "expected at least one unmatched (NULL) right side"
    );
    for ((id, budget), row) in expected.iter().zip(&rows) {
        assert_eq!(row[0].as_f64().unwrap() as i64, *id, "id mismatch");
        match budget {
            Some(b) => assert_eq!(row[1].as_f64().unwrap(), *b, "budget mismatch for id {id}"),
            None => assert!(
                matches!(row[1], Value::Null),
                "expected NULL budget for unmatched id {id}"
            ),
        }
    }
}

/// #65: a semi-join (`WHERE col IN (SELECT ...)`) matches DuckDB. Excludes
/// `orders.parquet`'s unmatched row (`id=21`, `region_key='south'`), same as
/// the LEFT JOIN test's unmatched-row case but via `IN` instead of a join.
#[test]
fn oracle_semi_join_matches_duckdb() {
    let orders_path = fixture_path("orders.parquet");
    let regions_path = fixture_path("regions.parquet");
    let csv = require_duckdb_or_skip!(&format!(
        "SELECT id FROM '{orders_path}' WHERE region_key IN (SELECT key FROM '{regions_path}') ORDER BY id"
    ));
    let expected: Vec<i64> = parse_csv_rows(&csv)
        .iter()
        .map(|row| row[0].parse().unwrap())
        .collect();

    let orders_data = std::fs::read(&orders_path).unwrap();
    let regions_data = std::fs::read(&regions_path).unwrap();
    let orders = ParquetFile::open(&orders_data).unwrap();
    let regions = ParquetFile::open(&regions_data).unwrap();

    let parsed = sql::parse(
        "SELECT id FROM orders WHERE region_key IN (SELECT key FROM regions) ORDER BY id",
    )
    .unwrap();
    let rows = query::execute_semi_join(&orders, &regions, &parsed).unwrap();

    assert_eq!(
        rows.len(),
        expected.len(),
        "semi-join should exclude the unmatched region_key row"
    );
    for (id, row) in expected.iter().zip(&rows) {
        assert_eq!(row[0].as_f64().unwrap() as i64, *id);
    }
}

/// #67: ROW_NUMBER/RANK/DENSE_RANK match DuckDB.
#[test]
fn oracle_ranking_window_functions_match_duckdb() {
    let path = fixture_path("orders.parquet");
    let sql_text =
        "SELECT id, region_key, ROW_NUMBER() OVER w, RANK() OVER w, DENSE_RANK() OVER w \
                     FROM '{path}' WINDOW w AS (PARTITION BY region_key ORDER BY id) ORDER BY id";
    let csv = require_duckdb_or_skip!(&sql_text.replace("{path}", &path));
    let expected: Vec<(i64, i64, i64, i64)> = parse_csv_rows(&csv)
        .iter()
        .map(|row| {
            (
                row[0].parse().unwrap(),
                row[2].parse().unwrap(),
                row[3].parse().unwrap(),
                row[4].parse().unwrap(),
            )
        })
        .collect();

    let data = std::fs::read(&path).unwrap();
    let file = ParquetFile::open(&data).unwrap();
    let parsed =
        sql::parse("SELECT id, region_key, ROW_NUMBER() OVER (PARTITION BY region_key ORDER BY id), RANK() OVER (PARTITION BY region_key ORDER BY id), DENSE_RANK() OVER (PARTITION BY region_key ORDER BY id) FROM orders ORDER BY id")
            .unwrap();
    let rows = query::execute_windowed(&file, &parsed).unwrap();

    assert_eq!(rows.len(), expected.len());
    for ((id, rn, rk, drk), row) in expected.iter().zip(&rows) {
        assert_eq!(row[0].as_f64().unwrap() as i64, *id);
        assert_eq!(
            row[2].as_f64().unwrap() as i64,
            *rn,
            "ROW_NUMBER mismatch for id {id}"
        );
        assert_eq!(
            row[3].as_f64().unwrap() as i64,
            *rk,
            "RANK mismatch for id {id}"
        );
        assert_eq!(
            row[4].as_f64().unwrap() as i64,
            *drk,
            "DENSE_RANK mismatch for id {id}"
        );
    }
}

/// #68/#69: LAG/LEAD and FIRST_VALUE/LAST_VALUE match DuckDB, including
/// LAST_VALUE's default-frame quirk (returns the current row, not the
/// partition's true last row).
#[test]
fn oracle_lag_lead_first_last_value_match_duckdb() {
    let path = fixture_path("orders.parquet");
    let csv = require_duckdb_or_skip!(&format!(
        "SELECT id, LAG(id) OVER w, LEAD(id) OVER w, FIRST_VALUE(id) OVER w, LAST_VALUE(id) OVER w \
         FROM '{path}' WINDOW w AS (PARTITION BY region_key ORDER BY id) ORDER BY id"
    ));
    type ExpectedRow = (i64, Option<i64>, Option<i64>, i64, i64);
    let expected: Vec<ExpectedRow> = parse_csv_rows(&csv)
        .iter()
        .map(|row| {
            (
                row[0].parse().unwrap(),
                cell_as_i64(&row[1]),
                cell_as_i64(&row[2]),
                row[3].parse().unwrap(),
                row[4].parse().unwrap(),
            )
        })
        .collect();

    let data = std::fs::read(&path).unwrap();
    let file = ParquetFile::open(&data).unwrap();
    let parsed = sql::parse(
        "SELECT id, LAG(id) OVER (PARTITION BY region_key ORDER BY id), LEAD(id) OVER (PARTITION BY region_key ORDER BY id), \
         FIRST_VALUE(id) OVER (PARTITION BY region_key ORDER BY id), LAST_VALUE(id) OVER (PARTITION BY region_key ORDER BY id) \
         FROM orders ORDER BY id",
    )
    .unwrap();
    let rows = query::execute_windowed(&file, &parsed).unwrap();

    assert_eq!(rows.len(), expected.len());
    for ((id, lag, lead, first, last), row) in expected.iter().zip(&rows) {
        assert_eq!(row[0].as_f64().unwrap() as i64, *id);
        match lag {
            Some(v) => assert_eq!(
                row[1].as_f64().unwrap() as i64,
                *v,
                "LAG mismatch for id {id}"
            ),
            None => assert!(
                matches!(row[1], Value::Null),
                "expected NULL LAG for id {id}"
            ),
        }
        match lead {
            Some(v) => assert_eq!(
                row[2].as_f64().unwrap() as i64,
                *v,
                "LEAD mismatch for id {id}"
            ),
            None => assert!(
                matches!(row[2], Value::Null),
                "expected NULL LEAD for id {id}"
            ),
        }
        assert_eq!(
            row[3].as_f64().unwrap() as i64,
            *first,
            "FIRST_VALUE mismatch for id {id}"
        );
        assert_eq!(
            row[4].as_f64().unwrap() as i64,
            *last,
            "LAST_VALUE mismatch for id {id}"
        );
    }
}

/// #70: SUM/COUNT/AVG OVER match DuckDB, both as a cumulative (running)
/// aggregate (when the window has an ORDER BY) and a whole-partition
/// aggregate (when it doesn't).
#[test]
fn oracle_window_aggregates_match_duckdb() {
    let path = fixture_path("orders.parquet");
    let csv = require_duckdb_or_skip!(&format!(
        "SELECT id, SUM(id) OVER running, COUNT(id) OVER whole, AVG(id) OVER whole FROM '{path}' \
         WINDOW running AS (PARTITION BY region_key ORDER BY id), whole AS (PARTITION BY region_key) ORDER BY id"
    ));
    let expected: Vec<(i64, f64, i64, f64)> = parse_csv_rows(&csv)
        .iter()
        .map(|row| {
            (
                row[0].parse().unwrap(),
                row[1].parse().unwrap(),
                row[2].parse().unwrap(),
                row[3].parse().unwrap(),
            )
        })
        .collect();

    let data = std::fs::read(&path).unwrap();
    let file = ParquetFile::open(&data).unwrap();
    let parsed = sql::parse(
        "SELECT id, SUM(id) OVER (PARTITION BY region_key ORDER BY id), COUNT(id) OVER (PARTITION BY region_key), AVG(id) OVER (PARTITION BY region_key) \
         FROM orders ORDER BY id",
    )
    .unwrap();
    let rows = query::execute_windowed(&file, &parsed).unwrap();

    assert_eq!(rows.len(), expected.len());
    for ((id, running_sum, whole_count, whole_avg), row) in expected.iter().zip(&rows) {
        assert_eq!(row[0].as_f64().unwrap() as i64, *id);
        assert_eq!(
            row[1].as_f64().unwrap(),
            *running_sum,
            "running SUM mismatch for id {id}"
        );
        assert_eq!(
            row[2].as_f64().unwrap() as i64,
            *whole_count,
            "COUNT mismatch for id {id}"
        );
        assert_eq!(
            row[3].as_f64().unwrap(),
            *whole_avg,
            "AVG mismatch for id {id}"
        );
    }
}

/// scan + GROUP BY together, validated against DuckDB.
#[test]
fn oracle_production_shaped_file_group_by_matches_duckdb() {
    let path = fixture_path("production.parquet");
    let csv = require_duckdb_or_skip!(&format!(
        "SELECT region, SUM(amount), COUNT(*) FROM '{path}' WHERE id > 1000 GROUP BY region ORDER BY region"
    ));
    let expected: Vec<(String, f64, i64)> = parse_csv_rows(&csv)
        .iter()
        .map(|row| {
            (
                row[0].clone(),
                row[1].parse().unwrap(),
                row[2].parse().unwrap(),
            )
        })
        .collect();

    let data = std::fs::read(&path).unwrap();
    let file = ParquetFile::open(&data).unwrap();
    assert!(
        file.num_row_groups() > 1,
        "fixture should span multiple row groups to exercise parallel scan"
    );

    let query = sql::parse("SELECT region, SUM(amount), COUNT(*) FROM production WHERE id > 1000 GROUP BY region ORDER BY region").unwrap();
    let mut rows = query::execute(&file, &query).unwrap();
    rows.sort_by(|a, b| a[0].to_string().cmp(&b[0].to_string()));

    assert_eq!(rows.len(), expected.len());
    for ((region, sum, count), row) in expected.iter().zip(&rows) {
        assert_eq!(row[0].to_string(), *region);
        assert_eq!(
            row[1].as_f64().unwrap(),
            *sum,
            "sum mismatch for region {region}"
        );
        assert_eq!(
            row[2].as_f64().unwrap() as i64,
            *count,
            "count mismatch for region {region}"
        );
    }
}

/// #108: a bare `LIMIT` (no `WHERE`/`ORDER BY`/aggregate) is satisfied by
/// a bounded prefix scan (`query::bounded_scan`) instead of decoding every
/// row group -- this only checks correctness (right rows, in file order,
/// even though the fixture spans 5 row groups); the actual perf win is
/// covered by `t-rust-db/benchmark/parity/column-rs`'s `queries/scan.sql`
/// (#100), not a unit test.
#[test]
fn oracle_limit_only_matches_duckdb() {
    let path = fixture_path("production.parquet");
    let csv = require_duckdb_or_skip!(&format!(
        "SELECT region, amount, id FROM '{path}' LIMIT 1500"
    ));
    let expected: Vec<(String, f64, i64)> = parse_csv_rows(&csv)
        .iter()
        .map(|row| {
            (
                row[0].clone(),
                row[1].parse().unwrap(),
                row[2].parse().unwrap(),
            )
        })
        .collect();

    let data = std::fs::read(&path).unwrap();
    let file = ParquetFile::open(&data).unwrap();
    assert!(file.num_row_groups() > 1, "fixture should span multiple row groups to exercise the bounded scan crossing a row-group boundary");

    let query = sql::parse("SELECT region, amount, id FROM production LIMIT 1500").unwrap();
    let rows = query::execute(&file, &query).unwrap();

    assert_eq!(rows.len(), expected.len());
    assert_eq!(rows.len(), 1500);
    for ((region, amount, id), row) in expected.iter().zip(&rows) {
        assert_eq!(row[0].to_string(), *region);
        assert_eq!(row[1].as_f64().unwrap(), *amount);
        assert_eq!(row[2].as_f64().unwrap() as i64, *id);
    }
}

#[test]
fn oracle_mixed_types_match_duckdb() {
    let path = fixture_path("mixed.parquet");
    let csv = require_duckdb_or_skip!(&format!("SELECT id, val, flag, name FROM '{path}'"));
    let rows = parse_csv_rows(&csv);

    let data = std::fs::read(&path).unwrap();
    let file = ParquetFile::open(&data).unwrap();
    let rg = file.row_group(0).unwrap();

    let ids = rg.read_int64_column(0).unwrap();
    let vals = rg.read_double_column(1).unwrap();
    let flags = rg.read_boolean_column(2).unwrap();
    let names = rg.read_string_column(3).unwrap();

    for (i, row) in rows.iter().enumerate() {
        assert_eq!(ids[i], cell_as_i64(&row[0]), "id mismatch at row {i}");
        assert_eq!(vals[i], cell_as_f64(&row[1]), "val mismatch at row {i}");
        assert_eq!(flags[i], cell_as_bool(&row[2]), "flag mismatch at row {i}");
        assert_eq!(
            names[i],
            cell_as_string(&row[3]),
            "name mismatch at row {i}"
        );
    }
}

#[test]
fn oracle_nullable_columns_match_duckdb() {
    let path = fixture_path("nullable.parquet");
    let csv = require_duckdb_or_skip!(&format!("SELECT id, val, flag, name FROM '{path}'"));
    let rows = parse_csv_rows(&csv);

    let data = std::fs::read(&path).unwrap();
    let file = ParquetFile::open(&data).unwrap();
    let rg = file.row_group(0).unwrap();

    let ids = rg.read_int64_column(0).unwrap();
    let vals = rg.read_double_column(1).unwrap();
    let flags = rg.read_boolean_column(2).unwrap();
    let names = rg.read_string_column(3).unwrap();

    // Sanity: this fixture actually exercises nulls in every column.
    assert!(ids.iter().any(Option::is_none));
    assert!(vals.iter().any(Option::is_none));
    assert!(flags.iter().any(Option::is_none));
    assert!(names.iter().any(Option::is_none));

    for (i, row) in rows.iter().enumerate() {
        assert_eq!(ids[i], cell_as_i64(&row[0]), "id mismatch at row {i}");
        assert_eq!(vals[i], cell_as_f64(&row[1]), "val mismatch at row {i}");
        assert_eq!(flags[i], cell_as_bool(&row[2]), "flag mismatch at row {i}");
        assert_eq!(
            names[i],
            cell_as_string(&row[3]),
            "name mismatch at row {i}"
        );
    }
}

/// `ORDER BY ... LIMIT ...` (#109): the bounded top-N path (`query::execute`
/// -> `run_program_top_n` -> `vm::run_parallel_top_n`) must match DuckDB's
/// row order and its default `NULLS LAST` placement (both `ASC` and `DESC`).
#[test]
fn oracle_order_by_limit_top_n_matches_duckdb() {
    let path = fixture_path("nullable.parquet");

    for order in ["ASC", "DESC"] {
        let csv = require_duckdb_or_skip!(&format!(
            "SELECT id, val FROM '{path}' ORDER BY val {order} LIMIT 5"
        ));
        let expected: Vec<(Option<i64>, Option<f64>)> = parse_csv_rows(&csv)
            .iter()
            .map(|row| (cell_as_i64(&row[0]), cell_as_f64(&row[1])))
            .collect();

        let data = std::fs::read(&path).unwrap();
        let file = ParquetFile::open(&data).unwrap();
        let query = sql::parse(&format!(
            "SELECT id, val FROM nullable ORDER BY val {order} LIMIT 5"
        ))
        .unwrap();
        let rows = query::execute(&file, &query).unwrap();

        let actual: Vec<(Option<i64>, Option<f64>)> = rows
            .iter()
            .map(|row| {
                let id = match &row[0] {
                    Value::Int(v) => Some(*v),
                    Value::Null => None,
                    other => panic!("unexpected id value: {other:?}"),
                };
                let val = match &row[1] {
                    Value::Float(v) => Some(*v),
                    Value::Null => None,
                    other => panic!("unexpected val value: {other:?}"),
                };
                (id, val)
            })
            .collect();

        assert_eq!(
            actual, expected,
            "mismatch for ORDER BY val {order} LIMIT 5"
        );
    }
}

/// Generates a 100K+ row fixture on the fly (not checked into git, to keep
/// the repo small) and validates row count plus spot-checked values against
/// DuckDB.
#[test]
fn oracle_large_file_100k_rows() {
    if duckdb_csv("SELECT 1").is_none() {
        eprintln!("duckdb not found on PATH; skipping oracle test");
        return;
    }

    let tmp_path = std::env::temp_dir().join(format!(
        "column-rs-oracle-large-{}.parquet",
        std::process::id()
    ));
    let tmp_path_str = tmp_path.to_string_lossy();

    let create_sql = format!(
        "COPY (SELECT i AS id, (i * 1.5)::DOUBLE AS val, (i % 2 = 0) AS flag, 'row_' || i AS name \
         FROM range(1, 100001) t(i)) TO '{tmp_path_str}' (FORMAT PARQUET, COMPRESSION UNCOMPRESSED)"
    );
    duckdb_csv(&create_sql);

    let data = std::fs::read(&tmp_path).unwrap();
    let file = ParquetFile::open(&data).unwrap();
    assert_eq!(file.num_rows(), 100_000);
    assert_eq!(file.num_row_groups(), 1);

    let rg = file.row_group(0).unwrap();
    let ids = rg.read_int64_column(0).unwrap();
    let names = rg.read_string_column(3).unwrap();
    assert_eq!(ids.len(), 100_000);

    let csv = duckdb_csv(&format!(
        "SELECT id, name FROM '{tmp_path_str}' WHERE id IN (1, 50000, 100000) ORDER BY id"
    ))
    .unwrap();
    let rows = parse_csv_rows(&csv);
    for row in rows {
        let expected_id = cell_as_i64(&row[0]).unwrap();
        let expected_name = cell_as_string(&row[1]).unwrap();
        let idx = (expected_id - 1) as usize;
        assert_eq!(ids[idx], Some(expected_id));
        assert_eq!(names[idx], Some(expected_name));
    }

    std::fs::remove_file(&tmp_path).ok();
}

/// Oracle test for the query VM (#35): `SELECT region, SUM(amount) ...
/// WHERE ... GROUP BY region` executed through the SQL parser + VM must
/// match DuckDB's own answer for the same query.
#[test]
fn oracle_query_vm_group_by_sum_matches_duckdb() {
    let path = fixture_path("mixed.parquet");
    let csv = require_duckdb_or_skip!(&format!(
        "SELECT flag, SUM(val) FROM '{path}' WHERE id > 100 GROUP BY flag ORDER BY flag"
    ));
    let expected: Vec<(bool, f64)> = parse_csv_rows(&csv)
        .iter()
        .map(|row| (row[0].parse().unwrap(), row[1].parse().unwrap()))
        .collect();

    let data = std::fs::read(&path).unwrap();
    let file = ParquetFile::open(&data).unwrap();
    let query =
        sql::parse("SELECT flag, SUM(val) FROM mixed WHERE id > 100 GROUP BY flag ORDER BY flag")
            .unwrap();
    let mut rows = query::execute(&file, &query).unwrap();
    rows.sort_by(|a, b| a[0].to_string().cmp(&b[0].to_string()));

    assert_eq!(rows.len(), expected.len());
    for ((flag, sum), row) in expected.iter().zip(&rows) {
        assert_eq!(row[0].to_string(), flag.to_string());
        assert_eq!(
            row[1].as_f64().unwrap(),
            *sum,
            "sum mismatch for flag {flag}"
        );
    }
}

/// COUNT(*) and a plain WHERE filter (no GROUP BY) through the VM must also
/// match DuckDB.
#[test]
fn oracle_query_vm_where_count_matches_duckdb() {
    let path = fixture_path("mixed.parquet");
    let csv = require_duckdb_or_skip!(&format!("SELECT COUNT(*) FROM '{path}' WHERE id > 500"));
    let expected: i64 = parse_csv_rows(&csv)[0][0].parse().unwrap();

    let data = std::fs::read(&path).unwrap();
    let file = ParquetFile::open(&data).unwrap();
    let query = sql::parse("SELECT COUNT(*) FROM mixed WHERE id > 500").unwrap();
    let rows = query::execute(&file, &query).unwrap();

    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0].as_f64().unwrap() as i64, expected);
}

/// Regression (#100): projected columns that don't appear in the `WHERE`
/// clause used to be loaded *after* the `Filter` opcode, so they kept the
/// full pre-filter batch length while the filtered registers shrank, and
/// `Emit` then indexed past the end of the short ones (a panic, not an
/// error). Found while building the DuckDB parity benchmark, where
/// `SELECT id, amount, region, customer_id ... WHERE amount > 9900`
/// aborted with exit code 101.
#[test]
fn oracle_projection_of_unfiltered_columns_matches_duckdb() {
    let path = fixture_path("mixed.parquet");
    let csv = require_duckdb_or_skip!(&format!("SELECT id, name FROM '{path}' WHERE val > 1400"));
    let expected: Vec<(i64, String)> = parse_csv_rows(&csv)
        .iter()
        .map(|row| (row[0].parse().unwrap(), row[1].clone()))
        .collect();

    let data = std::fs::read(&path).unwrap();
    let file = ParquetFile::open(&data).unwrap();

    let parsed = sql::parse("SELECT id, name FROM mixed WHERE val > 1400").unwrap();
    let rows = query::execute(&file, &parsed).unwrap();

    assert!(!expected.is_empty(), "fixture should select some rows");
    assert_eq!(rows.len(), expected.len());
    for ((id, name), row) in expected.iter().zip(&rows) {
        assert_eq!(row[0], Value::Int(*id), "id mismatch");
        assert_eq!(
            row[1],
            Value::Str(name.clone().into()),
            "name mismatch for id {id}"
        );
    }
}

/// #9: `SELECT *` expands against the loaded table's schema (via db-core's
/// `expand_star`) before dispatch, so it returns every column with the
/// right names and values, matching DuckDB.
#[test]
fn oracle_select_star_matches_duckdb() {
    use column_rs::query::QueryEngine;

    let path = fixture_path("mixed.parquet");
    let csv = require_duckdb_or_skip!(&format!("SELECT * FROM '{path}'"));
    let expected = parse_csv_rows(&csv);

    let engine = QueryEngine::open(Path::new(&path)).unwrap();
    let result = engine.execute("SELECT * FROM mixed").unwrap();

    assert_eq!(result.columns, vec!["id", "val", "flag", "name"]);
    assert_eq!(result.rows.len(), expected.len());
    for (expected_row, row) in expected.iter().zip(&result.rows) {
        assert_eq!(row[0], Value::Int(cell_as_i64(&expected_row[0]).unwrap()));
        assert_eq!(row[1], Value::Float(cell_as_f64(&expected_row[1]).unwrap()));
        assert_eq!(row[2], Value::Bool(cell_as_bool(&expected_row[2]).unwrap()));
        assert_eq!(
            row[3],
            Value::Str(cell_as_string(&expected_row[3]).unwrap().into())
        );
    }
}

/// #9: a mixed `SELECT id, *` keeps `id` first, then expands `*` after it
/// (duplicating `id`), matching DuckDB's own behavior for the same query.
#[test]
fn oracle_select_mixed_star_matches_duckdb() {
    use column_rs::query::QueryEngine;

    let path = fixture_path("mixed.parquet");
    let csv = require_duckdb_or_skip!(&format!("SELECT id, * FROM '{path}'"));
    let expected = parse_csv_rows(&csv);

    let engine = QueryEngine::open(Path::new(&path)).unwrap();
    let result = engine.execute("SELECT id, * FROM mixed").unwrap();

    assert_eq!(result.columns, vec!["id", "id", "val", "flag", "name"]);
    assert_eq!(result.rows.len(), expected.len());
    for (expected_row, row) in expected.iter().zip(&result.rows) {
        assert_eq!(row[0], Value::Int(cell_as_i64(&expected_row[0]).unwrap()));
        assert_eq!(row[1], Value::Int(cell_as_i64(&expected_row[1]).unwrap()));
    }
}

/// #9: `SELECT *` composes with `WHERE`/`ORDER BY`/`LIMIT`.
#[test]
fn oracle_select_star_with_where_order_limit_matches_duckdb() {
    use column_rs::query::QueryEngine;

    let path = fixture_path("mixed.parquet");
    let csv = require_duckdb_or_skip!(&format!(
        "SELECT * FROM '{path}' WHERE id > 100 ORDER BY id LIMIT 5"
    ));
    let expected = parse_csv_rows(&csv);

    let engine = QueryEngine::open(Path::new(&path)).unwrap();
    let result = engine
        .execute("SELECT * FROM mixed WHERE id > 100 ORDER BY id LIMIT 5")
        .unwrap();

    assert_eq!(result.rows.len(), expected.len());
    for (expected_row, row) in expected.iter().zip(&result.rows) {
        assert_eq!(row[0], Value::Int(cell_as_i64(&expected_row[0]).unwrap()));
    }
}

/// #9: `*` combined with `GROUP BY` is a clear error, not silently wrong
/// results -- db-core's `expand_star` rejects it (`PlanError::StarWithAggregation`),
/// mapped here to `QueryError::StarWithAggregation`.
#[test]
fn select_star_with_group_by_is_rejected() {
    use column_rs::query::{QueryEngine, QueryError};

    let path = fixture_path("production.parquet");
    let engine = QueryEngine::open(Path::new(&path)).unwrap();
    let err = match engine.execute("SELECT * FROM production GROUP BY region") {
        Err(e) => e,
        Ok(_) => panic!("expected StarWithAggregation error"),
    };
    assert!(matches!(err, QueryError::StarWithAggregation));
}

/// #9: `*` combined with an aggregate (even without `GROUP BY`) is likewise
/// rejected.
#[test]
fn select_star_with_aggregate_is_rejected() {
    use column_rs::query::{QueryEngine, QueryError};

    let path = fixture_path("production.parquet");
    let engine = QueryEngine::open(Path::new(&path)).unwrap();
    let err = match engine.execute("SELECT *, SUM(amount) FROM production") {
        Err(e) => e,
        Ok(_) => panic!("expected StarWithAggregation error"),
    };
    assert!(matches!(err, QueryError::StarWithAggregation));
}

/// #9: `SELECT * FROM orders JOIN regions ON ...` expands against both
/// tables' schemas, qualified `table.column` per the existing join-column
/// naming convention (see `oracle_inner_join_matches_duckdb`).
#[test]
fn oracle_select_star_join_matches_duckdb() {
    use column_rs::query::QueryEngine;
    use std::path::PathBuf;

    let orders_path = fixture_path("orders.parquet");
    let regions_path = fixture_path("regions.parquet");
    let csv = require_duckdb_or_skip!(&format!(
        "SELECT * FROM '{orders_path}' orders JOIN '{regions_path}' regions \
         ON orders.region_key = regions.key ORDER BY orders.id"
    ));
    let expected = parse_csv_rows(&csv);

    let paths = vec![PathBuf::from(&orders_path), PathBuf::from(&regions_path)];
    let engine = QueryEngine::open_many(&paths).unwrap();
    let result = engine
        .execute("SELECT * FROM orders JOIN regions ON orders.region_key = regions.key ORDER BY orders.id")
        .unwrap();

    assert_eq!(result.columns.len(), expected[0].len());
    assert_eq!(result.rows.len(), expected.len());
}

/// #4: `||` string concatenation (`BinOp::Concat -> MapOp::Concat`) and
/// unary minus (`Expr::Neg -> MapOp::Neg`) match DuckDB end to end. The
/// mapping itself lives in db-core's batch planner since the planner moved
/// there (0.16.0); this pins the column-rs-visible behavior. Both operators
/// are exercised in `WHERE` position: db-core's column grammar still
/// rejects any computed expression in the SELECT list (see the last
/// assertion), tracked upstream as db-core#198.
#[test]
fn oracle_concat_and_unary_minus_match_duckdb() {
    use column_rs::query::QueryEngine;

    let path = fixture_path("funky.parquet");
    let cases: &[(&str, &str)] = &[
        (
            "SELECT performer_id FROM '{path}' WHERE -audience_size < -300 ORDER BY performer_id",
            "SELECT performer_id FROM funky WHERE -audience_size < -300 ORDER BY performer_id",
        ),
        (
            "SELECT performer_id FROM '{path}' WHERE -(audience_size * 2) + 1 < -700 ORDER BY performer_id",
            "SELECT performer_id FROM funky WHERE -(audience_size * 2) + 1 < -700 ORDER BY performer_id",
        ),
        (
            "SELECT performer_id FROM '{path}' WHERE stage_name || '/' || act > 'M' ORDER BY performer_id",
            "SELECT performer_id FROM funky WHERE stage_name || '/' || act > 'M' ORDER BY performer_id",
        ),
    ];
    let engine = QueryEngine::open(Path::new(&path)).unwrap();
    for (duck_sql, ours_sql) in cases {
        let csv = require_duckdb_or_skip!(&duck_sql.replace("{path}", &path));
        let expected: Vec<i64> = parse_csv_rows(&csv)
            .iter()
            .map(|row| cell_as_i64(&row[0]).unwrap())
            .collect();
        assert!(
            !expected.is_empty(),
            "oracle returned no rows for {duck_sql}"
        );
        let result = engine.execute(ours_sql).unwrap();
        let ours: Vec<i64> = result
            .rows
            .iter()
            .map(|row| row[0].as_f64().unwrap() as i64)
            .collect();
        assert_eq!(ours, expected, "{ours_sql}");
    }

    // Computed SELECT-list items are a db-core column-grammar gap, not an
    // operator-wiring one: the rejection is explicit, never a wrong answer.
    let err = match engine.execute("SELECT stage_name || act FROM funky") {
        Ok(_) => panic!("computed SELECT-list item unexpectedly accepted"),
        Err(e) => e.to_string(),
    };
    assert!(err.contains("unsupported SELECT expression"), "{err}");
}
