#!/usr/bin/env bash
# DuckDB parity benchmark runner (#100).
#
#   ./benches/run.sh                 # medium (100K rows)
#   ./benches/run.sh large           # 10M rows
#   ./benches/run.sh medium scan     # one query only
#
# Runs every benches/queries/*.sql against both column-rs and DuckDB with the
# *identical* SQL text: DuckDB gets views named after the tables column-rs
# derives from the file stems (`bench`, `bench_customers`), so neither engine
# is handed a rewritten query.
#
# Results land in benches/results/<size>_<timestamp>/ as hyperfine JSON plus a
# peak-RSS reading per engine, and report.py renders the comparison table.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

SIZE="${1:-medium}"
ONLY="${2:-}"
WARMUP="${WARMUP:-3}"
MIN_RUNS="${MIN_RUNS:-10}"

DATA="benches/data"
FACT="$DATA/${SIZE}.parquet"
DIM="$DATA/${SIZE}_customers.parquet"
BIN="target/release/column-rs"

for tool in duckdb hyperfine; do
    command -v "$tool" >/dev/null 2>&1 || { echo "error: $tool not found on PATH" >&2; exit 1; }
done

if [ ! -f "$FACT" ] || [ ! -f "$DIM" ]; then
    echo "error: missing benchmark data for size '$SIZE'" >&2
    echo "  generate it with: ./benches/data/generate.sh $SIZE" >&2
    exit 1
fi

[ -x "$BIN" ] || cargo build --release

# column-rs names a table after its file stem, so the data files are linked
# under stable names and the queries can hardcode `bench`/`bench_customers`.
LINKS="$(mktemp -d)"
trap 'rm -rf "$LINKS"' EXIT
ln -sf "$ROOT/$FACT" "$LINKS/bench.parquet"
ln -sf "$ROOT/$DIM" "$LINKS/bench_customers.parquet"

# Give DuckDB the same table names via views over the same files.
INIT="$LINKS/init.sql"
cat > "$INIT" <<EOF
CREATE VIEW bench AS SELECT * FROM read_parquet('$LINKS/bench.parquet');
CREATE VIEW bench_customers AS SELECT * FROM read_parquet('$LINKS/bench_customers.parquet');
EOF

RESULTS="benches/results/${SIZE}_$(date +%Y%m%d_%H%M%S)"
mkdir -p "$RESULTS"

# Peak resident set size in KB. GNU time reports `Maximum resident set size
# (kbytes)`; BSD/macOS `time -l` reports `maximum resident set size` in bytes.
peak_rss_kb() {
    local out
    if /usr/bin/time -v true >/dev/null 2>&1; then
        out="$(/usr/bin/time -v "$@" 2>&1 >/dev/null | awk '/Maximum resident set size/ {print $NF}')"
        echo "${out:-0}"
    else
        out="$(/usr/bin/time -l "$@" 2>&1 >/dev/null | awk '/maximum resident set size/ {print $1}')"
        echo $(( ${out:-0} / 1024 ))
    fi
}

echo "size=$SIZE  fact=$(du -h "$FACT" | cut -f1)  results=$RESULTS"
echo

for query in benches/queries/*.sql; do
    name="$(basename "$query" .sql)"
    [ -z "$ONLY" ] || [ "$ONLY" = "$name" ] || continue

    # Strip the leading `--` commentary so both CLIs get a bare statement.
    sql="$(grep -v '^[[:space:]]*--' "$query" | tr '\n' ' ' | sed 's/  */ /g; s/^ //; s/ $//')"

    echo "=== $name ==="
    echo "    $sql"

    hyperfine --warmup "$WARMUP" --min-runs "$MIN_RUNS" \
        --command-name "column-rs" \
        --export-json "$RESULTS/${name}_column-rs.json" \
        "$BIN -c \"$sql\" $LINKS/bench.parquet $LINKS/bench_customers.parquet"

    hyperfine --warmup "$WARMUP" --min-runs "$MIN_RUNS" \
        --command-name "duckdb" \
        --export-json "$RESULTS/${name}_duckdb.json" \
        "duckdb -init $INIT -batch -noheader -c \"$sql\""

    {
        echo "column-rs $(peak_rss_kb "$BIN" -c "$sql" "$LINKS/bench.parquet" "$LINKS/bench_customers.parquet")"
        echo "duckdb $(peak_rss_kb duckdb -init "$INIT" -batch -noheader -c "$sql")"
    } > "$RESULTS/${name}_memory.txt"

    echo
done

# Binary size is a headline metric in #100 (target: <= 5MB).
wc -c < "$BIN" | tr -d ' ' > "$RESULTS/binary_size_bytes.txt"

python3 benches/report.py "$RESULTS" | tee "$RESULTS/report.md"
