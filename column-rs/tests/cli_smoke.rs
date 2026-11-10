//! `make smoke`'s contract, as a test: the binary builds, and `--help` /
//! `--version` exit 0 with something on stdout. Before the `--help` arm
//! existed it was opened as a Parquet path (`error: --help: No such file
//! or directory`, exit 1).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::process::Command;

fn run(arg: &str) -> (i32, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_column-rs"))
        .arg(arg)
        .output()
        .expect("spawn column-rs");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn help_exits_zero_with_usage_on_stdout() {
    for flag in ["--help", "-h"] {
        let (code, stdout, stderr) = run(flag);
        assert_eq!(code, 0, "{flag}: stderr={stderr}");
        assert!(stdout.starts_with("usage: column-rs "), "{flag}: {stdout}");
        assert!(stderr.is_empty(), "{flag}: {stderr}");
    }
}

#[test]
fn version_exits_zero_with_name_and_version() {
    let (code, stdout, _) = run("--version");
    assert_eq!(code, 0);
    assert_eq!(
        stdout.trim(),
        format!("column-rs {}", env!("CARGO_PKG_VERSION"))
    );
}

/// #83: `.schema` lists each column with its type, aligned per table.
#[test]
fn schema_lists_each_columns_type() {
    use std::io::Write;
    use std::process::Stdio;

    let fixture = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/tagged.parquet");
    let mut child = Command::new(env!("CARGO_BIN_EXE_column-rs"))
        .arg(fixture)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn column-rs");
    child.stdin.take().unwrap().write_all(b".schema\n").unwrap();
    let out = child.wait_with_output().expect("wait for column-rs");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains(
            "tagged:\n  id       BIGINT\n  product  VARCHAR\n  tags     VARCHAR[]\n  amount   DOUBLE\n"
        ),
        "{stdout}"
    );
}

/// tools#86: `.physical` on an open file shows its row groups and column
/// tree; `rowgroups` gives the column section per row group, and a
/// Parquet file that isn't open can be named by path.
#[test]
fn physical_shows_row_groups_and_the_column_tree() {
    use std::io::Write;
    use std::process::Stdio;

    let fixture = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/list.parquet");
    let map = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/map.parquet");
    let mut child = Command::new(env!("CARGO_BIN_EXE_column-rs"))
        .arg(fixture)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn column-rs");
    let input = format!(".physical\n.physical {map} rowgroups sizes\n.physical nope\n");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let out = child.wait_with_output().expect("wait for column-rs");
    let stdout = String::from_utf8_lossy(&out.stdout);
    for expected in [
        "row groups  1\n  row group  rows  stored    raw\n          0     3   165 B  165 B\n",
        "columns        physical    rep.      logical  encoding  compression\n\
         \x20 numbers      group       optional  LIST\n\
         \x20   list       group       repeated\n\
         \x20     element  INT64       optional           plain     none\n",
        "row group 0  (3 rows)\n",
        "      key      BYTE_ARRAY  required  STRING   plain     none           57 B   57 B    36%\n",
        "  total                                                               157 B  157 B\n",
        "error: no table or file named nope",
    ] {
        assert!(stdout.contains(expected), "missing {expected:?} in\n{stdout}");
    }
}

/// #101: a Binary column prints as `x'AB..'`, as db-core's `Display` does,
/// not as `NULL`.
#[test]
fn blob_columns_print_as_hex_literals() {
    let out = Command::new(env!("CARGO_BIN_EXE_column-rs"))
        .args(["-c", "SELECT id, k, seq FROM blobs WHERE id < 2"])
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/blobs.parquet"
        ))
        .output()
        .expect("spawn column-rs");
    assert!(out.status.success(), "{out:?}");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "id\tk\tseq\n\
         0\tx''\tx'FFFF0000'\n\
         1\tx'33333333333333333333333333333333'\tx'FFFF0001'\n"
    );
}

/// db-core#1046: a TIMESTAMP column, a `date_trunc` over one and a
/// `min` of one print as ISO 8601, as DuckDB prints them, through `-c`'s
/// streaming path and its collected one (an aggregate); `epoch_ms` stays
/// an integer.
#[test]
fn timestamps_print_as_iso_8601() {
    let fixture = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/logical_types_millis.parquet"
    );
    let query = |sql: &str| {
        let out = Command::new(env!("CARGO_BIN_EXE_column-rs"))
            .args(["-c", sql, fixture])
            .output()
            .expect("spawn column-rs");
        assert!(out.status.success(), "{out:?}");
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    assert_eq!(
        query(
            "SELECT ts_ms, date_trunc('hour', ts_ms) AS h, epoch_ms(ts_ms) AS e \
             FROM logical_types_millis LIMIT 1"
        ),
        "ts_ms\th\te\n2024-01-15 10:30:00\t2024-01-15 10:00:00\t1705314600000\n"
    );
    assert_eq!(
        query(
            "SELECT date_trunc('hour', ts_ms) AS h, min(ts_ms) AS m, count(*) AS n \
             FROM logical_types_millis GROUP BY h"
        ),
        "h\tm\tn\n2024-01-15 10:00:00\t2024-01-15 10:30:00\t5\n"
    );
}

/// db-core#1046: the REPL's table prints a TIMESTAMP column as ISO 8601
/// too.
#[test]
fn repl_prints_timestamps_as_iso_8601() {
    use std::io::Write;
    use std::process::Stdio;

    let fixture = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/logical_types_millis.parquet"
    );
    let mut child = Command::new(env!("CARGO_BIN_EXE_column-rs"))
        .arg(fixture)
        .env(
            "XDG_STATE_HOME",
            std::env::temp_dir().join("column-rs-repl-ts"),
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn column-rs");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"SELECT ts_ms FROM logical_types_millis LIMIT 1;\n")
        .unwrap();
    let out = child.wait_with_output().expect("wait for column-rs");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("\u{2502} 2024-01-15 10:30:00 \u{2502}"),
        "{stdout}"
    );
}
