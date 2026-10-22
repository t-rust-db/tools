# Changelog

All notable changes to column-ingest. Format follows [Keep a Changelog](https://keepachangelog.com/), versioning follows [SemVer](https://semver.org/).

## [0.3.0] - 2026-10-22

### Changed

- db-core pin v0.157.0 (was v0.156.0). zstd pages are decoded by db-core's own decoder (db-core#700), about 2x ruzstd, so a day merge of a zstd table reads its hour files faster.

## [0.2.0] - 2026-10-22

### Changed

- db-core pin v0.156.0 (was v0.137.0). Tables with a sort key record per-file key ranges in manifest version 4, so readers skip files on the key (db-core#589); the writer can write zstd pages (db-core#622), and a day merge keeps the table's codec choice.

## [0.1.0] - 2026-10-16

### Added

- `… | column-ingest <dir>` appends every line of stdin to the table directory `<dir>`, and `column-ingest --follow app.log <dir>` follows a log file into it, resuming where the last follow stopped (#14, db-core ADR 0029). A new table is tiered (hours in `live/`, days in `archive/`) unless `--flat`; `--format`, `--sort-key` and `--retention-days` set it up.
- Closed hours and days are written out every `--compact-every` seconds (10 by default) while it runs.
- SIGINT or SIGTERM stops intake, flushes and makes everything durable before exiting. Records already durable survive a kill.
