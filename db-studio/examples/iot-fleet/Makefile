.PHONY: generate run check check-sqlite check-parquet clean

FIXTURE := fixture

generate: $(FIXTURE)/fleet.sqlite $(FIXTURE)/readings.parquet $(FIXTURE)/device.log

$(FIXTURE)/fleet.sqlite $(FIXTURE)/readings.parquet $(FIXTURE)/device.log:
	./fixture/generate.sh

run: generate
	./run.sh

## check: sanity-check the generated fixtures are readable by the real
## tools that produced them -- catches a truncated/corrupt file from a
## failed or interrupted generate.sh run before db-studio ever opens it.
check: check-sqlite check-parquet

check-sqlite: generate
	sqlite3 $(FIXTURE)/fleet.sqlite \
		"select 'sites: ' || count(*) from sites union all \
		 select 'devices: ' || count(*) from devices union all \
		 select 'sensors: ' || count(*) from sensors;"

check-parquet: generate
	duckdb -c "select 'readings: ' || count(*) from read_parquet('$(FIXTURE)/readings.parquet');"

clean:
	rm -f $(FIXTURE)/fleet.sqlite $(FIXTURE)/readings.parquet $(FIXTURE)/device.log
