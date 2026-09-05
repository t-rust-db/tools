# db-cli

Generic REPL/readline infrastructure shared across `t-rust-db` engine CLIs
(column-rs, sqlite-rs, loglume, …). Contains no engine-specific types — an
engine plugs in its own query execution and result formatting.

## What's here

- `history` — command history storage, navigation, and file persistence.
- `editor` — a hand-rolled line editor (cursor movement, insert/delete,
  history recall, emacs/readline keybindings, tab completion, SQL syntax
  highlighting) driven by raw terminal mode. Raw-mode terminal I/O has no
  safe stdlib API, so `editor::term` is the crate's one
  `#[allow(unsafe_code)]` escape hatch; everything else is
  `#![deny(unsafe_code)]`.
- `output` — table/list/column/line/CSV/JSON rendering over plain
  `Vec<String>` headers and rows. No engine value types involved —
  stringify cells before calling in.
- `repl` — the generic REPL loop: statement buffering up to a trailing `;`,
  built-in dot-commands (`.help`, `.quit`/`.exit`, `.mode`, `.headers`,
  `.color`), and dispatch to an engine-supplied handler for everything
  else.

## Output modes

`OutputMode` covers `sqlite3`'s own set plus two db-cli additions:

| Mode | `.headers` respected? | Notes |
|---|---|---|
| `List` (`sqlite3`'s default) | yes | fields joined with `\|` |
| `Column` | yes | fixed-width columns, 2-space gap, `-`-underlined header |
| `Line` | no (always labels) | one `name = value` line per column, blank line between rows |
| `Csv` | yes | RFC 4180-ish quoting |
| `Table` (db-cli addition) | always on | ASCII box-drawn |
| `Json` (db-cli addition) | always on | self-describing, `.headers` is meaningless |

`Repl<H>` defaults to `Table`/`headers: false` (matching stock `sqlite3`'s
`.headers` default); `.mode <mode>` and `.headers on|off` switch them at
runtime. `ReplHandler::format` receives the current `headers` setting
alongside `mode` so an engine's own `output::render` call can pass it
straight through.

## Tab completion and syntax highlighting hooks

`Readline` exposes two pluggable traits — the built-in defaults (no
completions, the keyword-based highlighter) are unchanged unless an engine
opts in:

```rust
pub trait Completer {
    /// `(prefix_start, candidates)` for the word ending at `cursor` in
    /// `line`. Empty `candidates` means no completions.
    fn complete(&self, line: &str, cursor: usize) -> (usize, Vec<String>);
}

pub trait Highlighter {
    /// Must preserve `line`'s exact visible char count -- ANSI escapes are
    /// zero-width and fine to add, but inserting/removing visible
    /// characters breaks cursor-column math.
    fn highlight(&self, line: &str) -> String;
}
```

```rust
let mut editor = db_cli::Readline::new();
editor.set_completer(MyEngineCompleter::new(&schemas));
editor.set_highlighter(MyEngineHighlighter);
```

Tab with exactly one candidate completes the word outright; with multiple
candidates it inserts their longest common prefix (classic readline
"partial completion") if that extends past what's already typed, otherwise
nothing happens.

## The `ReplHandler` contract

```rust
pub trait ReplHandler {
    type Output;

    fn execute(&mut self, input: &str) -> Result<Self::Output, String>;
    fn format(&self, output: &Self::Output, mode: OutputMode, headers: bool) -> String;

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
- `execute`'s `input` is the buffered statement text with the trailing `;`
  stripped and internal newlines collapsed to single spaces — the same
  string content the user typed, not byte-identical to it. This is enough
  for an engine to pattern-match its own pragma-style introspection
  commands (e.g. `PRAGMA table_info(t)`) directly inside `execute`, the
  same way sqlite-rs's `pragma_query.rs` does against its own buffered
  statement text — no db-cli change needed for that.
- `command` handles engine-specific dot-commands (column-rs's `.tables`,
  `.schema`, `.open <path>`). `arg` is the trailing text after the command
  name (empty string, not absent, when nothing follows), so pattern-style
  commands like `.tables PATTERN` just check whether `arg` is empty.
  Return `None` for anything you don't recognize — `db-cli` reports it as
  `unknown command: .<name>`.
- The built-ins `.help`, `.quit`/`.exit`, `.mode <table|list|column|line|csv|json>`,
  `.headers on|off`, and `.color on|off` are handled by `db-cli` itself and
  never reach `command`.

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
TTY. `run_repl`'s loop syncs `.color`'s setting to the `Readline` it owns
after every line (`Repl` itself never touches a real `Readline`, so it stays
testable with plain strings).

## Not handled here

- SQL parsing/execution — that's the engine's `execute`.
- Value formatting for engine-specific types (e.g. column-rs's `Value`) —
  stringify before returning from `execute`, or format inside `format`.
- `.tables`/`.schema`/`.dump`/`.databases`/`.indices`/`.version` and any
  other engine-specific dot-command — those go through `command`, not a
  db-cli built-in.
