# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.8.1] - 2026-09-16

### Fixed
- `--filter`/`--highlight` expressions no longer silently misread `"..."` as a quoted SQL identifier instead of a string literal (e.g. `tag = "kernel"` failed with a confusing "unknown column: kernel" error). Double-quoted spans are now normalized to single-quoted string literals before compiling; existing single-quoted expressions are unaffected (#39).

## [0.8.0] - 2026-09-16

### Added
- The stacked/side-by-side layout toggle (`v`) is now remembered across sessions in `$XDG_CACHE_HOME/loglume/tui_state`, overwriting a single latest snapshot (#56).

## [0.7.0] - 2026-09-16

### Added
- Severity-based row coloring: row text is colored by the `severity` column's band (red for Error/Fatal, peach for Warn, dim for Info/Debug/Trace), configurable via `[tui.theme].severity_error/severity_warn/severity_dim`. Composes with the existing zebra background rather than overriding it (#54).

## [0.6.0] - 2026-09-16

### Added
- Zebra banding: alternating summary rows get a subtle background (Catppuccin surface0 by default), configurable via `[tui.theme].zebra_bg`. Detail rows and highlighted/selected rows are unaffected (#49).

## [0.5.0] - 2026-09-15

### Added
- Multi-file TUI now defaults to stacked/tabbed layout: only the focused pane draws at full area, improving readability in narrow terminals. Press `v` to toggle back to side-by-side column layout (#50).
- `make run` and `make start` now launch the TUI with both sample logs and a default filter for easy testing.

### Changed
- `Makefile` `run` and `start` targets now use `--tui` with sample logs instead of non-interactive output mode.

## [0.4.1] - 2026-09-14

### Fixed
- Live view now updates on every appended line. Filesystem notifications (FSEvents on macOS) report nothing while a producer appends through a long-held file descriptor — how syslogd, docker and most application loggers write — so `--follow`, `--alert` and the TUI only refreshed when some other process happened to close the file. All three now poll the file's size/mtime (`watch_file`), and refresh only ingests the newly appended lines, so the filter is applied to just those last lines.
- `--tui` with a missing file no longer leaves the terminal in raw mode and the alternate screen: files are opened before the terminal is switched, and the terminal is restored unconditionally.

### Added
- `tests/logs/producer.py` and `make produce`: a live log producer that appends (and echoes) synthetic syslog lines at a given rate, for exercising the live view.
- `tests/spikes/notify_probe.rs` and `make spike-notify`: a spike comparing FSEvents, notify's poll watcher and plain size polling against a held-open-fd writer.

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
