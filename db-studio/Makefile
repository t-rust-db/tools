# db-studio

.PHONY: help build release test check lint fmt clean version

help: ## Show this help
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | sort | awk 'BEGIN {FS = ":.*?## "}; {printf "\033[36m%-15s\033[0m %s\n", $$1, $$2}'

build: ## Debug build
	cargo build

release: ## Optimised build (target/release/db-studio)
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

clean: ## Remove build artifacts
	cargo clean

version: ## Print the crate version (Cargo.toml [package].version)
	@grep -m1 '^version' Cargo.toml | sed -E 's/version = "(.*)"/\1/'
