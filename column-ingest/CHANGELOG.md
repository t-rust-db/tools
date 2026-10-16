# Changelog

All notable changes to column-ingest. Format follows [Keep a Changelog](https://keepachangelog.com/), versioning follows [SemVer](https://semver.org/).

## [0.1.0] - 2026-10-16

### Added

- `… | column-ingest <dir>` appends every line of stdin to the table directory `<dir>`, and `column-ingest --follow app.log <dir>` follows a log file into it, resuming where the last follow stopped (#14, db-core ADR 0029). A new table is tiered (hours in `live/`, days in `archive/`) unless `--flat`; `--format`, `--sort-key` and `--retention-days` set it up.
- Closed hours and days are written out every `--compact-every` seconds (10 by default) while it runs.
- SIGINT or SIGTERM stops intake, flushes and makes everything durable before exiting. Records already durable survive a kill.
