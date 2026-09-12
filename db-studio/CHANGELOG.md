# Changelog

All notable changes to db-studio. Format follows [Keep a Changelog](https://keepachangelog.com/), versioning follows [SemVer](https://semver.org/). Pre-1.0: minor bumps may break the public API.

## [0.9.0] - 2026-09-12

### Added

- **Rainfrog-style completion popup** (#48): static SQL keywords/scalar function names (sourced from db-core's `keyword_names()`/`SCALAR_FUNCTION_NAMES` rather than hand-maintained) alongside table/column names from every open file, not just the active one.

### Fixed

- A real `nucleo-matcher` 0.3.1 panic ("should have been caught by prefilter") on certain all-uppercase-keyword/lowercase-needle shapes -- `rank()` now lowercases both sides itself before matching.
- Bumped db-core to v0.88.2.

## [0.8.0] - 2026-09-12

### Added

- **Inline row-detail expansion in the results grid** (#45): `Tab` reaches a new third focus stop on the grid (`Query` -> `Tree` -> `Grid` -> `Query`); `Enter` there opens the selected row's full-value detail section (`key: value` per line, `raw` sorted first for `.log` files), inserted directly below it in the same table rather than a separate pane. Navigating rows auto-collapses it.

## [0.7.1] - 2026-09-12

### Fixed

- **`NULL` cells rendered as blank, indistinguishable from an empty string** (#44): `Cell::Null` now renders as `<NULL>` in the results grid instead of an empty string -- `Cell::Display`'s "NULL is empty" convention is correct for db-core's shell-style clients (sqlite-rs/column-rs/loglume), but made a sparse result (e.g. `SELECT *` over a `.log` file's Tier-3 columns, absent on most rows) look indistinguishable from one full of blanks.

## [0.7.0] - 2026-09-12

### Added

- **Usability batch from hands-on testing** (#42): `F1` on a schema-tree table/column runs `SELECT * FROM <table> LIMIT 1000`/`SELECT DISTINCT <column> FROM <table> LIMIT 100` and shows results; a real horizontal `Scrollbar` widget alongside the existing column-hiding scroll; `Ctrl+Y` copies the active output view to the system clipboard via OSC 52 (plain text, no ANSI/border chars); `Up`/`Down` at the query buffer's edges cycles submitted-query history, persisted at `$XDG_CACHE_HOME/db-studio/history`; `Ctrl+O` opens a path-input prompt to add a new file mid-session; the query editor shows line numbers; `q` quits when the schema tree has focus.

### Changed

- Query plan pane word-wraps and indents one space per depth instead of two; error pane word-wraps instead of clipping to one line; query and error panes render borderless; results pane title shows a row count; query pane title no longer mentions F5.

## [0.6.0] - 2026-09-11

### Added

- **M5: third mode: `.log` / stream** (epic #35). `.log` files open through db-core's `StreamEngine`, the third `Engine` mode alongside row/batch (`main.rs`'s `open_by_extension`, #36). db-core's stream engine (its own epic, db-core#302) was already complete by the time this landed -- no `db-core` changes were needed here, unlike M3's `#295` blocker. Verified end to end rather than assumed: the schema tree, introspection panes (`F2`/`F3`/`F4`), and error pane were all already mode-agnostic and needed zero rendering-code changes to handle stream data correctly (#37-#39); a query switching between `.sqlite`, `.parquet`, and `.log` files in one session runs against the right engine and reports the right mode throughout (#40).
- `JOIN`/window-function queries against a `.log` file's single table surface specific `ErrorKind::Unsupported` messages (e.g. "JOIN with `l2`: the stream engine has one table") through the existing generic error pane.

## [0.5.0] - 2026-09-10

### Added

- **M4: introspection panes, both modes** (epic #29). `F1`-`F4` cycle the results area between Results (default), Query plan, Opcodes, and File statistics -- all rendering data `Engine` already computed (`explain_plan`/`explain_opcodes`/`stats`), no new query-planning logic in db-studio (#30-#33). Plan/Opcodes read the query pane's *current* text, not the last-submitted query, so a plan can be previewed before running with `F5`. A compile/parse failure while viewing Plan/Opcodes routes to the existing error pane and leaves the current view unchanged, rather than replacing it with something broken.
- Unlike M3, this milestone had no `db-core` blocker -- verified before starting that both `RowEngine` and `BatchEngine` already implement the needed `Engine` methods.

### Fixed

- `F5` no longer clears the query pane's text after running -- it broke `F2`/`F3` immediately afterward (they read the query pane's current text, so right after running they had nothing to preview and errored with "parse: empty statement"). Every other SQL tool keeps the query visible after running it too.
- `F5` now switches back to the Results view before running -- previously, running a query while viewing Plan/Opcodes/Stats updated the results grid invisibly behind whichever of those stayed on screen, making `F5` look like it had stopped working.

## [0.4.0] - 2026-09-10

### Added

- **M3: second mode, `.parquet`** (epic #23). `db-studio a.sqlite b.parquet` dispatches each file to the right `Engine` by extension -- `.sqlite`/`.db` to `RowEngine`, `.parquet` to the new `db_core::engine::column::BatchEngine` (t-rust-db/db-core#325-#328, a port of column-rs's `QueryEngine` single-file case) (#24). The schema tree/completion needed no code changes to render batch-mode tables correctly -- `TableInfo`/`ColumnInfo` were already mode-agnostic by construction (#25). The status bar now reads `Engine::mode()` instead of a hardcoded `"row"`, showing "row" or "batch" per the active file (#26).
- Verified interactively (real `tmux` sessions, not unit tests alone): a `GROUP BY`/`COUNT(*)` aggregate query runs correctly against a real Parquet file through `vm::batch`, switching between a `.sqlite` and a `.parquet` file mid-session correctly retargets both query execution and the status bar's reported mode.

## [0.3.0] - 2026-09-10

### Added

- **M2: file switcher, multiple `.sqlite` files** (epic #21). `db-studio a.sqlite b.sqlite ...` opens one `Engine` per file (#16); the "Data Catalog" tree gained a file root level (`file -> table -> columns`, replacing the bare `table -> columns` M1.5 shipped) so multiple files render as sibling roots (#17). Selecting a file root (`Enter`/`Space`) makes it the active query/completion target -- confirmed via real `tmux capture-pane` sessions, not just unit tests, that switching genuinely retargets execution and back again (#18). A new status bar shows the active file, execution mode, and the query pane's live cursor position (#19).
- A pty-based raw-byte-diff testing artifact was found and worked around during this milestone: ratatui only retransmits changed terminal cells, so naively concatenating pty output across frames can make a working feature (the status bar's live cursor position) look broken. `tmux capture-pane`, which tracks real screen state, is the more reliable check going forward.

## [0.2.0] - 2026-09-10

### Added

- **M1.5: usability & layout pass** (epic #13). Query pane rewritten around `tui-textarea-2` (multi-line editing, cursor/selection/undo/redo) with SQL syntax highlighting classified through `db-core`'s real row tokenizer, not a hand-maintained keyword list (#9). `F5` now submits -- not `Ctrl+Enter`, which a plain terminal without the Kitty keyboard protocol can't distinguish from plain `Enter` -- with the binding surfaced right in the pane's title so it's discoverable without reading the changelog.
- A "Data Catalog" schema tree pane (`tui-tree-widget`), table -> columns, fed by a new `db-core#310` addition (`Engine::tables()`) rather than `sqlite_master`/`PRAGMA table_info`, neither of which works through `vm::row`'s compiled `SELECT` path (#10). Real focus-cycling (`Tab`) between the query pane and the tree, since the tree is genuinely a second focusable pane.
- A schema-aware completion popup (`nucleo-matcher`), ranking table/column names fuzzily against the same catalog fetch the schema tree uses; suppressed inside string/blob literals, including one still being typed (#11).
- Visual polish: a catppuccin mocha palette and rounded borders on every pane, the focused pane's border rendered distinctly, and a scrollbar on the grid pane alongside its existing row highlight (#12).

## [0.1.0] - 2026-09-10

### Added

- **M1: one file, one pane loop** (#7). `db-studio path/to.sqlite` opens a query pane, runs SQL against `db-core`'s row `Engine` (`t-rust-db/db-core#295`), and renders results in a scrollable grid or an error message -- Esc/Ctrl-C quit cleanly, restoring the terminal on both a normal quit and a panic. Built on `ratatui` + `crossterm` (#1), with a hand-rolled single-line query input (#3, slated for replacement by `tui-textarea` in #9), a `Table`-based grid pane (#4), and an error pane sharing the query-submission path with the grid rather than a separate error code path (#5).
