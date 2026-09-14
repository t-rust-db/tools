# loglume Makefile

.PHONY: help build test check lint fmt clean run start version smoke

help: ## Show this help
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | sort | awk 'BEGIN {FS = ":.*?## "}; {printf "\033[36m%-15s\033[0m %s\n", $$1, $$2}'

build: ## Build release binary
	cargo build --release

test: ## Run tests
	cargo test

check: ## Run fmt, clippy, and tests
	cargo fmt --check
	cargo clippy -- -D warnings
	cargo test

lint: ## Run clippy
	cargo clippy -- -D warnings

fmt: ## Format code
	cargo fmt

clean: ## Clean build artifacts
	cargo clean

run: ## Run with sample: make run FILTER="severity >= WARN"
	cargo run -- "$(FILTER)" tests/logs/sample.log

start: ## Start interactive log viewer (alias for run with sample)
	cargo run -- "severity >= INFO" tests/logs/sample.log

gen-logs: ## Generate test logs: make gen-logs N=1000
	python3 tests/logs/gen_syslog.py --seed 42 -n $(or $(N),1000) -o tests/logs/sample.log
	python3 tests/logs/gen_syslog.py --seed 99 -n 200 -o tests/logs/sample2.log
	python3 tests/logs/gen_access.py --seed 42 -n 200 -o tests/logs/access.log
	python3 tests/logs/gen_jsonl.py --seed 42 -n 200 -o tests/logs/sample.jsonl
	python3 tests/logs/gen_jsonl.py --seed 42 -n 50 --docker -o tests/logs/docker.jsonl
	python3 tests/logs/gen_jsonl.py --seed 42 -n 100 --sparse -o tests/logs/sparse.jsonl
	python3 tests/logs/gen_logfmt.py --seed 42 -n 200 -o tests/logs/sample.logfmt

version: ## Show version
	@cargo run -- --version

smoke: ## Build and run --help/--version (cheapest CLI survival check)
	cargo build --bin loglume
	@./target/debug/loglume --help >/dev/null
	@./target/debug/loglume --version
