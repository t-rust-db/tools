.PHONY: help generate run check check-sqlite check-parquet clean

FIXTURE := fixture

.DEFAULT_GOAL := help

help: ## Show this list of targets
	@grep -E '^[a-zA-Z0-9_-]+:.*## ' $(MAKEFILE_LIST) | \
		awk 'BEGIN {FS = ":.*## "}; {printf "  %-14s %s\n", $$1, $$2}'

generate: $(FIXTURE)/fleet.sqlite $(FIXTURE)/readings.parquet $(FIXTURE)/device.log ## Generate fixture/*.sqlite,*.parquet,*.log if missing

$(FIXTURE)/fleet.sqlite $(FIXTURE)/readings.parquet $(FIXTURE)/device.log:
	./fixture/generate.sh

run: generate ## Generate fixtures (if needed) and launch db-studio on all three
	./run.sh

## check: sanity-check the generated fixtures are readable by the real
## tools that produced them -- catches a truncated/corrupt file from a
## failed or interrupted generate.sh run before db-studio ever opens it.
check: check-sqlite check-parquet ## Verify fleet.sqlite and readings.parquet are readable

check-sqlite: generate ## Verify fleet.sqlite is readable via sqlite3
	sqlite3 $(FIXTURE)/fleet.sqlite \
		"select 'sites: ' || count(*) from sites union all \
		 select 'devices: ' || count(*) from devices union all \
		 select 'sensors: ' || count(*) from sensors;"

check-parquet: generate ## Verify readings.parquet is readable via duckdb
	duckdb -c "select 'readings: ' || count(*) from read_parquet('$(FIXTURE)/readings.parquet');"

clean: ## Remove the generated fixture files
	rm -f $(FIXTURE)/fleet.sqlite $(FIXTURE)/readings.parquet $(FIXTURE)/device.log
