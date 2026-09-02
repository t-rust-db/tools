# column-rs

.DEFAULT_GOAL := help

.PHONY: help test test-lib test-oracle lint fixtures fixtures-funky bench bench-data version

help: ## Show this help
	@echo ""
	@awk 'BEGIN {FS = ":.*?## "} \
	  /^# === .* ===$$/  { sub(/^# === /, ""); sub(/ ===$$/, ""); printf "\n\033[33m%s\033[0m\n", $$0 } \
	  /^[a-zA-Z0-9_-]+:.*?## / { printf "  \033[36m%-24s\033[0m %s\n", $$1, $$2 }' \
	  $(MAKEFILE_LIST)
	@echo ""

# === Test ===

test: ## Run the full test suite (unit tests + DuckDB oracle integration tests, tests/oracle.rs)
	cargo test

test-lib: ## Just the library unit tests (fastest inner loop)
	cargo test --lib

test-oracle: ## Just the DuckDB oracle tests (tests/oracle.rs) -- skip gracefully if `duckdb` isn't on PATH
	cargo test --test oracle

# === Gates ===

lint: ## Run clippy (deny warnings) and check formatting
	cargo clippy --all-targets -- -D warnings
	cargo fmt -- --check

# === Fixtures ===

fixtures: ## Regenerate the DuckDB oracle fixtures (tests/fixtures/) -- requires duckdb on PATH (pinned version: see tests/fixtures/generate.sh)
	./tests/fixtures/generate.sh

fixtures-funky: ## Regenerate funky.parquet, a themed wide-type-mix smoke-test fixture -- requires duckdb on PATH
	./tests/fixtures/generate_funky.sh

# === Benchmarks ===

bench-data: ## Generate the DuckDB parity benchmark datasets (benches/data/) -- requires duckdb on PATH
	./benches/data/generate.sh

bench: ## Run the DuckDB parity benchmark suite (#100) -- requires duckdb + hyperfine; SIZE=small|medium|large
	./benches/run.sh $(or $(SIZE),medium)


# === Release ===

version: ## Print the crate's current version (Cargo.toml [package].version)
	@sed -n 's/^version *= *"\([^"]*\)".*/\1/p' Cargo.toml | head -1
