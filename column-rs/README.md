# column-rs

A verified columnar analytics engine over Parquet — zero-unsafe-code-
by-default, part of the t-rust-db family (alongside sqlite-rs and
loglume). column-rs reads Parquet files and answers SQL queries against
them; it never writes.

## What's here

column-rs itself is the query engine and CLI. The Parquet reader, SQL
parser, storage abstraction, and REPL infrastructure are dependencies
(see `Cargo.toml`):

| Crate | Provides |
|-------|----------|
| [`db-core`](https://github.com/t-rust-db/db-core)'s `storage::column` | Parquet reader (footer/page/schema parsing, encodings, compression) and the `Vfs`/`VfsFile` file abstraction (formerly db-parquet and db-storage, db-core ADR 0016) |
| [`db-core`](https://github.com/t-rust-db/db-core)'s `parser` | SQL AST and parser |
| [`db-core`](https://github.com/t-rust-db/db-core)'s `vm` | Batch VM, including the hash table for equi-joins |
| [`db-cli`](../db-cli/) | Generic `ReplHandler` trait, line editing, output rendering |

What stays in this repo:

- `src/query.rs` — the Parquet glue only: resolve a planned program's
  columns against a file's leaf schema, expose each row group as a
  `Segment`, materialize whole tables for join/semi-join/window queries,
  and `QueryEngine` (table registry + dispatch). The planner
  (`db_core::codegen::batch`), the VM (`db_core::vm::batch`) and the
  cross-segment engine that applies the terminal `Combine`/`Sort`/`Limit` opcodes
  (`db_core::vm::engine`) all live in db-core (its ADR 0007).
- `src/bin/column-rs/` — the CLI binary: REPL (via `db_cli::run_repl`),
  one-shot `-c` mode, `codegen` subcommand (wraps db-core's AOT emitter,
  `db_core::emit::batch`)

`src/lib.rs` re-exports `column_rs::sql::*` and `column_rs::file::*` as
thin compatibility shims over db-core's `parser` and `storage::column`
respectively, so code written against column-rs's pre-
restructure public API still compiles.

## Grammar

The exact SQL subset `sql-parser` accepts is documented as EBNF in
[`t-rust-db/grammar`](https://github.com/t-rust-db/grammar) —
`column-rs.ebnf`, derived directly from the parser (not aspirational).

## Usage

```bash
# Interactive REPL — table names are each file's stem
column-rs orders.parquet customers.parquet

# One-shot mode
column-rs -c "SELECT id, amount FROM orders WHERE amount > 1000" orders.parquet

# A table directory (written by column-ingest) is a table named after it
column-rs -c "SELECT event_id, COUNT(*) FROM events GROUP BY event_id" events/

# Pipe mode: JSON Lines requests on stdin, typed JSON Lines results
column-rs --pipe events/

# Compile a query ahead of time (no runtime parser in the output)
column-rs codegen "SELECT id FROM orders WHERE amount > 1000" --out query.rs
```

See [`t-rust-db/examples/column-rs`](https://github.com/t-rust-db/examples)
for a runnable fixture and a spread of example queries (filter, GROUP BY,
ORDER BY + LIMIT, JOIN, window function).

## Pipe mode

`column-rs --pipe <tables...>` keeps one process open for many queries. Each
line on stdin is one request, `{"id": ..., "sql": "..."}`. The answer to it
is a header line, one line per row, and a `done` line, all carrying the
request's `id`:

```json
{"id":1,"columns":[{"name":"event_id","type":"VARCHAR"},{"name":"n","type":"BIGINT"}]}
{"id":1,"row":["ev_0000",4749]}
{"id":1,"done":true,"rows":1,"elapsed_ms":3}
```

A failed request gets `{"id":1,"error":{"kind":"...","message":"..."}}`
instead, and the process carries on. Each value is encoded by its
column's type:
- Integers above 2^53 are strings.
- A DECIMAL is an exact string at its scale. It is exact up to about 15
  digits, because db-core's engine carries decimals as floats.
- A BLOB is base64.
- Timestamps are ISO 8601 UTC.
- A MAP is an object.

Each request reads a fresh snapshot of a table directory, so it sees every
record acknowledged before it.

```python
import json, subprocess

proc = subprocess.Popen(["column-rs", "--pipe", "events/"],
                        stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)

def query(id, sql):
    proc.stdin.write(json.dumps({"id": id, "sql": sql}) + "\n")
    proc.stdin.flush()
    header, rows = json.loads(proc.stdout.readline()), []
    if "error" in header:
        raise RuntimeError(header["error"]["message"])
    while "done" not in (line := json.loads(proc.stdout.readline())):
        rows.append(line["row"])
    return [c["name"] for c in header["columns"]], rows

print(query(1, "SELECT event_id, COUNT(*) AS n FROM events GROUP BY event_id ORDER BY n DESC LIMIT 3"))
proc.stdin.close()
proc.wait()
```

## Benchmarks

DuckDB parity benchmarks live in
[`t-rust-db/benchmark/parity/column-rs`](https://github.com/t-rust-db/benchmark),
kept separate from this repo so parity testing isn't coupled to
column-rs's own release cycle.

## Building

```bash
cargo build --release
cargo test
```

column-rs is a member of the [tools](../README.md) workspace, which builds
against one tagged db-core release from GitHub and builds db-cli from
`../db-cli`.
