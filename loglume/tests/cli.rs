//! Integration tests driving the compiled `loglume` binary against the
//! committed `tests/logs/sample.log` fixture (deterministic: `--seed 42`).

use assert_cmd::Command;
use predicates::prelude::*;

const SAMPLE_LOG: &str = "tests/logs/sample.log";
const SAMPLE_LOG_2: &str = "tests/logs/sample2.log";

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

#[test]
fn multiple_files_without_tui_are_rejected() {
    Command::cargo_bin("loglume")
        .unwrap()
        .args(["severity >= WARN", SAMPLE_LOG, SAMPLE_LOG_2])
        .assert()
        .failure()
        .stderr(predicate::str::contains("multiple files require --tui"));
}

#[test]
fn multiple_files_with_tui_pass_argument_validation() {
    // No real TTY is available under the test harness, so --tui itself
    // fails fast once it tries to initialize the terminal -- but that
    // failure is distinct from (and happens after) the CLI's own
    // "multiple files require --tui" argument-validation rejection above,
    // which proves multiple files + --tui are accepted as a valid
    // combination before terminal setup is even attempted.
    Command::cargo_bin("loglume")
        .unwrap()
        .args(["severity >= WARN", "--tui", SAMPLE_LOG, SAMPLE_LOG_2])
        .assert()
        .failure()
        .stderr(predicate::str::contains("multiple files require --tui").not());
}

#[test]
fn sample2_fixture_has_distinct_content_from_sample() {
    // Sanity check for the fixture itself: it must actually differ from
    // sample.log (different seed/line count), or the multi-pane tests
    // that rely on "two distinct files" wouldn't prove anything.
    let sample_lines = std::fs::read_to_string(SAMPLE_LOG).unwrap().lines().count();
    let sample2_lines = std::fs::read_to_string(SAMPLE_LOG_2)
        .unwrap()
        .lines()
        .count();
    assert_ne!(sample_lines, sample2_lines);
}

/// A tempdir to use as an isolated XDG_CONFIG_HOME, so these tests never
/// touch the real user's config. Set via `.env(...)` on the child process
/// only -- never `std::env::set_var` on this test process itself, which
/// would be a thread-safety hazard under parallel test execution.
fn isolated_xdg_config_home(test_name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "loglume-xdg-test-{test_name}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

#[test]
fn config_subcommand_prints_resolved_path_and_defaults() {
    let xdg = isolated_xdg_config_home("print-defaults");
    Command::cargo_bin("loglume")
        .unwrap()
        .args(["config"])
        .env("XDG_CONFIG_HOME", &xdg)
        .assert()
        .success()
        .stdout(
            predicate::str::contains(xdg.to_str().unwrap())
                .and(predicate::str::contains("[filters]")),
        );
    let _ = std::fs::remove_dir_all(&xdg);
}

#[test]
fn save_filter_persists_and_is_reusable_via_at_name() {
    let xdg = isolated_xdg_config_home("save-and-reuse");

    Command::cargo_bin("loglume")
        .unwrap()
        .args(["severity >= WARN", "--save-filter", "myerr", SAMPLE_LOG])
        .env("XDG_CONFIG_HOME", &xdg)
        .assert()
        .success();

    Command::cargo_bin("loglume")
        .unwrap()
        .args(["config"])
        .env("XDG_CONFIG_HOME", &xdg)
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "myerr = \"SELECT * FROM log WHERE severity >= 'WARN'\"",
        ));

    let direct = Command::cargo_bin("loglume")
        .unwrap()
        .args(["severity >= WARN", SAMPLE_LOG])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let via_saved = Command::cargo_bin("loglume")
        .unwrap()
        .args(["@myerr", SAMPLE_LOG])
        .env("XDG_CONFIG_HOME", &xdg)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    assert_eq!(direct, via_saved);
    let _ = std::fs::remove_dir_all(&xdg);
}

#[test]
fn unknown_saved_filter_name_errors_clearly() {
    let xdg = isolated_xdg_config_home("unknown-name");
    Command::cargo_bin("loglume")
        .unwrap()
        .args(["@doesnotexist", SAMPLE_LOG])
        .env("XDG_CONFIG_HOME", &xdg)
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "no saved filter named 'doesnotexist'",
        ));
    let _ = std::fs::remove_dir_all(&xdg);
}
