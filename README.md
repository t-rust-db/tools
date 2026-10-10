# tools

The command-line and TUI tools built on [db-core](https://github.com/t-rust-db/db-core),
in one Cargo workspace: one `Cargo.lock`, one db-core pin, one CI.

| Tool | What it is |
|---|---|
| [db-cli](db-cli/) | Shared REPL/readline library for the CLIs |
| [column-rs](column-rs/) | SQL over Parquet files (CLI) |
| [sqlite-rs](sqlite-rs/) | Binary-compatible SQLite replica (library + CLI) |
| [loglume](loglume/) | Log viewer with SQL filtering (TUI) |
| [trigrep](trigrep/) | Trigram-indexed grep (`tg`) |
| [db-studio](db-studio/) | TUI database studio over .sqlite/.parquet/.log files |

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

Every tool builds against one tagged db-core release from GitHub
(`[workspace.dependencies]` in `Cargo.toml`), never a local checkout. To
move to a new db-core release, bump the tag (and version) there and run
`cargo update -p db-core`.
