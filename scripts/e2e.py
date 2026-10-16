#!/usr/bin/env python3
"""End-to-end event pipeline (tools#47): gen_events.py -> column-ingest ->
column-rs, checked against DuckDB.

1. Flat: generated events and transactions are piped into `column-ingest
   --flat`. Every generated record is in the table once. column-rs over
   the table directories agrees with DuckDB over their Parquet files on
   aggregates, a join of events to transactions and a window function.
2. Tiered, live: events are piped in at a paced rate while column-rs
   queries the directory. The count grows, never shrinks, and ends at
   what was generated.

column-ingest tiers by arrival time, so a run never closes a day: the
oracle reads the flat tables' `data/`, and the tiered one checks reads
while ingest runs.

Usage: scripts/e2e.py [--bin target/release]   (run by `make e2e`)
"""

import argparse
import re
import subprocess
import sys
import tempfile
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
GEN = [sys.executable, str(HERE / "gen_events.py")]

# Each query runs as `column-rs -c` over the table directories and as
# DuckDB over their files, with `{events}`/`{txns}` naming the tables.
ORACLE_QUERIES = [
    "SELECT event_id, COUNT(*), MIN(request_id), MAX(request_id) FROM {events} "
    "GROUP BY event_id ORDER BY event_id",
    "SELECT COUNT(*), SUM(request_id), COUNT(score), COUNT(value) FROM {events}",
    "SELECT {txns}.channel, COUNT(*) FROM {events} JOIN {txns} "
    "ON {events}.session_id = {txns}.session_id GROUP BY {txns}.channel ORDER BY {txns}.channel",
    "SELECT event_id, COUNT(*) OVER (PARTITION BY event_id) FROM {events} ORDER BY event_id",
]


def normalized(rows):
    """Cells compared by value: db-core's batch engine returns MIN/MAX/SUM
    of an integer column as a float (`0.0`), DuckDB as an integer (`0`)."""

    def cell(text):
        try:
            return ("number", round(float(text), 9))
        except ValueError:
            return ("text", text)

    return [[cell(c) for c in row] for row in rows]


def fail(message):
    print(f"e2e: FAIL: {message}", file=sys.stderr)
    sys.exit(1)


