# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.4.0] - 2026-09-14

### Added
- Per-field history for filter (`/`) and highlight (`?`) expressions, persisted across sessions under `$XDG_CACHE_HOME/loglume/` (or `$HOME/.cache/loglume/`) (#43)
- `Up`/`Down` while editing cycle backward/forward through that field's history, restoring the in-progress draft when paging back past the newest entry

## [0.3.0] - 2026-09-14

### Added
- Emacs/readline-style line editing in filter (`/`) and highlight (`?`) text boxes (#42)
- Support for cursor movement: Ctrl-A/E (line start/end), Ctrl-B/F + arrows (char movement), Alt-B/F (word movement)
- Support for line editing: Ctrl-K (kill to end), Ctrl-U (kill to start), Ctrl-W/Alt-Backspace (delete word), Ctrl-D (delete char)
- Visible cursor position in filter and highlight bars (reverse-styled character)

### Changed
- Filter and highlight edit blocks now use a shared line editing helper for consistency

## [0.2.0] - 2026-09-14

### Changed
- TUI panes now default to newest-first (reverse) display order, showing the latest log lines at the top without requiring scroll (#40)
- Title tag now marks the non-default (oldest-first) display order with `[oldest-first]` instead of marking the default

### Fixed
- Tests updated for new default reverse display order

## [0.1.4] - Previous release

See git history for earlier changes.
