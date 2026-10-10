#!/usr/bin/env bash
# Generates the iot-fleet fixtures used by ../queries/*.sql:
#   - fleet.sqlite      (row mode)    -- sites/devices/sensors dimensions
#   - readings.parquet  (batch mode)  -- device/sensor time-series metrics
#   - device.log        (stream mode) -- device events, real Heroku-style logfmt
# Re-run any time to regenerate -- all three are gitignored.
#
# All three are generated from one DuckDB session (generate_series +
# random() give a fleet-sized dataset without hand-listing hundreds of
# rows): dimensions are exported to CSV and imported into fleet.sqlite via
# sqlite3's own `.import` (fleet.sqlite is still a real SQLite file, built
# by the real sqlite3 CLI, not by DuckDB's SQLite writer extension), the
# readings go straight to Parquet, and the log lines are assembled as
# fully-formatted text and written via `COPY ... (FORMAT CSV, QUOTE '')`
# with quoting disabled -- the default CSV writer would otherwise wrap
# and escape the double quotes already inside each `msg="..."` field and
# corrupt the logfmt.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$ROOT"

command -v sqlite3 >/dev/null 2>&1 || { echo "error: sqlite3 not found on PATH" >&2; exit 1; }
command -v duckdb >/dev/null 2>&1 || { echo "error: duckdb not found on PATH" >&2; exit 1; }

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

# --- Dimensions + readings + log, all generated in one DuckDB session ---
duckdb -batch <<SQL
CREATE TABLE sites AS
SELECT * FROM (VALUES
    (1, 'Rotterdam Warehouse', 'EU-West'),
    (2, 'Hamburg Depot', 'EU-West'),
    (3, 'Singapore Hub', 'APAC'),
    (4, 'Austin Distribution Center', 'AMER'),
    (5, 'Krakow Fulfillment Center', 'EU-Central'),
    (6, 'Osaka Logistics Park', 'APAC')
) AS t(id, name, region);

-- 60 devices spread round-robin across the 6 sites, model/firmware
-- picked from a small realistic catalog by device id.
CREATE TABLE devices AS
WITH catalog(model, firmware) AS (
    VALUES
        ('EdgeSense-200', '1.4.2'),
        ('EdgeSense-300', '2.0.1'),
        ('EdgeSense-300', '1.9.5'),
        ('EdgeSense-400', '2.3.0')
)
SELECT
    d.id,
    c.model,
    c.firmware,
    ((d.id - 1) % 6) + 1 AS site_id
FROM range(1, 61) AS d(id)
JOIN catalog AS c ON true
QUALIFY row_number() OVER (PARTITION BY d.id ORDER BY random()) = 1;

-- 2-3 sensors per device (id-parity decides which), type cycling
-- through a fixed profile list so every sensor's base/spread is known.
CREATE TABLE sensor_profile(type, unit, base, spread) AS
SELECT * FROM (VALUES
    ('temperature', 'celsius', 21.0, 3.0),
    ('humidity',    'percent', 45.0, 8.0),
    ('vibration',   'mm_s',    1.0,  0.7),
    ('pressure',    'hpa',     1013.0, 15.0)
);

CREATE TABLE sensors AS
WITH per_device AS (
    SELECT id AS device_id, CASE WHEN id % 2 = 0 THEN 3 ELSE 2 END AS sensor_count
    FROM devices
),
expanded AS (
    SELECT device_id, n, (device_id * 7 + n * 3) % 4 AS profile_idx
    FROM per_device, range(0, sensor_count) AS t(n)
),
profiles AS (
    SELECT type, unit, row_number() OVER () - 1 AS idx FROM sensor_profile
)
SELECT
    row_number() OVER (ORDER BY e.device_id, e.n) AS id,
    e.device_id,
    p.type,
    p.unit
FROM expanded e
JOIN profiles p ON p.idx = e.profile_idx
ORDER BY e.device_id, e.n;

COPY sites TO '$WORK/sites.csv' (HEADER, DELIMITER ',');
COPY devices TO '$WORK/devices.csv' (HEADER, DELIMITER ',');
COPY sensors TO '$WORK/sensors.csv' (HEADER, DELIMITER ',');

-- --- readings.parquet: one row per (sensor, 5-minute tick) over 24h ---
COPY (
    WITH profiles AS (
        SELECT type, base, spread FROM sensor_profile
    ),
    sensor_stats AS (
        SELECT s.id AS sensor_id, s.device_id, p.base, p.spread
        FROM sensors s
        JOIN profiles p ON p.type = s.type
    ),
    ticks AS (
        SELECT range AS i FROM range(0, 288)
    )
    SELECT
        st.device_id,
        st.sensor_id,
        TIMESTAMP '2026-09-10 00:00:00' + (t.i * INTERVAL 5 MINUTE) AS ts,
        ROUND(st.base + (sin((t.i + st.sensor_id) / 12.0) * st.spread) + (random() * st.spread * 0.2), 2) AS value
    FROM sensor_stats st
    CROSS JOIN ticks t
    ORDER BY ts, st.sensor_id
) TO '$ROOT/readings.parquet' (FORMAT PARQUET);

