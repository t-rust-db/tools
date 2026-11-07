//! Tables in an object store (tools#123): `s3://bucket/prefix?endpoint=...`
//! names a table whose manifest and Parquet files live in an S3-compatible
//! store. These tests serve a table directory written by db-core's ingest
//! through a read-only fake S3 (GET and HEAD, path-style) and check that
//! column-rs answers as it does over the directory itself, refreshes per
//! query without fetching a file twice, and never writes to the store.

use column_rs::query::QueryEngine;
use db_core::storage::ingest::{Ack, Layout, TableOptions, TableWriter, TierOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Once, PoisonError};

const HOUR_NS: i64 = 3_600_000_000_000;
const DAY_NS: i64 = 24 * HOUR_NS;
const SECRET: &str = "s3-table-test-secret";

/// The tables' clock: shared, so tests that move it hold [`serial`].
static NOW: AtomicI64 = AtomicI64::new(0);
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(PoisonError::into_inner)
}

fn clock() -> i64 {
    NOW.load(Ordering::SeqCst)
}

/// 2026-10-11 plus `hours`.
fn set_clock(hours: i64) {
    NOW.store(20_737 * DAY_NS + hours * HOUR_NS, Ordering::SeqCst);
}

/// db-core reads credentials from the environment; any will do here.
fn credentials() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        std::env::set_var("AWS_ACCESS_KEY_ID", "test-key");
        std::env::set_var("AWS_SECRET_ACCESS_KEY", SECRET);
    });
}

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("column-rs-s3-{}-{n}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

fn tiered() -> TableOptions {
    TableOptions {
        layout: Layout::Tiered(TierOptions {
            row_group_max_rows: 7,
            delete_grace_ns: 0,
            ..TierOptions::default()
        }),
        clock,
        ..TableOptions::default()
    }
}

/// Appends `lines` to the table at `dir`, one an hour from `first_hour`,
/// then closes every day into `archive/`: a store holds no WAL, so only
/// flushed rows are in it.
fn append_archived(dir: &Path, first_hour: i64, lines: &[String]) {
    let mut w = TableWriter::open(dir, tiered()).unwrap();
    for (i, line) in lines.iter().enumerate() {
        set_clock(first_hour + i as i64);
        w.append(line.as_bytes(), Ack::Memory).unwrap();
    }
    set_clock(first_hour + lines.len() as i64 + 48);
    w.compact().unwrap();
    w.compact().unwrap(); // deletes the merged hours
    assert!(w.snapshot().unwrap().pages().is_empty());
    w.close().unwrap();
}

fn events(from: usize, n: usize) -> Vec<String> {
    (from..from + n)
        .map(|i| {
            let kind = ["view", "buy", "exit"][i % 3];
            format!(r#"{{"user":"u{}","kind":"{kind}","n":{}}}"#, i % 5, i % 4)
        })
        .collect()
}

fn users() -> Vec<String> {
    ["gold", "silver", "gold", "bronze"]
        .iter()
        .enumerate()
        .map(|(i, tier)| format!(r#"{{"user":"u{i}","tier":"{tier}"}}"#))
        .collect()
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// A read-only S3 over a local directory: bucket `bucket` is `root`,
/// path-style. It answers GET (whole or one `Range`) and HEAD, refuses
/// everything else with 405, and logs each request as `METHOD /path`.
struct FakeS3 {
    port: u16,
    log: Arc<Mutex<Vec<String>>>,
}

impl FakeS3 {
    fn serve(root: &Path) -> FakeS3 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let log = Arc::new(Mutex::new(Vec::new()));
        let (root, served) = (root.to_path_buf(), Arc::clone(&log));
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                answer(stream, &root, &served);
            }
        });
        FakeS3 { port, log }
    }

    /// The table `prefix` of the bucket, as column-rs takes it.
    fn location(&self, prefix: &str) -> String {
        format!(
            "s3://bucket/{prefix}?endpoint=http://127.0.0.1:{}&region=test",
            self.port
        )
    }

    /// The requests since the last call.
    fn take(&self) -> Vec<String> {
        std::mem::take(&mut *self.log.lock().unwrap())
    }
}

fn answer(stream: TcpStream, root: &Path, log: &Mutex<Vec<String>>) {
    let mut reader = BufReader::new(stream);
    let mut head = Vec::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
            break;
        }
        head.push(line.trim_end().to_string());
    }
    let Some(request) = head.first() else { return };
    let mut parts = request.split(' ');
    let (method, target) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    let path = percent_decode(target.split('?').next().unwrap_or(""));
    log.lock().unwrap().push(format!("{method} {path}"));
    let length = header(&head, "content-length").map_or(0, |v| v.parse().unwrap_or(0));
    let mut body = vec![0; length];
    reader.read_exact(&mut body).ok();
    let file = path.strip_prefix("/bucket/").map(|key| root.join(key));
    let mut etag = String::new();
    let (status, bytes) = match (method, file) {
        ("GET" | "HEAD", Some(file)) if file.is_file() => {
            let bytes = std::fs::read(file).unwrap();
            etag = format!("ETag: \"{:016x}\"\r\n", fnv1a(&bytes));
            match header(&head, "range").and_then(|r| r.strip_prefix("bytes=")) {
                Some(range) => {
                    let (from, to) = range.split_once('-').unwrap();
                    let from: usize = from.parse().unwrap();
                    let to = to
                        .parse::<usize>()
                        .map_or(bytes.len(), |t| (t + 1).min(bytes.len()));
                    ("206 Partial Content", bytes[from..to].to_vec())
                }
                None => ("200 OK", bytes),
            }
        }
        ("GET" | "HEAD", _) => (
            "404 Not Found",
            b"<Error><Code>NoSuchKey</Code></Error>".to_vec(),
        ),
        _ => ("405 Method Not Allowed", Vec::new()),
    };
    let mut stream = reader.into_inner();
    let reply = format!(
        "HTTP/1.1 {status}\r\n{etag}Content-Length: {}\r\nConnection: close\r\n\r\n",
        bytes.len()
    );
    stream.write_all(reply.as_bytes()).ok();
    if method != "HEAD" {
        stream.write_all(&bytes).ok();
    }
}

