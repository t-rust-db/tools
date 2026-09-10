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
