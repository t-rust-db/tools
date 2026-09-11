# Changelog

All notable changes to db-studio. Format follows [Keep a Changelog](https://keepachangelog.com/), versioning follows [SemVer](https://semver.org/). Pre-1.0: minor bumps may break the public API.

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
