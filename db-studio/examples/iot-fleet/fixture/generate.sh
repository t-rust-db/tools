#!/usr/bin/env bash
# Generates the iot-fleet fixtures used by ../queries/*.sql:
#   - fleet.sqlite   (row mode)   -- sites/devices/sensors dimensions, via sqlite3
#   - readings.parquet (batch mode) -- device/sensor time-series metrics, via DuckDB
#   - device.log     (stream mode, not yet openable by db-studio -- see ../README.md)
# Re-run any time to regenerate -- all three are gitignored.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$ROOT"

command -v sqlite3 >/dev/null 2>&1 || { echo "error: sqlite3 not found on PATH" >&2; exit 1; }
command -v duckdb >/dev/null 2>&1 || { echo "error: duckdb not found on PATH" >&2; exit 1; }

# --- fleet.sqlite: dimensions -----------------------------------------
rm -f fleet.sqlite
sqlite3 fleet.sqlite <<'SQL'
CREATE TABLE sites(id INTEGER PRIMARY KEY, name TEXT, region TEXT);
CREATE TABLE devices(id INTEGER PRIMARY KEY, model TEXT, firmware TEXT, site_id INTEGER);
CREATE TABLE sensors(id INTEGER PRIMARY KEY, device_id INTEGER, type TEXT, unit TEXT);

INSERT INTO sites(id, name, region) VALUES (1, 'Rotterdam Warehouse', 'EU-West');
INSERT INTO sites(id, name, region) VALUES (2, 'Hamburg Depot', 'EU-West');
INSERT INTO sites(id, name, region) VALUES (3, 'Singapore Hub', 'APAC');

INSERT INTO devices(id, model, firmware, site_id) VALUES (1, 'EdgeSense-200', '1.4.2', 1);
INSERT INTO devices(id, model, firmware, site_id) VALUES (2, 'EdgeSense-200', '1.4.2', 1);
INSERT INTO devices(id, model, firmware, site_id) VALUES (3, 'EdgeSense-300', '2.0.1', 2);
INSERT INTO devices(id, model, firmware, site_id) VALUES (4, 'EdgeSense-300', '2.0.1', 2);
INSERT INTO devices(id, model, firmware, site_id) VALUES (5, 'EdgeSense-300', '1.9.5', 3);

INSERT INTO sensors(id, device_id, type, unit) VALUES (1, 1, 'temperature', 'celsius');
INSERT INTO sensors(id, device_id, type, unit) VALUES (2, 1, 'humidity', 'percent');
INSERT INTO sensors(id, device_id, type, unit) VALUES (3, 2, 'temperature', 'celsius');
INSERT INTO sensors(id, device_id, type, unit) VALUES (4, 3, 'temperature', 'celsius');
INSERT INTO sensors(id, device_id, type, unit) VALUES (5, 3, 'vibration', 'mm_s');
INSERT INTO sensors(id, device_id, type, unit) VALUES (6, 4, 'humidity', 'percent');
INSERT INTO sensors(id, device_id, type, unit) VALUES (7, 5, 'temperature', 'celsius');
INSERT INTO sensors(id, device_id, type, unit) VALUES (8, 5, 'vibration', 'mm_s');
SQL
echo "wrote $ROOT/fleet.sqlite"

# --- readings.parquet: metrics -----------------------------------------
# One reading every 5 minutes for 24h, per sensor above (8 sensors * 288 = 2304 rows),
# with a per-sensor base value/noise range so temperature/humidity/vibration look distinct.
rm -f readings.parquet
duckdb -batch <<'SQL'
COPY (
    WITH sensor_profile(sensor_id, device_id, base, spread) AS (
        VALUES
            (1, 1, 21.0, 3.0),
            (2, 1, 45.0, 8.0),
            (3, 2, 22.5, 2.5),
            (4, 3, 19.0, 4.0),
            (5, 3, 0.8,  0.6),
            (6, 4, 50.0, 10.0),
            (7, 5, 24.0, 3.5),
            (8, 5, 1.2,  0.9)
    ),
    ticks AS (
        SELECT range AS i FROM range(0, 288)
    )
    SELECT
        p.device_id,
        p.sensor_id,
        TIMESTAMP '2026-09-10 00:00:00' + (t.i * INTERVAL 5 MINUTE) AS ts,
        ROUND(p.base + (sin(t.i / 12.0) * p.spread) + (random() * p.spread * 0.2), 2) AS value
    FROM sensor_profile p
    CROSS JOIN ticks t
    ORDER BY ts, p.sensor_id
) TO 'readings.parquet' (FORMAT PARQUET);
SQL
echo "wrote $ROOT/readings.parquet"

# --- device.log: raw events ---------------------------------------------
# db-studio can't open .log files yet (stream mode is M5, not built) --
# this fixture is forward-looking, see ../README.md.
rm -f device.log
cat > device.log <<'LOG'
2026-09-10T00:03:11Z INFO  device=1 site=1 msg="heartbeat ok"
2026-09-10T02:14:47Z WARN  device=3 site=2 msg="humidity sensor reading unstable"
2026-09-10T05:41:02Z INFO  device=5 site=3 msg="firmware check ok" firmware=1.9.5
2026-09-10T08:22:59Z ERROR device=2 site=1 msg="connectivity lost" retries=3
2026-09-10T08:24:10Z INFO  device=2 site=1 msg="connectivity restored"
2026-09-10T11:05:33Z WARN  device=5 site=3 msg="vibration threshold exceeded" value=2.8
2026-09-10T14:47:20Z INFO  device=4 site=2 msg="heartbeat ok"
2026-09-10T18:02:15Z ERROR device=1 site=1 msg="sensor read timeout" sensor=1
2026-09-10T21:30:44Z INFO  device=3 site=2 msg="firmware check ok" firmware=2.0.1
2026-09-10T23:58:02Z WARN  device=2 site=1 msg="battery low" level=12
LOG
echo "wrote $ROOT/device.log"
