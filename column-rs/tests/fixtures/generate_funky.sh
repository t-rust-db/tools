#!/usr/bin/env bash
# Regenerate the funky.parquet trio: a themed, deliberately weird-but-valid
# fixture set exercising a wide mix of types (nullable columns, a decimal,
# a timestamp, booleans, dictionary-friendly repeated strings, ZSTD) and,
# across the three files, a join-able star-schema-ish shape:
#
#   funky.parquet (performers) --venue_id--> funky_venues.parquet
#                              --act-------> funky_acts.parquet
#
# Not DuckDB oracle fixtures for a specific narrow feature -- more of a
# "kick the tyres" smoke-test set for manual poking, demos, and join tests.
#
# Pinned DuckDB version: v1.5.5 (Variegata) -- see generate.sh.
set -euo pipefail
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$DIR"

duckdb -c "
COPY (
  SELECT
    i AS performer_id,
    (['Blazeworthy', 'Miss Ricochet', 'The Unravelling Nun', 'Sir Combusts-a-Lot',
       'Doctor Kaboodle', 'Vermilion Volt', 'The Gyroscopic Twins', 'Uncle Fandango'
      ])[1 + (i % 8)] AS stage_name,
    (['trapeze', 'fire-breathing', 'clown car repair', 'lion whispering',
       'unicycle chess', 'sword swallowing', 'human cannonball', 'shadow puppetry'
      ])[1 + (i % 8)] AS act,
    CASE WHEN i % 13 = 0 THEN NULL ELSE (50 + (i * 7) % 50 + (i % 100) / 100.0)::DECIMAL(6,2) END AS crowd_rating,
    (TIMESTAMP '2026-04-01 20:00:00' + INTERVAL (i * 37) MINUTE) AS showtime,
    (i % 5 = 0) AS is_headliner,
    (300 + (i * 17) % 4700) AS audience_size,
    CASE
      WHEN i % 29 = 0 THEN NULL
      WHEN i % 11 = 0 THEN 'contract renegotiation pending'
      WHEN i % 7 = 0 THEN 'requires extra safety net'
      ELSE 'business as usual'
    END AS notes,
    (1 + (i % 6)) AS venue_id
  FROM range(1, 1001) t(i)
) TO 'funky.parquet' (FORMAT PARQUET, COMPRESSION ZSTD, ROW_GROUP_SIZE 250);

COPY (
  SELECT
    v AS venue_id,
    (['The Big Top', 'Sawdust Palace', 'The Velvet Cannon', 'Moonlight Amphitheatre',
       'The Rickety Pavilion', 'Starlight Fairgrounds'
      ])[v] AS venue_name,
    (['Ghent', 'Brussels', 'Antwerp', 'Bruges', 'Leuven', 'Namur'])[v] AS city,
    (800 + v * 650) AS capacity,
    (v % 2 = 0) AS has_permanent_roof
  FROM range(1, 7) t(v)
) TO 'funky_venues.parquet' (FORMAT PARQUET, COMPRESSION UNCOMPRESSED);

COPY (
  SELECT
    act,
    danger_level,
    insurance_cost_per_show
  FROM (VALUES
    ('trapeze',           4, 120.50),
    ('fire-breathing',    5, 340.00),
    ('clown car repair',  1,  15.75),
    ('lion whispering',   5, 500.00),
    ('unicycle chess',    2,  22.00),
    ('sword swallowing',  4, 210.25),
    ('human cannonball',  5, 610.00),
    ('shadow puppetry',   1,   8.50)
  ) AS t(act, danger_level, insurance_cost_per_show)
) TO 'funky_acts.parquet' (FORMAT PARQUET, COMPRESSION UNCOMPRESSED);
"

echo "Fixtures written to $DIR:"
echo
echo "funky.parquet -- performers (1000 rows)"
echo "  performer_id (BIGINT), stage_name/act (dictionary-friendly VARCHAR),"
echo "  crowd_rating (nullable DECIMAL(6,2)), showtime (TIMESTAMP), is_headliner (BOOLEAN),"
echo "  audience_size (BIGINT), notes (nullable VARCHAR, mostly repeated), venue_id (BIGINT, FK)"
echo
echo "funky_venues.parquet -- venues (6 rows), joined via funky.venue_id = funky_venues.venue_id"
echo "  venue_id (BIGINT), venue_name/city (VARCHAR), capacity (BIGINT), has_permanent_roof (BOOLEAN)"
echo
echo "funky_acts.parquet -- act catalogue (8 rows), joined via funky.act = funky_acts.act"
echo "  act (VARCHAR), danger_level (BIGINT), insurance_cost_per_show (DOUBLE)"
