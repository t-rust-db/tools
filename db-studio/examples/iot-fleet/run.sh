#!/usr/bin/env bash
# Generates the fixtures (if missing) and launches db-studio against
# fleet.sqlite, readings.parquet, and device.log as three open files.
#
#   ./run.sh
#
# This doesn't loop over queries/*.sql and print output the way
# sqlite-rs/column-rs's own examples do -- db-studio is an interactive
# TUI, not a batch query runner. Once it's open, paste any queries/*.sql
# query into the query pane (Tab cycles focus, F1 shows results). See
# ./README.md for what each open file demonstrates and the current
# cross-mode-join and .log limitations.
#
# ## Which db-studio binary?
#
#   1. $DB_STUDIO                                  -- explicit path
#   2. `db-studio` on $PATH                         -- installed build
#   3. ../../target/release/db-studio               -- built in place,
#      the normal case when running from inside this checkout of
#      t-rust-db/db-studio (examples/iot-fleet/ -> repo root)
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$ROOT"

if [ -n "${DB_STUDIO:-}" ]; then
    BIN="$DB_STUDIO"
elif command -v db-studio >/dev/null 2>&1; then
    BIN="$(command -v db-studio)"
else
    BIN="$ROOT/../../target/release/db-studio"
fi

if [ ! -x "$BIN" ]; then
    echo "error: no db-studio binary found at '$BIN'" >&2
    echo "  set \$DB_STUDIO to an explicit binary path, or put db-studio on \$PATH," >&2
    echo "  or build it in place: (cd ../.. && cargo build --release --bin db-studio)" >&2
    exit 1
fi

if [ ! -f fixture/fleet.sqlite ] || [ ! -f fixture/readings.parquet ] || [ ! -f fixture/device.log ]; then
    ./fixture/generate.sh
fi

exec "$BIN" fixture/fleet.sqlite fixture/readings.parquet fixture/device.log
