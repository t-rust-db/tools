# column-ingest Makefile

.PHONY: help build test check lint fmt clean version smoke

help: ## Show this help
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | sort | awk 'BEGIN {FS = ":.*?## "}; {printf "\033[36m%-15s\033[0m %s\n", $$1, $$2}'

build: ## Build release binary
	cargo build --release

test: ## Run tests
	cargo test

check: ## Run fmt, clippy, and tests
	cargo fmt --check
	cargo clippy --all-targets -- -D warnings
	cargo test

lint: ## Run clippy
	cargo clippy --all-targets -- -D warnings

fmt: ## Format code
	cargo fmt

clean: ## Clean build artifacts
	cargo clean

version: ## Show version
	@cargo run -- --version

smoke: ## Build and run --help/--version (cheapest CLI survival check)
	cargo build --bin column-ingest
	@../target/debug/column-ingest --help >/dev/null
	@../target/debug/column-ingest --version
