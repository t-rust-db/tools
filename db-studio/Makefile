# db-studio

.PHONY: help build release test check lint fmt clean version fixtures

help: ## Show this help
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | sort | awk 'BEGIN {FS = ":.*?## "}; {printf "\033[36m%-15s\033[0m %s\n", $$1, $$2}'

build: ## Debug build
	cargo build

release: ## Optimised build (../target/release/db-studio)
	cargo build --release

# The app tests open examples/iot-fleet's fixtures, which are gitignored.
fixtures: ## Generate the iot-fleet fixtures if missing (needs duckdb and sqlite3)
	$(MAKE) -C examples/iot-fleet generate

test: fixtures ## Run tests
	cargo test

check: fixtures ## Run fmt, clippy, and tests
	cargo fmt --check
	cargo clippy --all-targets -- -D warnings
	cargo test

lint: ## Run clippy
	cargo clippy --all-targets -- -D warnings

fmt: ## Format code
	cargo fmt

clean: ## Remove build artifacts
	cargo clean

version: ## Print the crate version (Cargo.toml [package].version)
	@grep -m1 '^version' Cargo.toml | sed -E 's/version = "(.*)"/\1/'
