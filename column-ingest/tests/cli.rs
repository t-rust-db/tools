// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0

//! column-ingest end to end: the binary, a table directory, and db-core's
//! `Snapshot` reading what it wrote from another process.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use assert_cmd::cargo::CommandCargoExt;
use db_core::storage::ingest::{LayoutKind, Snapshot};

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("column-ingest-{}-{n}", std::process::id()));
        fs::remove_dir_all(&dir).ok();
        fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }

    fn table(&self) -> PathBuf {
        self.0.join("events")
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).ok();
    }
}

fn command() -> Command {
    Command::cargo_bin("column-ingest").unwrap()
}

/// Runs column-ingest on `args` with `input` on stdin; returns its stderr.
fn pipe(args: &[&str], input: &str) -> (bool, String) {
    let mut child = command()
        .args(args)
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Records in the table at `dir`, read from this process.
fn rows(dir: &Path) -> u64 {
    Snapshot::open(dir).map_or(0, |s| s.rows())
}

/// Waits up to 10 s for the table at `dir` to hold `n` records.
fn wait_for(dir: &Path, n: u64) {
    let start = Instant::now();
    while rows(dir) < n {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "timed out at {n}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn signal(child: &Child, name: &str) {
    let status = Command::new("kill")
        .args([&format!("-{name}"), &child.id().to_string()])
        .status()
        .unwrap();
    assert!(status.success());
}

fn append_to(path: &Path, text: &str) {
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    f.write_all(text.as_bytes()).unwrap();
}

#[test]
fn piped_lines_land_in_a_tiered_table_by_default() {
    let t = TempDir::new();
    let dir = t.table();
    let (ok, stderr) = pipe(
        &[dir.to_str().unwrap()],
        "{\"a\":1}\n{\"a\":2}\n\n{\"a\":3}\n",
    );
    assert!(ok, "{stderr}");
    assert!(stderr.contains("3 records into"), "{stderr}");
    let snap = Snapshot::open(&dir).unwrap();
    assert_eq!(snap.rows(), 3);
    assert_eq!(snap.manifest().layout, LayoutKind::Tiered);
}

#[test]
fn a_flat_table_is_flushed_on_exit() {
    let t = TempDir::new();
    let dir = t.table();
    let (ok, stderr) = pipe(&["--flat", dir.to_str().unwrap()], "one\ntwo\n");
    assert!(ok, "{stderr}");
    let snap = Snapshot::open(&dir).unwrap();
    assert_eq!(snap.manifest().layout, LayoutKind::Flat);
    assert_eq!(
        snap.manifest().flushed_lsn,
        2,
        "in Parquet, not just the WAL"
    );
    assert!(snap.pages().is_empty());
}

#[test]
fn an_existing_table_keeps_its_format_and_layout() {
    let t = TempDir::new();
    let dir = t.table();
    let path = dir.to_str().unwrap();
    assert!(pipe(&["--flat", "--format", "logfmt", path], "a=1\n").0);
    // Other flags on a later run do not change what the table is.
    let (ok, stderr) = pipe(&[path], "a=2\n");
    assert!(ok, "{stderr}");
    let snap = Snapshot::open(&dir).unwrap();
    assert_eq!(snap.manifest().layout, LayoutKind::Flat);
    assert_eq!(snap.manifest().format.as_str(), "logfmt");
    assert_eq!(snap.rows(), 2);
}

#[test]
fn jsonl_is_accepted_as_a_name_for_json_lines() {
    let t = TempDir::new();
    let dir = t.table();
    assert!(pipe(&["--format", "jsonl", dir.to_str().unwrap()], "{}\n").0);
    let snap = Snapshot::open(&dir).unwrap();
    assert_eq!(snap.manifest().format.as_str(), "json");
}

#[test]
fn an_unknown_format_is_refused() {
    let t = TempDir::new();
    let out = command()
        .args(["--format", "csv", t.table().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("unknown format"));
}

#[test]
fn follow_appends_until_sigterm_then_resumes_without_duplicates() {
    let t = TempDir::new();
    let dir = t.table();
    let log = t.0.join("app.log");
    append_to(&log, "{\"n\":1}\n{\"n\":2}\n");
    let follow = |dir: &Path| {
        command()
            .args(["--follow", log.to_str().unwrap(), dir.to_str().unwrap()])
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    };
    let child = follow(&dir);
    wait_for(&dir, 2);
    append_to(&log, "{\"n\":3}\n");
    wait_for(&dir, 3);
    signal(&child, "TERM");
    let out = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert!(stderr.contains("3 records into"), "{stderr}");

    append_to(&log, "{\"n\":4}\n");
    let child = follow(&dir);
    wait_for(&dir, 4);
    signal(&child, "INT");
    assert!(child.wait_with_output().unwrap().status.success());
    assert_eq!(rows(&dir), 4, "nothing appended twice");
}

#[test]
fn a_kill_loses_nothing_that_was_durable() {
    let t = TempDir::new();
    let dir = t.table();
    let log = t.0.join("app.log");
    let lines: String = (0..50).map(|i| format!("{{\"i\":{i}}}\n")).collect();
    append_to(&log, &lines);
    let child = command()
        .args(["--follow", log.to_str().unwrap(), dir.to_str().unwrap()])
        .spawn()
        .unwrap();
    wait_for(&dir, 50);
    signal(&child, "KILL");
    let mut child = child;
    child.wait().unwrap();
    // The WAL holds them; a reader and the next writer both see them.
    assert_eq!(rows(&dir), 50);
    assert!(
        pipe(&[dir.to_str().unwrap()], "").0,
        "reopens after the kill"
    );
    assert_eq!(rows(&dir), 50);
}
