# Changelog

All notable changes to column-ingest. Format follows [Keep a Changelog](https://keepachangelog.com/), versioning follows [SemVer](https://semver.org/).

## [0.3.16] - 2026-11-14

### Changed

- db-core v0.241.0: in the column engine, `round(x, n)` with a negative `n` rounds to tens, a decimal literal keeps its written scale, a BLOB used as a number is an error, and `CAST`, `::` and `decode()` follow DuckDB (db-core#1027, db-core#948); a cold query on an `s3://` table makes far fewer requests (db-core#1059).

## [0.3.15] - 2026-11-13

### Changed

- db-core v0.240.0: range reads for object stores (db-core#1052).

## [0.3.14] - 2026-11-11

### Changed

- db-core v0.239.0: S3 signs with IAM role credentials (db-core#1042).

## [0.3.13] - 2026-11-07

### Changed

- db-core pin v0.236.0.

## [0.3.12] - 2026-11-07

### Changed

- db-core pin v0.235.0.

## [0.3.11] - 2026-11-07

### Changed

- db-core pin v0.234.2.

## [0.3.10] - 2026-11-07

### Changed

- db-core pin v0.234.0.

## [0.3.9] - 2026-11-06

### Changed

- db-core pin v0.229.0.

## [0.3.8] - 2026-11-04

### Changed

- db-core pin v0.228.2.

## [0.3.7] - 2026-11-04

### Changed

- db-core pin v0.225.1.

## [0.3.6] - 2026-11-02

### Changed

- db-core pin v0.225.0.

## [0.3.5] - 2026-11-01

### Changed

- db-core pin v0.224.1.

## [0.3.4] - 2026-11-01

### Changed

- db-core pin v0.223.0.

## [0.3.3] - 2026-11-01

### Changed

- db-core pin v0.221.0.

## [0.3.2] - 2026-10-31

### Fixed

- `--follow` re-ingested the log from the start on any resume-file error; it now restarts only on a missing resume file or a rotated or truncated log (db-core#884).
- Following a log was quadratic on long lines and buffered whole files (db-core#883).
- A table whose writer panicked kept accepting calls; it refuses them until reopened (db-core#884).

### Changed

- db-core pin v0.220.2.

## [0.3.1] - 2026-10-31

### Fixed

- CLF, syslog and ISO 8601 timestamps were stored a day early for most dates (every date in 2025, March to December 2024): a CLF line stamped `15/Jun/2025:12:00:00 +0000` was written as 2025-06-14 12:00. Impossible dates such as 2023-02-29 are rejected, and a fractional JSON Lines epoch keeps its fraction (db-core#882). Rows already written by 0.3.0 keep the wrong value; re-ingest them to correct it.
- A day merge refuses a Parquet file whose footer claims more than 64 Mi rows in one row group, or a string dictionary larger than its bytes can hold, instead of allocating for it (db-core#881).

### Changed

- db-core pin v0.218.0 (was v0.157.0).

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
