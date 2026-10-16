# tools workspace: each tool keeps its own Makefile; these targets run them all.

MEMBERS := db-cli column-rs column-ingest sqlite-rs loglume trigrep db-studio

.DEFAULT_GOAL := help

.PHONY: help build test lint

help: ## Show this help
	@awk 'BEGIN {FS = ":.*?## "} /^[a-zA-Z0-9_-]+:.*?## / { printf "  \033[36m%-12s\033[0m %s\n", $$1, $$2 }' $(MAKEFILE_LIST)
	@echo "  Per tool: make -C <tool> help   ($(MEMBERS))"

build: ## Build every tool
	cargo build --workspace

test: ## Run each tool's `make test`
	@set -e; for m in $(MEMBERS); do echo "== $$m"; $(MAKE) -C $$m test; done

lint: ## Run each tool's `make lint`
	@set -e; for m in $(MEMBERS); do echo "== $$m"; $(MAKE) -C $$m lint; done
