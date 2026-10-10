# tools

The command-line and TUI tools built on [db-core](https://github.com/t-rust-db/db-core),
in one Cargo workspace: one `Cargo.lock`, one db-core pin, one CI.

| Tool | What it is |
|---|---|
| [db-cli](db-cli/) | Shared REPL/readline library for the CLIs |
| [column-rs](column-rs/) | SQL over Parquet files (CLI) |

Each tool keeps its own version, `CHANGELOG.md`, `Makefile` and README.

## Install

```bash
cargo install --git https://github.com/t-rust-db/tools <tool>
```

## Develop

```bash
make build        # cargo build --workspace
make test         # each tool's `make test`
make lint         # each tool's `make lint`
make -C <tool> help
```

The workspace pins a tagged db-core (`[workspace.dependencies]` in
`Cargo.toml`). To build against a local db-core checkout next to this repo,
create an uncommitted `.cargo/config.toml`:

```toml
[patch."https://github.com/t-rust-db/db-core"]
db-core = { path = "../db-core" }
```

then run `cargo update -p db-core` so the lockfile picks the patch up.
