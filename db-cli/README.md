# db-cli

Generic REPL/readline infrastructure shared across `t-rust-db` engine CLIs
(column-rs, sqlite-rs, loglume, …). Contains no engine-specific types — an
engine plugs in its own query execution and result formatting.

## What's here

- `history` — command history storage, navigation, and file persistence.
- `editor` — a hand-rolled line editor (cursor movement, insert/delete,
  history recall, SQL syntax highlighting) driven by raw terminal mode.
  Raw-mode terminal I/O has no safe stdlib API, so `editor::term` is the
  crate's one `#[allow(unsafe_code)]` escape hatch; everything else is
  `#![deny(unsafe_code)]`.
- `output` — table/JSON/CSV rendering over plain `Vec<String>` headers and
  rows. No engine value types involved — stringify cells before calling in.
- `repl` — the generic REPL loop: statement buffering up to a trailing `;`,
  built-in dot-commands (`.help`, `.quit`/`.exit`, `.mode`), and dispatch to
  an engine-supplied handler for everything else.

## The `ReplHandler` contract

```rust
pub trait ReplHandler {
    type Output;

    fn execute(&mut self, input: &str) -> Result<Self::Output, String>;
    fn format(&self, output: &Self::Output, mode: OutputMode) -> String;

    fn command(&mut self, name: &str, arg: &str) -> Option<Vec<String>> { None }
    fn help_extra(&self) -> Vec<String> { Vec::new() }
    fn banner(&self) -> Option<String> { None }
}
```

- `Output` is whatever shape an engine's result naturally takes. A simple
  engine can use `(Vec<String>, Vec<Vec<String>>)` and hand it straight to
  `output::render`. An engine with richer shapes — e.g. column-rs's
  `EXPLAIN` plan tree — can define its own enum and format each variant
  however it likes; `db-cli` never needs to know that type exists.
- `command` handles engine-specific dot-commands (column-rs's `.tables`,
  `.schema`, `.open <path>`). Return `None` for anything you don't
  recognize — `db-cli` reports it as `unknown command: .<name>`.
- The built-ins `.help`, `.quit`/`.exit`, and `.mode <table|json|csv>` are
  handled by `db-cli` itself and never reach `command`.

Drive it with:

```rust
use db_cli::{run_repl, ReplOptions};

run_repl(my_handler, ReplOptions {
    prompt: "mydb> ",
    continuation_prompt: "   -> ",
    history_file: db_cli::history_path("mydb").as_deref(),
})?;
```

`repl::Repl<H>` is the same dispatch logic decoupled from terminal I/O —
it's what the crate's own tests drive with plain strings, and what you'd
reach for if you want to unit test an engine's `ReplHandler` without a real
TTY.

## Not handled here

- SQL parsing/execution — that's the engine's `execute`.
- Value formatting for engine-specific types (e.g. column-rs's `Value`) —
  stringify before returning from `execute`, or format inside `format`.
