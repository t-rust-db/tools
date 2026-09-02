#!/usr/bin/env bash
# Generate the Parquet datasets used by the DuckDB parity benchmark (#100).
#
# Datasets are NOT committed (see .gitignore) -- regenerate them with:
#
#   ./benches/data/generate.sh              # small + medium + large
#   ./benches/data/generate.sh small        # just one size
#   ./benches/data/generate.sh xlarge       # opt-in, ~10GB, slow
#
# Pinned DuckDB version: v1.5.5 (Variegata) -- the same version tests/oracle.rs
# pins, so generated data is byte-reproducible across machines.
#
# Each size produces two files:
#   <size>.parquet          fact table: id, amount, region, customer_id
#   <size>_customers.parquet  dim table: customer_id, tier  (for the JOIN query)
#
# The table name column-rs uses is the file stem, so queries in benches/queries/
# refer to `bench` and `bench_customers`; run.sh symlinks the chosen size into
# place under those names.
set -euo pipefail

DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$DIR"

if ! command -v duckdb >/dev/null 2>&1; then
    echo "error: duckdb not found on PATH (needed to generate benchmark data)" >&2
    exit 1
fi

# Row counts per size label.
rows_for() {
    case "$1" in
        small)  echo 1000 ;;
        medium) echo 100000 ;;
        large)  echo 10000000 ;;
        xlarge) echo 100000000 ;;
        *) echo "error: unknown size '$1' (small|medium|large|xlarge)" >&2; exit 1 ;;
    esac
}

# Number of distinct customers, kept well below the row count so the JOIN and
# GROUP BY queries see realistic fan-out.
customers_for() {
    case "$1" in
        small)  echo 50 ;;
        medium) echo 1000 ;;
        large)  echo 100000 ;;
        xlarge) echo 1000000 ;;
    esac
}

generate() {
    local size="$1"
    local rows customers
    rows="$(rows_for "$size")"
    customers="$(customers_for "$size")"

    echo "==> $size: $rows rows, $customers customers"

    # `amount` is a deterministic pseudo-uniform spread over [0, 10000): the
    # multiply-and-mod keeps it reproducible without an RNG seed, so the fixed
    # thresholds in benches/queries/filter_*.sql select a stable row fraction
    # (median ~5000, 99th percentile ~9900) at every size.
    duckdb -c "
    COPY (
      SELECT
        i AS id,
        (((i * 2654435761) % 1000000) / 100.0)::DOUBLE AS amount,
        ['north', 'south', 'east', 'west'][(i % 4) + 1] AS region,
        (i % $customers) AS customer_id
      FROM range(0, $rows) t(i)
    ) TO '${size}.parquet' (FORMAT PARQUET, COMPRESSION SNAPPY);
    "

    duckdb -c "
    COPY (
      SELECT
        i AS customer_id,
        ['bronze', 'silver', 'gold'][(i % 3) + 1] AS tier
      FROM range(0, $customers) t(i)
    ) TO '${size}_customers.parquet' (FORMAT PARQUET, COMPRESSION SNAPPY);
    "

    ls -lh "${size}.parquet" "${size}_customers.parquet"
}

if [ "$#" -gt 0 ]; then
    for size in "$@"; do generate "$size"; done
else
    for size in small medium large; do generate "$size"; done
fi