def generate_into(bin_dir, table, gen_args, ingest_args=()):
    """Pipes gen_events.py into column-ingest; returns (generated, ingested)."""
    gen = subprocess.Popen(GEN + gen_args + ["--summary"], stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    ingest = subprocess.run(
        [str(bin_dir / "column-ingest"), *ingest_args, str(table)],
        stdin=gen.stdout,
        capture_output=True,
        text=True,
        check=False,
    )
    gen.stdout.close()
    gen_err = gen.stderr.read().decode()
    if gen.wait() != 0 or ingest.returncode != 0:
        fail(f"generate into {table}: {gen_err} {ingest.stderr}")
    generated = int(re.search(r"records=(\d+)", gen_err).group(1))
    ingested = int(re.search(r"(\d+) records into", ingest.stderr).group(1))
    return generated, ingested


def column_rs(bin_dir, tables, sql):
    """`sql` through column-rs over `tables`; rows of text cells."""
    out = subprocess.run(
        [str(bin_dir / "column-rs"), "-c", sql, *map(str, tables)],
        capture_output=True,
        text=True,
        check=False,
    )
    if out.returncode != 0:
        fail(f"column-rs: {sql}: {out.stderr}")
    lines = out.stdout.splitlines()
    return [line.split("\t") for line in lines[1:]]  # without the header


def duckdb(sql):
    out = subprocess.run(
        ["duckdb", "-csv", "-noheader", "-nullvalue", "NULL", "-c", sql],
        capture_output=True,
        text=True,
        check=False,
    )
    if out.returncode != 0:
        fail(f"duckdb: {sql}: {out.stderr}")
    return [line.split(",") for line in out.stdout.splitlines()]


def flat_oracle(bin_dir, work):
    events, txns = work / "events", work / "txns"
    common = ["--sessions", "800", "--sps", "20", "--seed", "11"]
    for table, kind in [(events, "events"), (txns, "transactions")]:
        generated, ingested = generate_into(bin_dir, table, common + ["--kind", kind], ["--flat"])
        stored = int(column_rs(bin_dir, [table], f"SELECT COUNT(*) FROM {table.name}")[0][0])
        if not generated == ingested == stored:
            fail(f"{kind}: generated {generated}, ingested {ingested}, stored {stored}")
        print(f"e2e: {kind}: {generated} generated = ingested = stored")
    files = {
        "events": f"read_parquet('{events}/data/*.parquet', union_by_name = true)",
        "txns": f"read_parquet('{txns}/data/*.parquet', union_by_name = true)",
    }
    for template in ORACLE_QUERIES:
        ours = column_rs(bin_dir, [events, txns], template.format(events="events", txns="txns"))
        # DuckDB names the file readers by alias, as column-rs names the
        # directories by name.
        sql = template.format(events="events", txns="txns")
        for name, reader in files.items():
            sql = re.sub(rf"\bFROM {name}\b", f"FROM {reader} {name}", sql)
            sql = re.sub(rf"\bJOIN {name}\b", f"JOIN {reader} {name}", sql)
        theirs = duckdb(sql)
        if not theirs:
            fail(f"an empty oracle proves nothing: {sql}")
        if normalized(ours) != normalized(theirs):
            fail(f"column-rs and DuckDB disagree on {template}:\n  ours   {ours[:5]}\n  theirs {theirs[:5]}")
        print(f"e2e: agrees with DuckDB ({len(ours)} rows): {template[:60]}...")


def tiered_live(bin_dir, work):
    table = work / "live"
    gen = subprocess.Popen(
        GEN + ["--sessions", "60", "--e", "3", "--seed", "5", "--rate", "1500", "--summary"],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    ingest = subprocess.Popen(
        [str(bin_dir / "column-ingest"), "--compact-every", "1", str(table)],
        stdin=gen.stdout,
        stderr=subprocess.PIPE,
        text=True,
    )
    gen.stdout.close()
    seen = []
    while ingest.poll() is None:
        time.sleep(0.2)
        if (table / "manifest.json").exists():
            seen.append(int(column_rs(bin_dir, [table], "SELECT COUNT(*) FROM live")[0][0]))
    gen_err = gen.stderr.read().decode()
    if gen.wait() != 0 or ingest.returncode != 0:
        fail(f"live ingest: {gen_err} {ingest.stderr.read()}")
    generated = int(re.search(r"records=(\d+)", gen_err).group(1))
    final = int(column_rs(bin_dir, [table], "SELECT COUNT(*) FROM live")[0][0])
    growing = [n for n in seen if 0 < n < generated]
    if seen != sorted(seen):
        fail(f"a read saw fewer records than an earlier one: {seen}")
    if not growing:
        fail(f"no read saw the table while it grew: {seen}")
    if final != generated:
        fail(f"live: generated {generated}, stored {final}")
    print(f"e2e: live: {len(seen)} reads while ingesting, {len(growing)} mid-way, never fewer; {final} stored")


def main():
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("--bin", type=Path, default=Path("target/release"), help="where column-ingest and column-rs are")
    args = p.parse_args()
    bin_dir = args.bin.resolve()
    for tool in ("column-ingest", "column-rs"):
        if not (bin_dir / tool).exists():
            fail(f"{bin_dir / tool} not built (cargo build --release -p {tool})")
    with tempfile.TemporaryDirectory(prefix="tools-e2e-") as tmp:
        work = Path(tmp)
        flat_oracle(bin_dir, work)
        tiered_live(bin_dir, work)
    print("e2e: ok")


if __name__ == "__main__":
    main()
