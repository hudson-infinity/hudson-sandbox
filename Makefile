# Development entry points. Everything here runs on macOS except `supervisor`,
# which needs Linux with KVM — see docs/implementation/dev-env.md.

.DEFAULT_GOAL := help
.PHONY: help up down logs fmt lint test check docs clean

help: ## Show this help
	@grep -E '^[a-z-]+:.*?## ' $(MAKEFILE_LIST) | awk 'BEGIN {FS = ":.*?## "}; {printf "  %-8s %s\n", $$1, $$2}'

up: ## Start PostgreSQL and MinIO
	docker compose up -d --wait

down: ## Stop them, keeping data
	docker compose down

logs: ## Follow the dependency logs
	docker compose logs -f

fmt: ## Format
	cargo fmt --all

lint: ## Format check and clippy, as CI runs them
	cargo fmt --all -- --check
	cargo clippy --workspace --all-targets -- -D warnings

test: ## Run the test suite
	cargo test --workspace

check: lint test docs ## Everything CI checks

docs: ## Documentation link and fence checks
	python3 scripts/check_docs.py

clean: ## Remove build output and dependency volumes
	cargo clean
	docker compose down -v
