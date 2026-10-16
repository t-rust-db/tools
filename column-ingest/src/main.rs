// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0

//! `column-ingest`: pipe or follow events into a db-core table directory
//! (#14, db-core ADR 0029).
//!
//! ```text
//! app | column-ingest events/
//! column-ingest --follow /var/log/app.log events/
//! ```
//!
//! Everything it does goes through db-core's `Table` API: this binary only
//! parses arguments, runs the maintenance timer and handles signals.
//!
//! - A new table takes `--format`, `--flat`, `--sort-key` and
//!   `--retention-days`. An existing table keeps its format and layout (the
//!   manifest records them); the tier settings may change between runs.
//! - Every `--compact-every` seconds a second thread closes finished hours
//!   and days (a flat table: flushes its pages).
//! - SIGINT or SIGTERM stops intake, flushes, makes everything durable and
//!   exits 0. A record is durable once its batch is acknowledged, so a kill
//!   loses none of those; the next run recovers them from the WAL.

use std::io;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::Parser;
use db_core::storage::ingest::manifest::{format_from_name, Manifest};
use db_core::storage::ingest::{
    IngestError, Layout, LayoutKind, Table, TableOptions, TierOptions, WalError,
};
use signal_hook::consts::{SIGINT, SIGTERM};
use signal_hook::iterator::Signals;

#[derive(Debug, Parser)]
#[command(version, about)]
struct Args {
    /// The table directory (created if missing).
    table: PathBuf,
    /// Follow this log file instead of reading stdin.
    #[arg(long, value_name = "FILE")]
    follow: Option<PathBuf>,
    /// The line format of a new table: json (JSON Lines; also "jsonl"),
    /// logfmt, syslog, clf, freeform.
    #[arg(long, default_value = "json", value_parser = parse_format)]
    format: db_core::storage::stream::detect::Format,
    /// Create a flat table (one Parquet file per flush) instead of a tiered one.
    #[arg(long)]
    flat: bool,
    /// Columns a closed hour is sorted by, comma separated (tiered).
    #[arg(long, value_delimiter = ',', value_name = "COLUMNS")]
    sort_key: Vec<String>,
    /// Closed days to keep in archive/ (tiered); all if not given.
    #[arg(long, value_name = "DAYS")]
    retention_days: Option<u32>,
    /// Seconds between closing finished hours and days.
    #[arg(long, default_value_t = 10, value_name = "SECONDS")]
    compact_every: u64,
}

fn parse_format(name: &str) -> Result<db_core::storage::stream::detect::Format, String> {
    // db-core's stable name for JSON Lines is "json".
    let stable = if name == "jsonl" { "json" } else { name };
    format_from_name(stable)
        .ok_or_else(|| format!("unknown format {name:?}: json, logfmt, syslog, clf or freeform"))
}

/// The options to open `dir` with: an existing table's format and layout,
/// or the flags' for a new one.
fn options(args: &Args) -> Result<TableOptions, IngestError> {
    let existing = Manifest::read(&args.table)?;
    let format = existing.as_ref().map_or(args.format, |m| m.format);
    let tiered = existing
        .as_ref()
        .map_or(!args.flat, |m| m.layout == LayoutKind::Tiered);
    let layout = if tiered {
        Layout::Tiered(TierOptions {
            sort_key: args.sort_key.clone(),
            retention_days: args.retention_days,
            ..TierOptions::default()
        })
    } else {
        Layout::Flat
    };
    Ok(TableOptions {
        format,
        layout,
        ..TableOptions::default()
    })
}

/// Closes finished hours and days, or flushes a flat table.
fn maintain(table: &Table, tiered: bool) -> Result<(), IngestError> {
    if tiered {
        table.compact().map(drop)
    } else {
        table.flush().map(drop)
    }
}

/// Whether `e` only says the table was closed under us (by a signal).
fn closed(e: &IngestError) -> bool {
    matches!(e, IngestError::Wal(WalError::Closed))
}

/// Flushes what can be and closes the table; a table already closed is
/// fine.
fn shut_down(table: &Table) -> Result<(), IngestError> {
    match table.flush() {
        Ok(_) => {}
        Err(e) if closed(&e) => return Ok(()),
        Err(e) => return Err(e),
    }
    table.close()
}

fn run(args: &Args) -> Result<u64, IngestError> {
    let options = options(args)?;
    let tiered = matches!(options.layout, Layout::Tiered(_));
    let table = Table::open(&args.table, options)?;

    // Maintenance: every `compact_every` seconds until the table closes.
    let stop = Arc::new(AtomicBool::new(false));
    let maintenance = {
        let (table, stop) = (table.clone(), Arc::clone(&stop));
        let every = Duration::from_secs(args.compact_every.max(1));
        std::thread::spawn(move || {
            let mut last = Instant::now();
            while !stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(100));
                if last.elapsed() < every {
                    continue;
                }
                last = Instant::now();
                match maintain(&table, tiered) {
                    Ok(()) => {}
                    Err(e) if closed(&e) => break,
                    Err(e) => eprintln!("column-ingest: maintenance: {e}"),
                }
            }
        })
    };

    // Signals: stop intake, flush and close. Reading stdin blocks, so in
    // pipe mode the process then exits from here.
    let mut signals = Signals::new([SIGINT, SIGTERM])?;
    let handle = signals.handle();
    let piped = args.follow.is_none();
    let signal_thread = {
        let table = table.clone();
        let dir = args.table.clone();
        std::thread::spawn(move || {
            if signals.forever().next().is_some() {
                let code = match shut_down(&table) {
                    Ok(()) => 0,
                    Err(e) => {
                        eprintln!("column-ingest: {}: {e}", dir.display());
                        1
                    }
                };
                if piped {
                    std::process::exit(code);
                }
            }
        })
    };

    let ingested = match &args.follow {
        Some(path) => table.follow(path),
        None => table.ingest_lines(io::stdin().lock()),
    };
    stop.store(true, Ordering::Relaxed);
    let count = match ingested {
        Ok(n) => n,
        Err(e) if closed(&e) => 0,
        Err(e) => return Err(e),
    };
    shut_down(&table)?;
    handle.close();
    maintenance.join().ok();
    signal_thread.join().ok();
    Ok(count)
}

fn main() -> ExitCode {
    let args = Args::parse();
    match run(&args) {
        Ok(count) => {
            eprintln!(
                "column-ingest: {count} records into {}",
                args.table.display()
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("column-ingest: {}: {e}", args.table.display());
            ExitCode::FAILURE
        }
    }
}
