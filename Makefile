# Build and test everything, part by part:
#
#   core/        the state crate (core/state), and SQL on it (core/sql)
#   packages/    the pnpm workspace: the Node.js addon, restate-fns, the
#                integrations (tests/ and examples/ are members too)
#
#   make          build everything
#   make test     every test that needs no Restate server (test-core, test-ts, ...)
#   make lint     every lint
#   make help     every target
#
# The Rust crates share one target directory.
export CARGO_TARGET_DIR ?= $(CURDIR)/target

CARGO ?= cargo
# The state, and SQL on it.
CORE := core/state core/sql
ADDON := packages/sql/sql.node
# The Rust crates that rustfmt and clippy check.
RUST_CRATES := $(CORE) packages/sql
# Formats the TypeScript, JavaScript and Markdown of every part, as .oxfmtrc.json says.
OXFMT := node_modules/.bin/oxfmt
# Lints the TypeScript of every part, as .oxlintrc.json says; its type-aware
# rules run tsgolint, from the pnpm workspace too.
OXLINT := OXLINT_TSGOLINT_PATH=$(CURDIR)/node_modules/.bin/tsgolint node_modules/.bin/oxlint
# The tests against a Restate server run RESTATE_VERSION's GitHub release for
# this platform by default (1.8; its protocol is v7: the stores' state reads
# are journaled until v8 ships, see FUTURE.md). Or RESTATE_FROM=image
# takes restate-server out of RESTATE_IMAGE without docker (Linux only), or set
# RESTATE_SERVER to a restate-server binary.
RESTATE_FROM ?= release
RESTATE_VERSION ?= 1.8.0
RESTATE_IMAGE ?= ghcr.io/restatedev/restate:$(RESTATE_VERSION)
ifeq ($(RESTATE_FROM),image)
RESTATE_SERVER ?= $(CARGO_TARGET_DIR)/restate/$(lastword $(subst :, ,$(RESTATE_IMAGE)))/restate-server
else
RESTATE_SERVER ?= $(CARGO_TARGET_DIR)/restate/$(RESTATE_VERSION)/restate-server
endif
export RESTATE_SERVER
RESTATE_ARCH := $(shell uname -m | sed 's/arm64/aarch64/')
ifeq ($(shell uname -s),Darwin)
RESTATE_TARGET := $(RESTATE_ARCH)-apple-darwin
else
RESTATE_TARGET := $(RESTATE_ARCH)-unknown-linux-musl
endif

.PHONY: all help core addon js examples-rust \
	test test-core test-addon test-ts \
	crash-test concurrency-test tools-test sql-bench lint lint-core lint-ts format clean

# --- Build

all: core addon js ## Build everything

help: ## List the targets
	@grep -E '^[a-z-]+:.*## ' $(MAKEFILE_LIST) | awk -F':.*## ' '{printf "  %-20s %s\n", $$1, $$2}'

core: ## The state crate and the SQL crate
	for crate in $(CORE); do (cd $$crate && $(CARGO) build --locked --release) || exit 1; done

addon: $(ADDON) ## The Node.js addon (packages/sql)

$(ADDON): $(wildcard packages/sql/src/*.rs $(addsuffix /src/*.rs,$(CORE)) $(addsuffix /src/*/*.rs,$(CORE))) packages/sql/Cargo.toml $(addsuffix /Cargo.toml,$(CORE))
	cd packages/sql && npm run build

js: $(ADDON) ## The pnpm workspace, installed with the addon
	pnpm install

examples-rust: ## The Rust example: the SQL crate's lookup
	cd core/sql && $(CARGO) build --locked --release --features simulation --example lookup

# --- Test

test: test-core test-addon test-ts ## Every test that needs no Restate server

test-core: ## The state and SQL crates, with the simulation campaigns
	for crate in $(CORE); do (cd $$crate && $(CARGO) test --locked && $(CARGO) test --locked --features simulation) || exit 1; done

test-addon: $(ADDON) ## The Node.js addon
	cd packages/sql && npm test

test-ts: js ## The integrations, their storage and SQL in objects, without a Restate server (T=<filter>, such as T=langgraph)
	cd tests && pnpm exec vitest run $(T)

lint: lint-core lint-ts ## Every lint

lint-core: ## The Rust crates (the core and the addon): rustfmt --check and clippy
	for crate in $(RUST_CRATES); do \
		(cd $$crate && $(CARGO) fmt --check && $(CARGO) clippy --locked --all-targets --all-features -- -D warnings) || exit 1; \
	done

lint-ts: js ## The TypeScript, JavaScript and Markdown: tsc, oxlint (type-aware) and oxfmt --check
	pnpm typecheck
	$(OXLINT) --type-aware packages tests examples
	$(OXFMT) --check

format: js ## Format everything: rustfmt and oxfmt (TypeScript, JavaScript, Markdown)
	for crate in $(RUST_CRATES); do (cd $$crate && $(CARGO) fmt) || exit 1; done
	$(OXFMT)

crash-test: js $(RESTATE_SERVER) ## The integrations and SQL objects through crashes, against a Restate server
	cd tests && node crash.ts

concurrency-test: js $(RESTATE_SERVER) ## Handlers with SQL: concurrent awaits, recovery, ctx.db and its transactions, against a Restate server
	cd tests && node concurrency.ts

sql-bench: js $(RESTATE_SERVER) ## SQL in objects against a Restate server: latency, throughput and stored bytes, for databases up to 100 MB (core/sql/benches/RESTATE_RESULTS.md)
	cd tests && node sql/bench.ts

tools-test: js $(RESTATE_SERVER) ## Restate's own agent tools (call, signal, workflow), against a Restate server
	cd tests && node restate-tools.ts

# Into a temporary file, moved into place once it runs: a failed download or
# copy leaves no binary that make would take as up to date.
$(RESTATE_SERVER):
	mkdir -p $(dir $@)
	rm -f $@.tmp
ifeq ($(RESTATE_FROM),image)
	scripts/restate-server-from-image.py $(RESTATE_IMAGE) $@.tmp
else
	curl -fsSL https://github.com/restatedev/restate/releases/download/v$(RESTATE_VERSION)/restate-server-$(RESTATE_TARGET).tar.xz \
		| tar -xJ -O restate-server-$(RESTATE_TARGET)/restate-server > $@.tmp
endif
	chmod +x $@.tmp
	$@.tmp --version
	mv $@.tmp $@

clean: ## Remove build outputs
	rm -rf $(CARGO_TARGET_DIR) $(ADDON) \
		node_modules packages/*/node_modules tests/node_modules examples/node_modules