-- --- device.log: procedurally generated events across all 60 devices ---
COPY (
    WITH event_kind(idx, event, level, msg, detail_key) AS (
        VALUES
            (0, 'heartbeat',     'info',  'heartbeat ok',                     NULL),
            (1, 'connectivity',  'error', 'connectivity lost',                'retries'),
            (2, 'connectivity',  'info',  'connectivity restored',            NULL),
            (3, 'firmware',      'info',  'firmware check ok',                'firmware'),
            (4, 'vibration',     'warn',  'vibration threshold exceeded',     'reading'),
            (5, 'sensor_timeout','error', 'sensor read timeout',              'sensor'),
            (6, 'battery',       'warn',  'battery low',                      'battery_pct'),
            (7, 'humidity',      'warn',  'humidity sensor reading unstable', NULL)
    ),
    device_sites AS (
        SELECT id AS device_id, site_id FROM devices
    ),
    -- ~12 events per device, event kind picked pseudo-randomly per row
    raw AS (
        SELECT
            d.device_id,
            d.site_id,
            e.event,
            e.level,
            e.msg,
            e.detail_key,
            TIMESTAMP '2026-09-10 00:00:00' + ((d.device_id * 37 + n * 53) % (96 * 4)) * INTERVAL 15 MINUTE AS ts
        FROM device_sites d
        CROSS JOIN range(0, 12) AS t(n)
        JOIN event_kind e ON e.idx = (d.device_id * 31 + n * 17) % 8
    ),
    sensor_pick AS (
        SELECT device_id, MIN(id) AS sensor_id FROM sensors GROUP BY device_id
    ),
    detailed AS (
        SELECT
            r.*,
            CASE r.detail_key
                WHEN 'retries' THEN CAST(1 + ((r.device_id + epoch(r.ts)::BIGINT) % 5) AS VARCHAR)
                WHEN 'firmware' THEN '2.' || CAST((r.device_id % 3) AS VARCHAR) || '.' || CAST((r.device_id % 10) AS VARCHAR)
                WHEN 'reading' THEN CAST(ROUND(0.5 + ((r.device_id + epoch(r.ts)::BIGINT) % 30) / 10.0, 1) AS VARCHAR)
                WHEN 'sensor' THEN CAST(COALESCE((SELECT sensor_id FROM sensor_pick WHERE device_id = r.device_id), 1) AS VARCHAR)
                WHEN 'battery_pct' THEN CAST(5 + ((r.device_id + epoch(r.ts)::BIGINT) % 20) AS VARCHAR)
                ELSE NULL
            END AS detail_value
        FROM raw r
    )
    SELECT
        'ts=' || strftime(ts, '%Y-%m-%dT%H:%M:%SZ')
            || ' level=' || level
            || ' device=' || device_id
            || ' site=' || site_id
            || ' event=' || event
            || ' msg="' || msg || '"'
            || CASE WHEN detail_key IS NOT NULL THEN ' ' || detail_key || '=' || detail_value ELSE '' END
        AS line
    FROM detailed
    ORDER BY ts
) TO '$WORK/device.log' (FORMAT CSV, HEADER false, QUOTE '', ESCAPE '');
SQL

# --- fleet.sqlite: import the dimensions generated above via sqlite3 ---
rm -f fleet.sqlite
sqlite3 fleet.sqlite <<SQL
CREATE TABLE sites(id INTEGER PRIMARY KEY, name TEXT, region TEXT);
CREATE TABLE devices(id INTEGER PRIMARY KEY, model TEXT, firmware TEXT, site_id INTEGER);
CREATE TABLE sensors(id INTEGER PRIMARY KEY, device_id INTEGER, type TEXT, unit TEXT);
.mode csv
.import --skip 1 $WORK/sites.csv sites
.import --skip 1 $WORK/devices.csv devices
.import --skip 1 $WORK/sensors.csv sensors
SQL
echo "wrote $ROOT/fleet.sqlite ($(sqlite3 fleet.sqlite 'select count(*) from devices') devices, $(sqlite3 fleet.sqlite 'select count(*) from sensors') sensors)"
echo "wrote $ROOT/readings.parquet"

mv "$WORK/device.log" device.log
echo "wrote $ROOT/device.log ($(wc -l < device.log | tr -d ' ') lines)"
