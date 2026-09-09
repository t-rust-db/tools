# db-studio: plan

## What it is

A TUI database studio for the t-rust-db family — not a GUI editor. "IDE-shaped"
here means Zed's *editor ergonomics* (multi-buffer query editing, fuzzy file
switching, schema-aware hover/completion) delivered inside a terminal, not a
windowed application: no rendering layer, no cross-platform GUI toolkit, no
display server dependency. That keeps db-studio consistent with the rest of
the family — sqlite-rs, trigrep, and friends are all terminal-native,
ssh-friendly tools, and a GUI would be a different kind of project with a much
larger surface (windowing, mouse/rendering, its own plugin ecosystem) for a
tool whose actual job is "point at a file, run SQL, look at a grid."

A literal GUI is not ruled out forever, but it's a separate, later decision
(see Non-goals) — not something this plan designs for or hedges toward. One
binary that opens a file, figures out which of `db-core`'s execution modes it
belongs to, and gives you a query pane, a file browser, and a results grid
without caring which engine is underneath.

Usage shapes:
- `db-studio some.parquet some.sqlite some.log` — open one or more files on
  the command line, engine picked per file by extension/content sniff.
- `db-studio` with no args — opens to the file pane, files added/opened from
  inside the TUI.

## File type → execution mode

| Extension | Mode | `db-core` layer |
|---|---|---|
| `*.parquet` | column | `storage::column` (Parquet reader) + `vm::batch` |
| `*.sqlite` / `*.db` | row | `storage::row` (b-tree/pager/VFS) + `vm::row` |
| `*.log` | stream | `storage::stream` (syslog/log parsing, in progress in db-core) + a streaming VM |

