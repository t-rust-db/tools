# Changelog

All notable changes to column-rs. Format follows [Keep a Changelog](https://keepachangelog.com/), versioning follows [SemVer](https://semver.org/). Pre-1.0: minor bumps may break the public API.

## [0.20.1] - 2026-09-09

### Added

- `--help`/`-h`: usage on stdout, exit 0 (was opened as a Parquet path, exit 1). `make smoke` builds the binary and runs `--help`/`--version`; `tests/cli_smoke.rs` pins the contract.

## [Unreleased]

## [0.19.0] - 2026-09-09

### Changed

- **Joins probe per row group, in parallel** (#27). `execute_joined` reads only the build (right) side whole; the probe (left) side stays one `RowGroupSegment` per row group and goes to db-core 0.76's `run_join_segments`, which builds the hash table once, shares it across worker threads, and never materializes the joined table. Before, both sides were read whole and the join ran single-threaded: the `t-rust-db/benchmark` parity `join` at 10M rows took 4.84 s (108x DuckDB) at 4.4 GB peak RSS.
- **db-core pinned to v0.76.4** (was v0.71.1). Absorbed: `codegen::batch::compile`/`explain` return `Result` (db-core 0.75.0; new `QueryError::PlannerInvariant` mirrors `PlanError::Internal`), `Segment::load` returns `Result<Arc<Batch>>` and `Batch.columns` holds `Arc<Vec<Value>>` (db-core 0.76.0/0.76.2, zero-copy loads).

### Fixed

- **A Parquet column that fails to decode is an error, not NULLs.** `RowGroupSegment::load` used to swallow the decode error and return `num_rows` NULLs for that column, so a corrupt or unsupported page looked like a column full of NULLs. It now returns `VmError::SegmentLoad` naming the row group and column; an out-of-range row group is the same error instead of a panic.

### Added

- DuckDB oracle test for `||` concatenation and unary minus (#4): both operators were already wired through db-core's batch planner; the test pins them in `WHERE` position and asserts that a computed SELECT-list item is still rejected explicitly (db-core#198).

### Changed

- db-core pinned to v0.68.1 and db-storage to v0.5.9: `ExprKind::FunctionCall.over` became `tail: Option<FunctionTail { filter, over }>` (db-core#67), so window detection reads `tail.over`; the oracle test for `||`/unary minus now also projects them in the SELECT list (db-core#198 landed in v0.63.0).
- db-core pinned to v0.62.1 (was v0.61.1) and db-storage to v0.5.8. db-core#192 moved the AOT emitter: `db_core::emit::batch::generate` is now `db_core::codegen::batch::emit::generate` (call sites in `main.rs`, `codegen_e2e.rs`, docs).

## [0.18.0] - 2026-09-06

### Changed

- **`db-core` dependency bumped from `v0.30.0` to `v0.60.0`** (#19, supersedes #18). Three breaking changes absorbed:
  - `db_core::expr` is gone (db-core#153/#155, ADR 0002): the AST is `parser::ast::Select`. `column_rs::sql` now re-exports the whole `parser::ast` module plus `Span`, `Explain`, `WindowFunc`/`WindowSpec` (from `codegen::batch`) and `AggFunc` (from `vm::batch`) -- the emitter's generated binaries import all of these. `query.rs` reads the table and joins through `Select.from: Option<FromClause>` and matches `ExprKind::InSubquery`/`FunctionCall { over }` instead of `Expr::InSubquery`/`SelectItem::Window`. `QueryError::UnsupportedJoinKind` carries a `JoinOp`; new `QueryError::UnsupportedSelectItem` mirrors `PlanError::UnsupportedSelectItem`.
  - dyn-free batch API (db-core#156, v0.52.1): segments are passed as `&[RowGroupSegment]`/`&[InMemorySegment]`, no `Box<dyn Segment>`; `SemiJoinProgram.subquery`/`key_column` are owned.
  - `Opcode::Finalize` split into `Combine`/`Sort`/`Limit` (db-core#48, v0.60.0): `OpcodeRow.is_finalize` marks the `Combine` row; the EXPLAIN renderer's divider follows it. Results are unchanged: the 41 DuckDB oracle tests and 7 `codegen_e2e` tests pass as before.
- **`db-storage` bumped `v0.4.1` → `v0.5.5`** (`default-features = false, features = ["column"]` -- the Parquet reader only) and **`db-cli` `v0.3.0` → `v0.4.1`**, so the sibling-path patches in `.cargo/config.toml` apply again instead of warning `patch ... was not used in the crate graph`. No source changes needed.

## [0.17.0] - 2026-09-04

### Added

- **`SELECT *` now works** (#9): `QueryEngine::execute` resolves `*` against the queried table's schema (or, for a `JOIN`, both tables' schemas qualified `table.column`) via db-core's `expand_star` before dispatch, instead of falling through unhandled. `*` combined with `GROUP BY`/an aggregate/a window function returns a clear `QueryError::StarWithAggregation` instead of silently wrong output.

### Changed

- `db-core` dependency bumped to `v0.17.0` (pulls in `expand_star`/`PlanError::StarWithAggregation` from v0.15.0, plus an unrelated `emit` codegen fix from v0.17.0 -- see db-core's own changelog).

## [0.16.0] - 2026-09-04

### Changed

- **Planner and post-processing moved to db-core** (db-core ADR 0007). `src/query.rs` shrinks to Parquet glue: `RowGroupSegment`, column resolution, `read_whole_table`, the four `execute*` entry points (now thin: plan via `db_core::codegen::batch`, materialize, hand to `db_core::vm::engine`) and `QueryEngine`. Gone from this crate: `compile`/`Plan`, `AggPart`, `post_process`, `merge_rows`/`finalize_row`, `bounded_scan`/`run_program_top_n` (the engine decides those from the program), `output_column_names`, and the `EXPLAIN` tree builder (`PlanNode` is re-exported from `db_core::codegen::batch`). Execution results are unchanged -- the 35 DuckDB oracle tests and the 7 `codegen_e2e` tests pass as before.
- **`src/codegen.rs` deleted.** It was a near-verbatim duplicate of what is now `db_core::emit::batch`; the `column-rs codegen` subcommand calls `db_core::emit::batch::generate("column_rs", sql)`. Generated programs now embed the whole planned program including its terminal `Opcode::Finalize` (no `COLUMNS_TO_LOAD`/`AGG_PARTS`/`NUM_GROUP_KEYS`/`ORDER_BY`/`LIMIT` consts) and call `column_rs::query::run_program(&file, PROGRAM)`.
- **`query::run_program(file, &[Opcode])`** drops its `columns_to_load` parameter -- the columns are derived from the program's `LoadColumn` instructions.
- `db-core` dependency features: `["parser-column", "vm-batch", "codegen-batch", "emit-batch"]`.

## [0.15.2] - 2026-09-03

### Fixed

- **CI now checks out sibling repos** (`fix/ci-checkout-sibling-repos`): `.cargo/config.toml` patches db-storage/db-core/db-parquet/db-cli to sibling relative paths for local monorepo development, but CI only checked out column-rs itself, causing immediate cargo build failure at dependency resolution. CI now checks out all four siblings alongside column-rs so the relative-path layout resolves the same way locally and in CI.

## [0.15.1] - 2026-09-02

### Fixed

- **`filter_50pct` benchmark 79.5x -> 14.46x DuckDB at 10M rows** (`src/vm.rs`, `src/bin/column-rs`, #110): `Opcode::Emit`/`GroupReduce` cloned every surviving value twice (`.to_vec()` then a per-cell clone in the transpose) — now borrows the register slice directly. `Filter` sizes its kept-values buffers to the exact survivor count instead of the pre-filter length, cutting peak-RSS over-allocation. The larger fix: `column-rs -c "<SQL>"` (one-shot mode) no longer collects the whole result into a box-drawing table (which needs a full pre-pass over every cell to compute column widths) — it streams tab-separated rows directly via the new `mode::print_rows_streaming`; the REPL keeps the box table. `filter_1pct` improved too (4.9x -> 2.95x), `scan` unaffected. Doesn't fully meet the ticket's <=1.5x target — the residual cost is `Value`'s `Display` formatting itself, tracked as #121.

## [0.15.0] - 2026-09-02

Codegen (#98) extended to the rest of the query surface: a compiled query no longer bails out on `GROUP BY`/`ORDER BY`/`LIMIT` (#101) or on `JOIN`/`IN (SELECT ...)` semi-joins (#103).

### Added

- **Codegen for `GROUP BY`/aggregates/`ORDER BY`/`LIMIT`** (`src/codegen.rs`, #101): `generate()` emits `const AGG_PARTS`/`NUM_GROUP_KEYS`/`ORDER_BY`/`LIMIT` alongside `PROGRAM`, and the generated `main()` calls `query::post_process(...)` after `query::run_program(...)`.
- **Codegen for `JOIN` and `IN (SELECT ...)` semi-joins** (`src/codegen.rs`, #103): these bypass the VM program entirely at runtime, so codegen reconstructs the parsed `Query` as a literal Rust value and calls `execute_joined`/`execute_semi_join` with it — still no SQL text parsed at runtime. The generated `main()` opens every table named on the command line, matched by file stem, mirroring `QueryEngine`. More than one `JOIN` is rejected with a clear error, matching `execute_joined`'s own scope.
- **Glob expansion in generated binaries** (#101): a simple `*` in the file-name portion of a path argument, via a ~15-line helper emitted into the generated source (no new dependency).
- **`tests/codegen_e2e.rs`** (#101, #103): compiles each codegen'd file with `rustc` against the built `column_rs` rlib, runs it, and compares output to `QueryEngine::execute` — including a `GROUP BY` across `production.parquet`'s 5 row groups, exercising the cross-segment merge bug class the `post_process` refactor exists to avoid.

### Changed

- **`query::post_process` no longer needs a live `Query`** (`src/query.rs`, #101): `Plan` gains `num_group_keys`/`order_by`/`limit` (all derivable at compile time) and `post_process` takes plain `(agg_parts, num_group_keys, order_by, limit)`. It and `AggPart` are now `pub` so a codegen'd binary can call them with const data. No behavior change to the runtime path.
- **`post_process`'s sort now uses `vm::compare_for_order`** (`src/query.rs`): reconciling #101 with the bounded top-N heap from #109, so the full-sort path and the heap path order rows by exactly the same comparator rather than two separate implementations.

## [0.14.0] - 2026-09-02

### Changed

- **Bounded top-N heap for `ORDER BY ... LIMIT ...`** (`src/vm.rs`, `src/query.rs`, #109): the non-aggregated single-table path no longer fully sorts every row -- each row-group segment is reduced to its own top-N via a bounded max-heap during the existing parallel scan, then merged with a final top-N reduction, cutting the `order_by` benchmark from 121.5x DuckDB down to 3.15x at 10M rows (`GROUP BY`, joins, and window functions still sort in full, unchanged). Remaining gap tracked in #119 -- scan/decode cost, not sort cost.

### Fixed

- **`ORDER BY` NULL placement** (`src/query.rs`, #109): `NULL` values previously compared via `to_string()` instead of a consistent placement, which could sort them anywhere relative to non-null values depending on their string form. Now `NULL` always sorts last, in both `ASC` and `DESC`, matching DuckDB's default.

## [0.13.0] - 2026-09-02

### Added

- **`LIMIT` pushdown into the scan** (`src/query.rs`, #108): a bare `LIMIT` (no `WHERE`/`ORDER BY`/`GROUP BY`/aggregate) is now satisfied by a sequential bounded prefix scan that stops reading row groups as soon as enough rows are collected, instead of decoding the whole file. `scan` benchmark ratio vs DuckDB: 23.6x -> 1.29x at 10M rows; peak RSS 1860 MB -> 67 MB.
- **`QueryEngine` uses `mmap::MmapFile`** instead of an eager `std::fs::read` of the whole file into memory — otherwise peak RSS would still scale with file size regardless of how few row groups a bounded scan reads.
- `Vm::take_output()`: a small public accessor for callers (like the bounded scan) driving `Vm::execute()` batch-by-batch themselves rather than via `run`/`run_parallel`.

## [0.12.0] - 2026-09-02

### Added

- **DuckDB parity benchmark suite** (`benches/`, #100): `generate.sh` builds deterministic datasets (1K/100K/10M/100M rows plus a customer dimension table), `run.sh` times all eight query shapes on both engines with hyperfine and records peak RSS, and `report.py` renders the gap table. Both engines execute the *identical* SQL text — DuckDB gets views named after the tables column-rs derives from file stems — so neither is handed a rewritten query. Run it locally with `make bench` / `make bench-data`; it is deliberately not wired into CI, since shared hosted runners are too noisy to compare engines on (`order_by` measured 0.89x locally and 2.19x on a runner from the same data and code).

### Fixed

- **Panic projecting columns absent from the `WHERE` clause** (`src/query.rs`, #100): `compile()` loaded GROUP BY keys and aggregate sources before the `Filter` opcode but not plain SELECT-list columns, so a projected column not referenced in the predicate kept its full pre-filter batch length while the filtered registers shrank, and `Emit` indexed past the end of the short ones. `SELECT id, amount, region, customer_id FROM t WHERE amount > 9900` aborted with exit code 101.

## [0.11.0] - 2026-09-02

### Added

- **`EXPLAIN [QUERY PLAN] <select>`** (#99): shows what a query will do without running it, in the REPL and `-c` one-shot mode. Renders an ASCII tree (`SCAN`/`LOAD COLUMNS`/`FILTER`/`GROUP BY`/`AGGREGATE`/`HASH JOIN`/`SEMI JOIN`/`WINDOW`/`ORDER BY`/`LIMIT`/`EMIT`) built from file metadata and the compiled VM program for the plain/join path, or directly from query structure for joins and window functions (which bypass the VM entirely in this codebase).

## [0.10.0] - 2026-09-01 — Epic 7: Codegen (first slice)

`column-rs codegen "<SQL>" --out file.rs` (#98): compiles a query ahead of time into a standalone `.rs` source file with a `const PROGRAM: &[Opcode]` — no runtime SQL parsing, no dynamic dispatch, the query plan is baked into the binary at compile time. Scoped to flat, non-aggregating single-table queries for this first slice (no `JOIN`/semi-join/window/`GROUP BY`/`ORDER BY`/`LIMIT` yet — rejected with a clear error rather than silently compiled wrong); broadening this is tracked in #101. Epic #98 stays open.

### Added

- **`src/codegen.rs`**: `generate(sql) -> Result<String>` renders the compiled VM program as Rust source plus a runnable `main`
- **CLI**: `column-rs codegen "<SQL>" --out <file.rs>` subcommand
- **`src/query.rs`**: `output_column_names` and `run_program` (load + `run_parallel`, no cross-segment post-processing) extracted as `pub` helpers, shared by `QueryEngine` and `codegen.rs`

### Changed

- **`Opcode`/`Value` are now const-compatible**: `Opcode`'s dynamic fields (`LoadColumn.column`, `GroupReduce`'s `group_by`/`aggs`/`agg_dst`, `Emit.registers`) and `Value::Str` use `Cow<'static, str>`/`Cow<'static, [_]>` instead of `String`/`Vec`, so a `const PROGRAM` array can hold `Cow::Borrowed(...)` literals with zero heap allocation while runtime column data still owns its `String` via `Cow::Owned`. No behavior change.

## [0.9.0] - 2026-09-01

### Added

- **CLI multi-table sessions** (`src/query.rs`, `src/bin/column-rs`, #94): `QueryEngine` now loads more than one Parquet file into a single session (`open_many`/`add_table`), keyed by table name (each file's stem); `column-rs a.parquet b.parquet ...` (both REPL and `-c "<SQL>"` one-shot mode) loads every trailing path, and `.open <path> [AS <name>]` adds another file mid-session. Loading a duplicate table name is now a clear error instead of a silent replace.
- **`QueryEngine::execute` now dispatches to joins/semi-joins/windows**: previously it only ever ran the flat single-table path, even though `execute_joined`/`execute_semi_join`/`execute_windowed` already existed in the engine — a `JOIN`, `IN (SELECT ...)`, or window-function query typed at the CLI silently ran (or failed) as if it were a plain `SELECT`. It now inspects the parsed query's shape and routes accordingly.

## [0.8.1] - 2026-09-01

### Fixed

- **REPL readline: Left/Right/Home/End/Delete keys ignored** (#95): the terminal layer parsed these escape sequences but the line editor had no cursor concept and no match arms for them, so they were silently dropped. Added cursor tracking with mid-line insert/remove/redraw support.

## [0.8.0] - 2026-09-01 — Epic 4: Nested/Complex Types (Dremel encoding)

Struct/list/map Parquet columns — the deferred half of epic #12 (#54, #55, #56, #58, #59; epic tracked separately as #88).

### Added

- **Definition/repetition level decoder** (`src/schema_tree.rs`, #54): builds a tree from the flattened schema list and computes each node's max definition/repetition level per the Dremel encoding rule
- **Nested struct reader** (`src/nested.rs`, #55): reconstructs structs of scalars/structs, with per-row nullability tracked via the driver leaf's definition level
- **List/array reader** (`src/nested.rs`, #56): both 2- and 3-level `LIST` encodings, using repetition-level-0 boundaries to delimit rows (empty vs. null vs. populated lists all distinguished)
- **Map reader** (`src/nested.rs`, #58): `MAP`'s `key_value` group reconstructed as a list of `{key, value}` structs
- **`ParquetFile::read_nested_column`** (`src/file.rs`): new public API to reconstruct one top-level field (flat or nested) into `NestedValue`s; `ParquetFile::open` no longer rejects nested schemas outright (previously `FileError::UnsupportedNestedSchema`)
- Oracle tests for struct/list/map reconstruction (#59), using pyarrow-generated fixtures (DuckDB can't produce nested Parquet schemas)

### Known limitations

- Dictionary-encoded nested/repeated leaves aren't supported yet (`FileError::UnsupportedNestedDictionary`) — real writers (pyarrow, Spark) dictionary-encode nested leaves by default; tracked as #90

### Fixed

- `clippy::unused_io_amount` in `src/bin/column-rs/readline/term.rs` (short-read handling in escape-sequence parsing), surfaced by CI's newer clippy version

## [0.7.0] - 2026-09-01

Timestamps and decimals — the tractable half of epic #12 (Complex Types Read) (#50-53; nested types, #54-59, deliberately deferred, see #12).

### Added

- **INT64/INT96 timestamps** (`src/file.rs`, #50, #51): `read_timestamp_column` normalizes `TIMESTAMP_MILLIS`/`TIMESTAMP_MICROS` (INT64) and legacy INT96 (Julian day + nanoseconds) timestamps to microseconds since the Unix epoch, regardless of source encoding
- **`DECIMAL` logical type** (`src/decimal.rs`, `src/file.rs`, #52, #53): new `Decimal { unscaled: i128, scale: i32 }` preserving exact precision (no lossy `f64` conversion), with a fixed-point `Display` impl; `read_decimal_column` supports INT32, INT64, and `FIXED_LEN_BYTE_ARRAY` physical types
- **Schema logical-type parsing** (`src/footer.rs`): `SchemaElement` now parses `converted_type`/`scale`/`precision` (old-style `ConvertedType` annotation, still set by DuckDB/pyarrow/Spark alongside the newer `LogicalType` union)
- **INT96 dictionary encoding** (`src/reader.rs`): `decode_dictionary_int96`/`read_int96_column_dictionary` — didn't exist before this release; pyarrow dictionary-encodes INT96 by default, so legacy timestamps written that way were previously unreadable

## [0.6.0] - 2026-09-01 — Epic 5: Joins & Windows

Full analytics SQL support (joins, semi-joins, window functions) — epic #13 (#63-65, #67-71; #66 deliberately deferred, see #85).

### Added

- **SQL `JOIN` grammar and dotted/qualified identifiers** (`src/sql.rs`, #63, #64): `[INNER|LEFT] JOIN table ON col = col`; column names can now be qualified everywhere (e.g. `orders.id`), not just in joins
- **`query::execute_joined`** (#63, #64): in-memory equi-join — materializes both tables, builds a hash index on the right table's join column, probes with the left table's rows (INNER drops unmatched rows, LEFT keeps them with NULLs), then feeds the combined batch through the existing VM pipeline for WHERE/GROUP BY/SELECT
- **`col IN (SELECT ...)` semi-join** (`src/sql.rs`, `query::execute_semi_join`, #65): recursive subquery parsing, plus `query::execute_semi_join` evaluating the subquery and filtering by membership
- **Window functions** (`src/sql.rs`, `query::execute_windowed`, #67-70): `ROW_NUMBER`/`RANK`/`DENSE_RANK`, `LAG`/`LEAD`, `FIRST_VALUE`/`LAST_VALUE`, `SUM`/`AVG`/`COUNT OVER`, with `PARTITION BY`/`ORDER BY`. Computed directly over a materialized batch (partition + sort) rather than through VM opcodes, since windowing doesn't fit the register-machine model. `LAST_VALUE`'s default-frame behavior (returns the *current* row, per the SQL standard's default frame) verified against real DuckDB output.
- **6 new DuckDB oracle tests** (#71) covering joins, semi-joins, and all window function families

### Fixed

- **Plain `SELECT` columns silently dropped** (`src/query.rs`): `compile()` never loaded or emitted non-aggregate, non-`GROUP BY` `SELECT` columns at all — a flat `SELECT col1, col2 FROM t` returned zero rows via `query::execute` with no error. Predates this epic (no prior test exercised a flat `SELECT` through `query::execute`); found while adding the join oracle test.

## [0.5.1] - 2026-09-01

### Fixed

- **Nested/repeated schemas silently returned wrong data** (`src/file.rs`, #61): `ParquetFile::open`/`RowGroupReader::read_*_column` previously "succeeded" on struct/list/map schemas and returned incorrect values (or all-`None`) with no error, since `max_definition_level` assumed a flat schema. `ParquetFile::open` now checks the schema is flat and returns `FileError::UnsupportedNestedSchema` up front instead. Actual nested/repeated column support (repetition-level decoding, per-leaf max-definition-level) remains unimplemented — tracked as a stretch goal on #61, not attempted here.

## [0.5.0] - 2026-09-01

### Added

- **INT32, FLOAT, FIXED_LEN_BYTE_ARRAY, and INT96 physical types** (`src/reader.rs`, `src/file.rs`, `src/footer.rs`, #60): previously unreadable at all — INT32/FLOAT (used by e.g. `int8`/`int16`/`int32`/`date32`/`float32` columns) failed with `Read(UnexpectedEof)` misrouted through the INT64/DOUBLE readers, and FIXED_LEN_BYTE_ARRAY (decimals, fixed binary)/INT96 (legacy timestamps) had no reader function at all. `SchemaElement` now parses `type_length`, needed for FIXED_LEN_BYTE_ARRAY. Also fixed `src/query.rs`'s `RowGroupSegment::load`, which misrouted `PhysicalType::Int32` to the INT64 reader; added a `Float` branch too.

## [0.4.1] - 2026-09-01

### Fixed

- **Silent truncation of multi-page column chunks** (`src/file.rs`, #49): `read_*_column` only ever read the first page (or first dictionary+data page pair) of a chunk, silently dropping the rest — invisible against DuckDB-written fixtures (which always write one page per chunk regardless of size) but a real correctness bug against files from parquet-mr/pyarrow/Spark, which routinely split large chunks into many pages and commonly fall back from dictionary to `PLAIN` encoding mid-chunk. Also fixed a related `column_chunk_bytes` off-by-(dictionary-page-size) bug this surfaced, and made plain-vs-dictionary page decoding depend on each page's own encoding rather than "has any dictionary page been seen in this chunk."

## [0.4.0] - 2026-09-01 — Epic 3: Compression Read

Read compressed and more richly-encoded Parquet column data — epic #11 (#41, #43-#46; #40 and #42 were already satisfied by prior work).

### Added

- **DELTA_BINARY_PACKED decoder** (`src/encoding.rs`, `src/reader.rs`, #43): block/miniblock delta decoding for INT64 columns, matching DuckDB's V2 writer output
- **Hand-rolled Snappy decompressor** (`src/compression/snappy.rs`, #44): raw (unframed) block format, plus codec-aware page decompression wired through `src/file.rs` (`Codec` parsed from `ColumnMetaData`)
- **ZSTD decompression** (`src/compression/zstd.rs`, #45): via the pure-Rust `ruzstd` crate, after a hand-rolled attempt (matching Snappy's approach) hit bit-ordering bugs in the FSE/Huffman entropy coding that proved too open-ended without a reference decoder to diff against — see #47 for a possible future revisit
- **Raw dictionary indices** (`src/reader.rs`, `src/file.rs`, #41): `read_*_column_dictionary_indices` returns `(dictionary, Vec<Option<u32>>)` instead of eagerly resolving every row, so callers like `GROUP BY` can compare/group on indices instead of allocating a `String` per row
- **Oracle tests against real compressed files** (#46): DuckDB-written Snappy, ZSTD, and DELTA_BINARY_PACKED fixtures, plus a production-shaped multi-row-group file combining ZSTD + dictionary encoding + the query VM's parallel scan + `GROUP BY`

## [0.3.0] - 2026-09-01 — Epic 2: Query VM

Vectorized SQL query engine over Parquet row groups — epic #10 (#27-#35).

### Added

- **SQL parser** (`src/sql.rs`, #27): recursive-descent parser for `SELECT ... FROM ... [WHERE ...] [GROUP BY ...] [ORDER BY ...] [LIMIT ...]`, including arithmetic/comparison/boolean expressions and `COUNT`/`SUM`/`AVG`/`MIN`/`MAX` aggregates
- **Vectorized register VM** (`src/vm.rs`, #28-#33): a batch-oriented (1024-row) register machine with opcodes `LoadColumn`, `LoadConst`, `Map` (arithmetic/comparison/boolean), `Filter`, `Reduce` (whole-column aggregates), `GroupReduce` (hash aggregation), and `Scan`/`Emit`/`NextSegment`/`Halt` for driving execution across a table's segments
- **Parallel segment scan** (`src/vm.rs`, #34): `run_parallel` drives one segment per rayon task (morsel-driven), concatenating emitted rows in segment order; new `rayon` dependency
- **SQL-to-VM query planner** (`src/query.rs`, #35): compiles a parsed `Query` into a VM program, executes it against a `ParquetFile`'s row groups in parallel, and merges partial per-segment aggregates (sum/count pairs for `AVG`, associative min/max/sum/count for the rest) before applying `ORDER BY`/`LIMIT`
- **DuckDB oracle tests for the query VM** (`tests/oracle.rs`, #35): `WHERE`+`GROUP BY`+`SUM` and `WHERE`+`COUNT(*)` results validated against DuckDB

## [0.2.1] - 2026-09-01

### Fixed

- **PLAIN_DICTIONARY column decoding** (`src/page.rs`, `src/reader.rs`, `src/file.rs`, #38): `RowGroupReader` only handled PLAIN-encoded data pages, so DuckDB-written low-cardinality columns (dictionary-encoded) failed to decode. Added DICTIONARY_PAGE header parsing, dictionary-index decoding via the existing RLE/Bit-Packed Hybrid decoder, and per-type dictionary column readers for INT64, DOUBLE, BOOLEAN, and BYTE_ARRAY/string columns.

## [0.2.0] - 2026-09-01 — Epic 1: Parquet Reader

Read-only, PLAIN-encoded Parquet reader with no external dependencies beyond `memmap2` — epic #9 (#15-#24).

### Added

- **Thrift Compact Protocol decoder** (`src/thrift.rs`, #15): varint/zigzag decoding, struct field headers with delta encoding, list/set/map decoding, nested struct support — schema-less, decodes into a generic `Value` enum keyed by field id
- **Footer parser** (`src/footer.rs`, #16): locates `FileMetaData` via the trailing `[length][PAR1]` suffix and maps it into typed `SchemaElement`/`RowGroup`/`ColumnChunk`/`ColumnMetaData`
- **PLAIN column readers** (`src/reader.rs`, #17-#20): `read_int64_column`, `read_double_column`, `read_boolean_column` (bit-packed), `read_string_column` (BYTE_ARRAY, UTF-8), sharing a common data-page assembly path
- **Definition levels** (`src/encoding.rs`, #21): RLE/Bit-Packed Hybrid decoder plus `null_mask`, mapping decoded levels to a present/null bitmap for nullable columns
- **Multi-row-group iteration** (`src/file.rs`, #22): `ParquetFile`/`RowGroupReader` expose a lazy `row_groups()` iterator and per-column typed readers, deriving each column's max definition level from its schema repetition
- **Memory-mapped I/O** (`src/mmap.rs`, #23): `MmapFile`, a thin wrapper over `memmap2` — the crate's one external dependency exception
- **DuckDB oracle test harness** (`tests/oracle.rs`, #24): validates every column type, mixed types, nullable columns, and a 100K-row file against real DuckDB output; CI (`.github/workflows/ci.yml`) pins DuckDB v1.5.5

## [0.1.0] - 2026-09-01

Initial commit: project scaffolding.