/// The object's ETag: any stable hash of its bytes.
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    })
}

fn header<'h>(head: &'h [String], name: &str) -> Option<&'h str> {
    head.iter().skip(1).find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.trim().eq_ignore_ascii_case(name).then(|| value.trim())
    })
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap()
}

/// `sql` through column-rs, each row's cells as text.
fn rows(engine: &QueryEngine, sql: &str) -> Vec<Vec<String>> {
    let result = engine.execute(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    result
        .rows()
        .map(|row| row.iter().map(ToString::to_string).collect())
        .collect()
}

/// An events table (and a users table) on disk, served by a fake S3.
struct Fixture {
    _dir: TempDir,
    events: PathBuf,
    users: PathBuf,
    s3: FakeS3,
}

fn fixture_tables() -> Fixture {
    credentials();
    let dir = TempDir::new();
    let (events_dir, users_dir) = (dir.0.join("events"), dir.0.join("users"));
    append_archived(&events_dir, 0, &events(0, 30));
    append_archived(&users_dir, 0, &users());
    let s3 = FakeS3::serve(&dir.0);
    Fixture {
        _dir: dir,
        events: events_dir,
        users: users_dir,
        s3,
    }
}

const QUERIES: &[&str] = &[
    "SELECT count(*), sum(n) FROM events",
    "SELECT kind, count(*) FROM events GROUP BY kind ORDER BY kind",
    "SELECT user, n FROM events WHERE kind = 'buy' ORDER BY user, n LIMIT 5",
    "SELECT user, n, row_number() OVER (PARTITION BY user ORDER BY n) FROM events ORDER BY user, n LIMIT 6",
    "SELECT users.tier, count(*) FROM events JOIN users ON events.user = users.user GROUP BY users.tier ORDER BY users.tier",
    "SELECT count(*) FROM events WHERE user IN (SELECT user FROM users WHERE tier = 'gold')",
];

#[test]
fn an_s3_table_answers_as_its_local_directory_does() {
    let _serial = serial();
    let f = fixture_tables();
    let local = QueryEngine::open_many(&[f.events.clone(), f.users.clone()]).unwrap();
    let remote = QueryEngine::open_many(&[
        PathBuf::from(f.s3.location("events")),
        PathBuf::from(f.s3.location("users")),
    ])
    .unwrap();
    assert_eq!(remote.table_names(), ["events", "users"]);
    for sql in QUERIES {
        let expected = rows(&local, sql);
        assert!(!expected.is_empty(), "{sql}");
        assert_eq!(rows(&remote, sql), expected, "{sql}");
    }
    // Joined to a local table directory too.
    let mixed =
        QueryEngine::open_many(&[PathBuf::from(f.s3.location("events")), f.users.clone()]).unwrap();
    for sql in QUERIES {
        assert_eq!(rows(&mixed, sql), rows(&local, sql), "{sql}");
    }
    let requests = f.s3.take();
    assert!(
        requests
            .iter()
            .all(|r| r.starts_with("GET ") || r.starts_with("HEAD ")),
        "a reader only reads: {requests:?}"
    );
}

#[test]
fn an_s3_table_joins_sqlite_tables_and_parquet_files() {
    let _serial = serial();
    let f = fixture_tables();
    let queries = [
        "SELECT labels.label, count(*) FROM events JOIN labels ON events.n = labels.id GROUP BY labels.label ORDER BY labels.label",
        "SELECT count(*) FROM events JOIN production ON events.n = production.id",
    ];
    let local = QueryEngine::open_many(&[
        f.events.clone(),
        fixture("lookups.db"),
        fixture("production.parquet"),
    ])
    .unwrap();
    let remote = QueryEngine::open_many(&[
        PathBuf::from(f.s3.location("events")),
        fixture("lookups.db"),
        fixture("production.parquet"),
    ])
    .unwrap();
    for sql in queries {
        let expected = rows(&local, sql);
        assert!(!expected.is_empty(), "{sql}");
        assert_eq!(rows(&remote, sql), expected, "{sql}");
    }
}

#[test]
fn each_query_refreshes_and_fetches_only_new_files() {
    let _serial = serial();
    let f = fixture_tables();
    let engine = QueryEngine::open(Path::new(&f.s3.location("events"))).unwrap();
    let parquet = |requests: &[String]| -> Vec<String> {
        requests
            .iter()
            .filter(|r| r.ends_with(".parquet"))
            .cloned()
            .collect()
    };
    let opened = f.s3.take();
    assert!(!parquet(&opened).is_empty(), "{opened:?}");

    // Nothing changed: the manifest is read again, no file is fetched.
    assert_eq!(rows(&engine, "SELECT count(*) FROM events"), [["30"]]);
    let unchanged = f.s3.take();
    assert!(
        unchanged.iter().any(|r| r.ends_with("/manifest.json")),
        "{unchanged:?}"
    );
    assert_eq!(parquet(&unchanged), Vec::<String>::new());

    // A new day's file: the next query sees its rows and fetches only it.
    append_archived(&f.events, 30 * 24, &events(30, 12));
    assert_eq!(rows(&engine, "SELECT count(*) FROM events"), [["42"]]);
    let grown = parquet(&f.s3.take());
    assert!(!grown.is_empty());
    assert!(
        grown.iter().all(|r| !opened.contains(r)),
        "refetched a file it already had: {grown:?}"
    );
}

#[test]
fn an_s3_table_works_from_the_cli_and_keeps_credentials_out_of_errors() {
    let _serial = serial();
    let f = fixture_tables();
    let run = |location: &str| {
        Command::new(env!("CARGO_BIN_EXE_column-rs"))
            .args(["-c", "SELECT count(*) FROM events", location])
            .env("AWS_ACCESS_KEY_ID", "test-key")
            .env("AWS_SECRET_ACCESS_KEY", SECRET)
            .output()
            .unwrap()
    };
    let out = run(&f.s3.location("events"));
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("30"));

    let out = run(&f.s3.location("missing"));
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("s3://bucket/missing"), "{stderr}");
    assert!(!stderr.contains(SECRET), "{stderr}");
}

#[test]
fn physical_describes_an_s3_table_without_a_wal() {
    let _serial = serial();
    let f = fixture_tables();
    let location = f.s3.location("events");
    let report = column_rs::physical::describe(
        "events",
        Path::new(&location),
        column_rs::physical::Options::default(),
    )
    .unwrap();
    let text = report.join("\n");
    assert!(text.contains("(object store s3://bucket/events"), "{text}");
    assert!(text.contains("flushed files only"), "{text}");
    assert!(text.contains("30 in files"), "{text}");
}
