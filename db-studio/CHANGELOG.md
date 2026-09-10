# Changelog

All notable changes to db-studio. Format follows [Keep a Changelog](https://keepachangelog.com/), versioning follows [SemVer](https://semver.org/). Pre-1.0: minor bumps may break the public API.

## [0.1.0] - 2026-09-10

### Added

- **M1: one file, one pane loop** (#7). `db-studio path/to.sqlite` opens a query pane, runs SQL against `db-core`'s row `Engine` (`t-rust-db/db-core#295`), and renders results in a scrollable grid or an error message -- Esc/Ctrl-C quit cleanly, restoring the terminal on both a normal quit and a panic. Built on `ratatui` + `crossterm` (#1), with a hand-rolled single-line query input (#3, slated for replacement by `tui-textarea` in #9), a `Table`-based grid pane (#4), and an error pane sharing the query-submission path with the grid rather than a separate error code path (#5).