One SQL dialect, one parser, one `Value` model per mode (row's `value::Value`
vs. batch's `vm::batch::Value` — see db-core ADR 0010). db-studio never
reimplements query execution: it opens a file, decides its mode, and drives
the matching `db-core` VM. Switching between open files switches which
engine's VM is live; the query pane's grammar doesn't change, but which
constructs are accepted does (e.g. no window functions against a `.log`
stream if the stream VM doesn't support them yet).

## Layout

```
┌─────────────────────────────────────────────────────────────┐
│ query [+ highlighting options]                               │  top
├───────────────┬───────────────────────────────────────────────┤
│ open files     │ query output (grid)                          │  small left | big right
│                │                                               │
├───────────────┴───────────────────────────────────────────────┤
│ error message                                                 │  bottom
├─────────────────────────────────────────────────────────────┤
│ status bar                                                    │
└─────────────────────────────────────────────────────────────┘
```

- **Top — query pane.** Where SQL is typed/edited. Syntax highlighting
  reflects the active file's mode (row/column/stream constructs), plus
  user-facing highlighting options (theme, result-affecting tokens).
- **Left (small) — open files.** The list of files opened via CLI args or
  in-TUI; selecting one makes it the active query target and switches the
  active engine.
- **Right (big) — query output.** Results in grid form: paginated/scrollable
  rows and columns, mode-agnostic rendering regardless of which VM produced
  them.
- **Bottom — error message.** Parse errors, execution errors, engine-specific
  errors (e.g. "not supported in stream mode") surfaced here, not inline in
  the query pane.
- **Status bar.** Active file, active engine/mode, connection/transaction
  state, cursor position.

**No dot-commands in the query pane.** `sqlite-rs` and `column-rs`'s REPLs
have `.dot`-syntax meta-commands (`.tables`, `.schema`, ...); `loglume` has
no equivalent — it's a `clap` CLI (`--tail`/`--follow`), not a REPL, so
there's no existing third dot-command set to match. Typing dot-syntax into
a pane whose grammar is otherwise "SQL, mode determines what's accepted"
(see Shared core) would special-case two of three modes and leave the third
with nothing. Instead: a command palette (keybinding-triggered, e.g. Zed's
`Cmd-K`) surfaces the row/batch dot-command equivalents (`.tables`,
`.schema`, ...) and stream's tail/follow-style actions as palette entries,
uniformly across all three modes. The query pane stays pure SQL always.

## Shared core, not reimplemented

- **Parser**: one `db-core` SQL grammar for all three modes; db-studio never
  owns its own parser.
- **VM**: `vm::row` for `.sqlite`, `vm::batch` for `.parquet`, and whatever
  stream-execution surface lands in `db-core` for `.log` (`storage::stream`
  is the storage side of this, currently in progress there — see db-core's
  `src/storage/stream/{batch,syslog}.rs`). db-studio is a client of all
  three, same relationship sqlite-rs/column-rs/trigrep/loglume have.
- **Live operation on streams**: `.log` files are the one mode where
  "query" can mean "tail and re-evaluate," not just "run once over a static
  file" — the streaming VM's shape needs to account for this from the start
  rather than bolting it on after row/column parity.

## Introspection panes

Three more panes, all rendering data `db-core` already computes — db-studio
adds no new query-planning or opcode logic of its own here, only a view:

- **Query plan.** Row mode already has `EXPLAIN QUERY PLAN` (`SEARCH`/`SCAN`
  strategy). Batch mode has full parity, not a gap: `codegen::batch::explain`
  builds the same kind of plan tree (`PlanNode`s — filter/join/semi-join/
  window/distinct/group-by-aggregate) over its own query shape. Stream mode's
  equivalent doesn't exist yet (`storage::stream` is still storage-only), so
  this pane is row/batch-ready today and stream-pending.
- **Opcode overview.** Same story: `vm::row::explain::explain` renders a row
  `Program` as addr/opcode/p1-p5/p4/comment rows, and `codegen::batch::
  explain_opcodes` does the equivalent for batch, per-section (build/probe/
  body for joins, matching the compiled program exactly per its own tests).
  Both exist; the pane just needs to render whichever the active file's mode
  produces.
- **File statistics.** Mode-specific, read from the storage layer, not the
  VM: row mode from the pager/b-tree (page count/size, table/index roots —
  `storage::row::integrity` already walks these for its checks), column mode
  from Parquet footer/row-group metadata (`storage::column::parquet::footer`/
  `parquet_file`), stream mode from whatever `storage::stream` exposes once
  it has a notion of "how much of this file have I parsed" (line count, byte
  offset/tail position).

## Phased plan: simple to advanced

Each phase should be independently useful — not a throwaway prototype for
the next one. Ordered by what's already mature in `db-core`, so early phases
have the least new ground to cover.

### M1 — one file, one pane loop: `.sqlite` only

The smallest useful thing: `db-studio some.sqlite`, query pane + grid output
+ error pane, no file switcher (one file per invocation), no highlighting
options yet. Row mode first because it's the family's most mature engine
(`vm::row` has full opcode coverage, `EXPLAIN`/`EXPLAIN QUERY PLAN` already
exist) — nothing to build in `db-core` to get here, purely a rendering and
input-loop exercise in db-studio itself. Exit criteria: type a query, see
results or an error, quit cleanly.

### M1.5 — usability & layout pass

Inserted after M1 shipped (#7/#8) because the bare `Paragraph`-based query
box and flat white/red styling turned out too rough to keep building on --
not new engine surface, a quality pass over what M1 already has, informed
by looking at how Harlequin (a terminal SQL IDE, closest comparable) and
ratatui's own showcase apps (Longbridge Terminal, gitui) handle the same
problems:

- **Query pane**: replace the hand-rolled `QueryPane` with [`tui-textarea`]
  (multi-line editing, cursor/selection, undo/redo already built) instead
  of maintaining our own cursor math.
- **Syntax highlighting**: classify tokens via `db-core`'s own row
  tokenizer (`parser::row::tokenizer`), not a second SQL grammar
  (`syntect`/`tree-sitter-sql`) that can drift from what the engine
  actually accepts -- db-cli's existing `highlight` module is a
  keyword-list ANSI-escape highlighter for a line-editor, the right idea
  but the wrong shape (raw escapes, not `ratatui::text::Span`s) and not
  tokenizer-driven, so it's a spec to crib from, not code to lift.
- **Schema tree inspector**: a [`tui-tree-widget`] pane (table -> columns)
  fed by `RowEngine::run_query` against `sqlite_master`/`PRAGMA
  table_info` -- no `db-core` changes needed, M1's `Engine::run_query` is
  already sufficient. Doubles as M2's open-files pane once multiple files
  exist, so it doesn't need throwing away later.
- **Completion**: a schema-aware popup using [`nucleo`] (fuzzy ranking,
  what Helix uses) over the same catalog query as the tree inspector.
- **Visual polish**: `BorderType::Rounded`, a [`catppuccin`] palette
  instead of hand-picked `Color::White`/`Color::Red`, the active pane's
  border distinctly highlighted (Longbridge's focus convention -- nothing
  today distinguishes which pane has keyboard focus), and a `ratatui::
  widgets::Scrollbar` on the grid pane instead of relying on the
  highlighted-row cue alone.

Tracked as epic t-rust-db/db-studio#13 (sub-issues #9-#12).

### M2 — file switcher, multiple `.sqlite` files

Add the left file pane and the status bar; CLI can take multiple files,
switching between them re-targets the query pane at a different open
connection. Still one mode — this phase is about the pane-switching
mechanic, not new engine surface.

### M3 — second mode: `.parquet`

Add column mode alongside row mode. This is where "switches between engines"
first becomes real: opening a `.parquet` file drives `vm::batch` instead of
`vm::row`, same query pane, different accepted constructs. Exit criteria:
one db-studio session with both a `.sqlite` and a `.parquet` file open,
switching between them changes which engine runs the query, single-file
queries only (no cross-mode joins yet — see Open questions).

### M4 — introspection panes, both modes

Query plan and opcode overview panes (see above) for row and batch — both
already exist in `db-core`, so this phase is purely wiring/rendering, not new
query-planning logic. File statistics pane for row (pager/b-tree) and column
(Parquet footer) follow the same pattern. Doing this after M3 rather than
alongside M1 keeps the earliest phases minimal; doing it before stream mode
means the pane-rendering shape is proven on two mature modes before the
third, less mature one needs it too.

### M5 — third mode: `.log` / stream

The hard phase, gated on `storage::stream` maturing past storage-only in
`db-core` (a streaming VM and its own `EXPLAIN` surface don't exist yet — see
Introspection panes). Also the first phase where "live" query semantics
matter (tailing vs. run-once — see Open questions), so it should land with
its own answer to that question rather than inheriting M1-M4's run-to-completion
assumption by default.

### M6 — editor ergonomics, cross-mode

Everything under "IDE-shaped" that isn't required for M1-M5 to work at all:
multi-buffer query editing, fuzzy file switching, schema-aware hover, and
(if still wanted) cross-mode joins in a single query. Deliberately last —
these are quality-of-life and scope-expansion features, not blockers for a
usable tool.

The command palette (see Layout — no dot-commands in the query pane) is
placed here provisionally; `.tables`/`.schema` discoverability might be
rough enough without it in M1 (row mode) that it's worth pulling earlier —
flagged in Open questions rather than decided here.

## Decisions

- **TUI framework: `ratatui` + `crossterm`.** `ratatui`'s `Table`/`Paragraph`/
  layout widgets map directly onto the plan's panes (grid, query/file/error,
  status bar), and it's the actively-maintained standard for Rust TUIs —
  hand-rolling pane/grid rendering from raw ANSI escapes (the way `db-cli`
  hand-rolled its own readline loop) would be a much bigger lift for a TUI's
  intrinsically bigger rendering surface. `db-core`'s zero-third-party-deps
  policy is scoped to `db-core` itself; `db-cli` already takes deliberate,
  tracked dependencies (`libc`, `dirs`), and db-studio may too — external
  packages in `t-rust-db/db-studio` are accepted, not a concern here.

- **An `Engine` seam, in `db-core`, before M3.** Row, batch, and (later)
  stream each need `open`/`run_query`/`explain_plan`/`explain_opcodes`/
  `stats` — db-studio should call one interface, not branch on mode itself.
  M1 only exercises the row implementation, but the trait needs to exist
  from M1 so M3's second implementation is additive, not a retrofit of
  file-switching code that assumed one concrete type. This is a `db-core`
  design decision (differing `Value` types and plan/opcode shapes between
  row and batch need a common client-facing representation) — tracked as
  t-rust-db/db-core#295, blocking M1's engine-hookup story (not the pane/
  rendering stories, which don't depend on it).

## Open questions

- Does the command palette need to ship in M1 (row mode's `.tables`/
  `.schema` discoverability) rather than waiting for M6, given the query
  pane deliberately has no dot-command fallback?
- What does "live" mean for a `.log` query — polling, `inotify`/`kqueue`
  file-watch, or an explicit re-run keystroke? Affects whether the streaming
  VM needs an incremental/resumable execution model or can stay
  run-to-completion per keystroke.
- Multi-file joins across modes (e.g. join a `.sqlite` table against a
  `.parquet` file in one query) — in scope for v1, or single-file-at-a-time
  to start?
- Where does the "which mode does this file imply" sniff live — db-studio,
  or a shared `db-core` helper other clients could reuse too?

## Non-goals (for now)

- Not a general-purpose file manager — file support is scoped to the three
  types `db-core` understands.
- Not a GUI. "IDE-shaped" is scoped to editor ergonomics inside the terminal
  (see What it is) — a windowed application is a separate, later decision
  this plan does not design toward.
