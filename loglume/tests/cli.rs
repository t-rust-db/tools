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
            "myerr = \"SELECT * FROM log WHERE severity >= 13\"",
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

#[test]
fn highlight_marks_matching_lines_without_hiding_others() {
    // A broad filter (matches nearly everything) with a narrow highlight:
    // both the highlighted and non-highlighted lines must still appear.
    let assert = Command::cargo_bin("loglume")
        .unwrap()
        .args([
            "severity >= DEBUG",
            "--highlight",
            "severity >= ERR",
            SAMPLE_LOG,
        ])
        .assert()
        .success();
    let output = assert.get_output();
    let stdout = String::from_utf8_lossy(&output.stdout);

    let total_lines = stdout.lines().count();
    let highlighted_lines = stdout.lines().filter(|l| l.contains("\x1b[1;43m")).count();
    assert!(
        highlighted_lines > 0,
        "expected at least one highlighted line"
    );
    assert!(
        highlighted_lines < total_lines,
        "expected some lines to remain unhighlighted (restrict vs. annotate)"
    );
}

#[test]
fn highlight_accepts_a_raw_expression_not_just_short_forms() {
    Command::cargo_bin("loglume")
        .unwrap()
        .args([
            "severity >= DEBUG",
            "--highlight",
            "message LIKE '%oom%'",
            SAMPLE_LOG,
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("\x1b[1;43m"));
}

#[test]
fn without_highlight_no_ansi_codes_appear() {
    Command::cargo_bin("loglume")
        .unwrap()
        .args(["severity >= DEBUG", SAMPLE_LOG])
        .assert()
        .success()
        .stdout(predicate::str::contains("\x1b[").not());
}

#[test]
fn alert_without_exec_is_rejected() {
    Command::cargo_bin("loglume")
        .unwrap()
        .args(["severity >= WARN", "--alert", SAMPLE_LOG])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--alert requires --exec"));
}

#[test]
fn alert_op_without_threshold_is_rejected() {
    Command::cargo_bin("loglume")
        .unwrap()
        .args([
            "severity >= WARN",
            "--alert",
            "--exec",
            "cat",
            "--alert-op",
            ">=",
            SAMPLE_LOG,
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "--alert-op and --alert-threshold must be given together",
        ));
}

#[test]
fn alert_invalid_window_is_rejected() {
    Command::cargo_bin("loglume")
        .unwrap()
        .args([
            "severity >= WARN",
            "--alert",
            "--exec",
            "cat",
            "--window",
            "bogus",
            SAMPLE_LOG,
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("invalid duration"));
}

#[test]
fn alert_requires_a_file_argument() {
    Command::cargo_bin("loglume")
        .unwrap()
        .args(["severity >= WARN", "--alert", "--exec", "cat"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--alert requires a file argument"));
}

#[test]
fn alert_fires_exec_on_a_new_matching_line() {
    use std::io::Write as _;
    use std::process::{Command as StdCommand, Stdio};
    use std::time::Duration;

    let dir = std::env::temp_dir();
    let path = dir.join(format!("loglume-alert-cli-test-{}.log", std::process::id()));
    let out_path = dir.join(format!("loglume-alert-cli-test-{}.out", std::process::id()));
    std::fs::write(&path, "<134>Sep 9 08:00:00 host app[1]: normal line\n").unwrap();
    let _ = std::fs::remove_file(&out_path);

    let bin = assert_cmd::cargo::cargo_bin("loglume");
    let mut child = StdCommand::new(bin)
        .args([
            "severity >= WARN",
            "--alert",
            "--window",
            "1s",
            "--exec",
            &format!("cat >> {}", out_path.display()),
            path.to_str().unwrap(),
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn loglume --alert");

    std::thread::sleep(Duration::from_millis(300));
    assert!(
        !out_path.exists() || std::fs::read_to_string(&out_path).unwrap().is_empty(),
        "must not fire before any WARN+ line exists"
    );

    {
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(f, "<131>Sep 9 08:00:01 host app[1]: something broke").unwrap();
    }
    std::thread::sleep(Duration::from_millis(700));

    child.kill().ok();
    let _ = child.wait_with_output();
    let fired = std::fs::read_to_string(&out_path).unwrap_or_default();
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&out_path);

    assert!(
        fired.contains("something broke"),
        "expected --exec to have received the fired row, got: {fired:?}"
    );
}

// Test corpus generators (#3): gen_access.py, gen_jsonl.py, gen_logfmt.py.
// These confirm loglume's format auto-detection actually parses each
// generated fixture correctly, not just that the generator scripts run.

const ACCESS_LOG: &str = "tests/logs/access.log";
const JSONL_LOG: &str = "tests/logs/sample.jsonl";
const DOCKER_JSONL_LOG: &str = "tests/logs/docker.jsonl";
const SPARSE_JSONL_LOG: &str = "tests/logs/sparse.jsonl";
const LOGFMT_LOG: &str = "tests/logs/sample.logfmt";

#[test]
fn access_log_fixture_is_auto_detected_and_queryable() {
    Command::cargo_bin("loglume")
        .unwrap()
        .args(["select count(*) from log", ACCESS_LOG])
        .assert()
        .success()
        .stdout(predicate::str::contains("200"));
}

#[test]
fn access_log_status_column_is_queryable() {
    // Confirms `status` was typed as an Int column (CLF has no severity,
    // per db-core's own ADR-0018 amendment -- status is what's queried).
    Command::cargo_bin("loglume")
        .unwrap()
        .args(["select count(*) from log where status >= 500", ACCESS_LOG])
        .assert()
        .success();
}

#[test]
fn jsonl_fixture_is_auto_detected_and_queryable() {
    Command::cargo_bin("loglume")
        .unwrap()
        .args(["select count(*) from log", JSONL_LOG])
        .assert()
        .success()
        .stdout(predicate::str::contains("200"));
}

#[test]
fn jsonl_docker_wrapped_fixture_is_queryable() {
    // The container-unwrap path (#319): each line is a Docker json-file
    // envelope wrapping a syslog payload, not a plain Pino record.
    Command::cargo_bin("loglume")
        .unwrap()
        .args(["select count(*) from log", DOCKER_JSONL_LOG])
        .assert()
        .success()
        .stdout(predicate::str::contains("50"));
}

#[test]
fn jsonl_sparse_fixture_is_queryable() {
    // Heterogeneous per-line key sets (extra random fields on ~30% of
    // lines) -- exercises FieldStore's schema-on-read handling.
    Command::cargo_bin("loglume")
        .unwrap()
        .args(["select count(*) from log", SPARSE_JSONL_LOG])
        .assert()
        .success()
        .stdout(predicate::str::contains("100"));
}

#[test]
fn logfmt_fixture_is_auto_detected_and_queryable() {
    Command::cargo_bin("loglume")
        .unwrap()
        .args(["select count(*) from log", LOGFMT_LOG])
        .assert()
        .success()
        .stdout(predicate::str::contains("200"));
}

#[test]
fn logfmt_service_field_is_queryable() {
    Command::cargo_bin("loglume")
        .unwrap()
        .args([
            "select count(*) from log where service = 'auth'",
            LOGFMT_LOG,
        ])
        .assert()
        .success();
}
