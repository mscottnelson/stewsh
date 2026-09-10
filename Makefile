# stewsh dev tasks. Bare `make` lists them.
#
# Self-documenting, minimally: a target appears in `make help` when its line
# carries a `## description`, filed under the nearest `#`+`#@ Section` header.
# No include, no generated file, no .dev-kit — every target is a plain rule at
# column 0, which is what zsh's make completion and the fzf-tab preview read.

.DEFAULT_GOAL := help

CARGO ?= cargo
PORT  ?= 7777
# The dev loop keeps its own database, so a half-written migration cannot reach
# the real queue. `stewsh agents` and `sync` refill it from the same sources.
# For real data: make dev DB=~/.config/stewsh/stewsh.db
DEV_DB := .dev/stewsh.db
DB    ?= $(DEV_DB)

##@ Development

dev: ## Serve the web view, rebuilding and restarting on every change
	@CARGO='$(CARGO)' STEWSH_DB='$(DB)' scripts/dev.sh serve --port $(PORT) --no-open

watch: ## Type-check on every change: no link, fastest feedback
	@CARGO='$(CARGO)' scripts/dev.sh check

run: ## Build and serve once, against the dev database
	@STEWSH_DB='$(DB)' $(CARGO) run -- serve --port $(PORT)

##@ Build

build: ## Debug build
	$(CARGO) build --locked

release: ## Optimized build
	$(CARGO) build --release --locked

install: ## Install stewsh into ~/.cargo/bin from this checkout
	$(CARGO) install --path . --locked

##@ Checks

test: ## Run the test suite
	$(CARGO) test --locked

check: ## Type-check everything, tests included
	$(CARGO) check --locked --all-targets

fmt: ## Format
	$(CARGO) fmt

lint: ## Clippy, with warnings as errors
	$(CARGO) clippy --locked --all-targets -- -D warnings

# Two deviations from the workflow file, both so this is runnable before a
# commit. `cargo package` gets its own target directory, because sharing the
# default one leaves the fingerprint describing the packaged sources, after
# which every later build reports Fresh and keeps a stale binary. It also gets
# --allow-dirty, which drops cargo's own clean-tree check -- so the guard below
# restores the half that matters: an untracked file under a packaged path is
# absent from CI's checkout and fails there, while uncommitted edits to tracked
# files are packaged the same either way.
ci: ## Every check CI runs, in CI's order
	$(CARGO) fmt --check
	$(CARGO) clippy --locked --all-targets -- -D warnings
	$(CARGO) test --locked
	@new=$$(git status --porcelain --untracked-files=all -- src Cargo.toml Cargo.lock | grep '^??' || true); \
	  [ -z "$$new" ] || { printf 'make ci: untracked file under a packaged path:\n%s\n' "$$new"; \
	  echo 'git add it, or this packages a tree CI will not have.'; exit 1; }
	CARGO_TARGET_DIR=target/package-check $(CARGO) package --locked --allow-dirty

##@ Housekeeping

# Deliberately $(DEV_DB) and not $(DB): `make clean DB=~/.config/stewsh/stewsh.db`
# must not delete the real queue.
clean: ## Remove build artifacts and the dev database
	$(CARGO) clean
	rm -f $(DEV_DB) $(DEV_DB)-wal $(DEV_DB)-shm

help: ## Show this help
	@awk 'BEGIN { FS = ":.*##"; printf "\nUsage:\n  make \033[36m<target>\033[0m\n" } \
	      /^##@/ { printf "\n\033[1m%s\033[0m\n", substr($$0, 5); next } \
	      /^[a-zA-Z0-9_-]+:.*##/ { sub(/^ +/, "", $$2); \
	        printf "  \033[36m%-9s\033[0m %s\n", $$1, $$2 }' $(MAKEFILE_LIST)
	@echo

.PHONY: dev watch run build release install test check fmt lint ci clean help
