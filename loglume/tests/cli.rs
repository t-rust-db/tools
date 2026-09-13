//! Integration tests driving the compiled `loglume` binary against the
//! committed `tests/logs/sample.log` fixture (deterministic: `--seed 42`).

use assert_cmd::Command;
use predicates::prelude::*;

const SAMPLE_LOG: &str = "tests/logs/sample.log";

#[test]
fn filters_by_severity() {
    let assert = Command::cargo_bin("loglume")
        .unwrap()
        .args(["severity >= ERR", SAMPLE_LOG])
        .assert()
        .success();

    let output = assert.get_output();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.is_empty(), "expected at least one matching line");
    for line in stdout.lines() {
        assert!(line.starts_with('<'), "unexpected line shape: {line}");
    }
}

#[test]
fn filters_by_facility() {
    Command::cargo_bin("loglume")
        .unwrap()
        .args(["facility = auth", SAMPLE_LOG])
        .assert()
        .success()
        .stdout(predicate::str::contains("sshd"));
}

#[test]
fn and_combinator_narrows_results() {
    let combined = Command::cargo_bin("loglume")
        .unwrap()
        .args(["severity >= WARN and facility = auth", SAMPLE_LOG])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let facility_only = Command::cargo_bin("loglume")
        .unwrap()
        .args(["facility = auth", SAMPLE_LOG])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let combined_lines = String::from_utf8_lossy(&combined).lines().count();
    let facility_only_lines = String::from_utf8_lossy(&facility_only).lines().count();
    assert!(combined_lines <= facility_only_lines);
}

#[test]
fn unrecognized_filter_errors_with_nonzero_exit() {
    Command::cargo_bin("loglume")
        .unwrap()
        .args(["not a real filter", SAMPLE_LOG])
        .assert()
        .failure()
        .stderr(predicate::str::contains("error:"));
}

#[test]
fn max_lines_caps_output() {
    Command::cargo_bin("loglume")
        .unwrap()
        .args(["severity >= DEBUG", "-n", "5", SAMPLE_LOG])
        .assert()
        .success()
        .stdout(predicate::function(|s: &str| s.lines().count() <= 5));
}

#[test]
fn prints_effective_range_footer_to_stderr() {
    Command::cargo_bin("loglume")
        .unwrap()
        .args(["severity >= DEBUG", SAMPLE_LOG])
        .assert()
        .success()
        .stderr(
            predicate::str::contains("lines,")
                .and(predicate::str::starts_with("--"))
                .and(predicate::str::contains('T').and(predicate::str::contains('Z'))),
        );
}

#[test]
fn raw_sql_query_works() {
    Command::cargo_bin("loglume")
        .unwrap()
        .args(["select count(*) from log", SAMPLE_LOG])
        .assert()
        .success()
        .stdout(predicate::str::contains("1000"));
}

#[test]
fn follow_mode_prints_appended_lines_and_footer() {
    use std::io::Write as _;
    use std::process::{Command as StdCommand, Stdio};
    use std::time::Duration;

    let dir = std::env::temp_dir();
    let path = dir.join(format!(
        "loglume-follow-cli-test-{}.log",
        std::process::id()
    ));
    std::fs::write(&path, "<134>Sep 9 08:00:00 host app[1]: initial line\n").unwrap();

    let bin = assert_cmd::cargo::cargo_bin("loglume");
    let mut child = StdCommand::new(bin)
        .args(["severity >= DEBUG", "--follow", path.to_str().unwrap()])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn loglume --follow");

    std::thread::sleep(Duration::from_millis(300));
    {
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(f, "<131>Sep 9 08:00:01 host app[1]: err after start").unwrap();
    }
    std::thread::sleep(Duration::from_millis(700));

    // SIGTERM would be nicer than kill, but std::process::Child has no
    // portable signal API; the process is a leaf test helper, so a hard
    // kill is fine here.
    child.kill().ok();
    let output = child.wait_with_output().expect("wait for loglume");
    let _ = std::fs::remove_file(&path);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("initial line"));
    assert!(stdout.contains("err after start"));

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("2 lines,"),
        "expected updated footer, got: {stderr}"
    );
}
