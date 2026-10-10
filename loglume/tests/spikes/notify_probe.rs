//! Spike: which notify configuration actually reports appends to a single
//! file on macOS? Probes four variants against a file being appended to,
//! and prints how many qualifying events each one saw.
//!
//! Run: cargo run --example notify_probe

use notify::{Config, PollWatcher, RecommendedWatcher, RecursiveMode, Watcher};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::mpsc::TryRecvError;
use std::thread;
use std::time::{Duration, Instant};

const PROBE_SECS: u64 = 3;

fn qualifying(rx: &mpsc::Receiver<notify::Result<notify::Event>>) -> (usize, Vec<String>) {
    let mut n = 0;
    let mut kinds = Vec::new();
    loop {
        match rx.try_recv() {
            Ok(Ok(ev)) => {
                if ev.kind.is_modify() || ev.kind.is_create() {
                    n += 1;
                    if kinds.len() < 3 {
                        kinds.push(format!("{:?} {:?}", ev.kind, ev.paths));
                    }
                }
            }
            Ok(Err(e)) => kinds.push(format!("ERR {e}")),
            Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
        }
    }
    (n, kinds)
}

fn appender(path: PathBuf, stop: std::sync::Arc<std::sync::atomic::AtomicBool>) {
    thread::spawn(move || {
        let mut i = 0;
        while !stop.load(std::sync::atomic::Ordering::Relaxed) {
            if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
                let _ = writeln!(f, "<30>Sep 14 00:00:00 host app[1]: spike line {i}");
                let _ = f.flush();
            }
            i += 1;
            thread::sleep(Duration::from_millis(200));
        }
    });
}

fn probe<W: Watcher>(
    label: &str,
    mut watcher: W,
    watch: &Path,
    mode: RecursiveMode,
    rx: mpsc::Receiver<notify::Result<notify::Event>>,
) {
    if let Err(e) = watcher.watch(watch, mode) {
        println!("{label:28} watch() failed: {e}");
        return;
    }
    let deadline = Instant::now() + Duration::from_secs(PROBE_SECS);
    let mut total = 0;
    let mut samples = Vec::new();
    while Instant::now() < deadline {
        thread::sleep(Duration::from_millis(100));
        let (n, k) = qualifying(&rx);
        total += n;
        if samples.len() < 3 {
            samples.extend(k);
        }
    }
    println!("{label:28} events={total}");
    for s in samples {
        println!("{:28}   e.g. {s}", "");
    }
}

fn main() -> notify::Result<()> {
    let rel = match std::env::args().nth(2) {
        Some(p) if !p.starts_with("--") => PathBuf::from(p),
        _ => PathBuf::from("tests/logs/spike.log"),
    };
    if !rel.exists() {
        std::fs::write(&rel, b"")?;
    }
    let abs = rel.canonicalize()?;
    let parent = abs.parent().unwrap_or(Path::new(".")).to_path_buf();

    // With --external, no built-in appender runs: point it at a file some
    // other process (e.g. tests/logs/producer.py) is appending to.
    let external = std::env::args().any(|a| a == "--external");
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    if !external {
        appender(abs.clone(), stop.clone());
    }
    println!(
        "appending to {} for {PROBE_SECS}s per probe\n",
        abs.display()
    );

    // 1. What loglume does today: recommended watcher on the relative path.
    let (tx, rx) = mpsc::channel();
    let w = RecommendedWatcher::new(
        move |res| {
            let _ = tx.send(res);
        },
        Config::default(),
    )?;
    probe(
        "recommended + relative",
        w,
        &rel,
        RecursiveMode::NonRecursive,
        rx,
    );

    // 2. Same, but on the canonicalized absolute path.
    let (tx, rx) = mpsc::channel();
    let w = RecommendedWatcher::new(
        move |res| {
            let _ = tx.send(res);
        },
        Config::default(),
    )?;
    probe(
        "recommended + canonical",
        w,
        &abs,
        RecursiveMode::NonRecursive,
        rx,
    );

    // 3. Watch the parent directory instead of the file.
    let (tx, rx) = mpsc::channel();
    let w = RecommendedWatcher::new(
        move |res| {
            let _ = tx.send(res);
        },
        Config::default(),
    )?;
    probe(
        "recommended + parent dir",
        w,
        &parent,
        RecursiveMode::NonRecursive,
        rx,
    );

    // 4. Explicit poll watcher, for comparison.
    let (tx, rx) = mpsc::channel();
    let w = PollWatcher::new(
        move |res| {
            let _ = tx.send(res);
        },
        Config::default().with_poll_interval(Duration::from_millis(200)),
    )?;
    probe("poll + canonical", w, &abs, RecursiveMode::NonRecursive, rx);

    // 5. Poll watcher at TUI tick speed, mtime comparison only.
    let (tx, rx) = mpsc::channel();
    let w = PollWatcher::new(
        move |res| {
            let _ = tx.send(res);
        },
        Config::default().with_poll_interval(Duration::from_millis(100)),
    )?;
    probe("poll 100ms mtime", w, &abs, RecursiveMode::NonRecursive, rx);

    // 6. Same, but hashing contents -- catches writes the mtime clock
    //    granularity hides, at the cost of re-reading the file each poll.
    let (tx, rx) = mpsc::channel();
    let w = PollWatcher::new(
        move |res| {
            let _ = tx.send(res);
        },
        Config::default()
            .with_poll_interval(Duration::from_millis(100))
            .with_compare_contents(true),
    )?;
    probe(
        "poll 100ms contents",
        w,
        &abs,
        RecursiveMode::NonRecursive,
        rx,
    );

    // 7. Plain size polling: one stat() per tick, no file contents read.
    //    The cheap signal an append always moves, unlike mtime (1s
    //    granularity here) and unlike FSEvents (silent on held-open fds).
    {
        let deadline = Instant::now() + Duration::from_secs(PROBE_SECS);
        let mut last = std::fs::metadata(&abs).map(|m| m.len()).unwrap_or(0);
        let mut changes = 0;
        while Instant::now() < deadline {
            thread::sleep(Duration::from_millis(100));
            let now = std::fs::metadata(&abs).map(|m| m.len()).unwrap_or(last);
            if now != last {
                changes += 1;
                last = now;
            }
        }
        println!("{:28} events={changes}", "size poll 100ms");
    }

    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    if !external {
        let _ = std::fs::remove_file(&rel);
    }
    Ok(())
}
