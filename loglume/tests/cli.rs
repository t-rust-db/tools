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
