# Build and test everything: the core crate, and the pnpm workspace: the
# patched TypeScript SDK (packages/sdk), the Node.js addon (packages/sql), the
# integrations (packages/integrations), the tests (tests/) and the examples.
#
#   make              build everything
#   make test         unit and integration tests (no Restate server needed)
#   make crash-test   the crash campaign, against RESTATE_SERVER
#   make concurrency-test   the context without journal replay, against RESTATE_SERVER
#   make help         every target
#
# The Rust workspaces (the crate, the addon) share one target directory.
export CARGO_TARGET_DIR ?= $(CURDIR)/target

CARGO ?= cargo
SDK_TARBALL := packages/sdk/.build/restatedev-restate-sdk-1.17.3.tgz
CORE := sqlite-kv
ADDON := packages/sql/sqlite_kv.node
# The tests against a Restate server run a released restate-server: by default
# RESTATE_VERSION's, downloaded from its GitHub release for this platform. Or
# RESTATE_FROM=image takes it out of RESTATE_IMAGE with docker (Linux only), or
# set RESTATE_SERVER to a restate-server binary.
RESTATE_VERSION ?= 1.7.13
RESTATE_FROM ?= release
RESTATE_IMAGE ?= ghcr.io/restatedev/restate:$(RESTATE_VERSION)
RESTATE_SERVER ?= $(CARGO_TARGET_DIR)/restate/restate-server
export RESTATE_SERVER
RESTATE_ARCH := $(shell uname -m | sed 's/arm64/aarch64/')
ifeq ($(shell uname -s),Darwin)
RESTATE_TARGET := $(RESTATE_ARCH)-apple-darwin
else
RESTATE_TARGET := $(RESTATE_ARCH)-unknown-linux-musl
endif

.PHONY: all help core addon sdk-ts js examples-rust test test-core test-addon test-sdk-ts test-ts \
	lint lint-core lint-ts format crash-test concurrency-test tools-test restate-server clean

# --- Build

all: core addon js ## Build everything

help: ## List the targets
	@grep -E '^[a-z-]+:.*## ' $(MAKEFILE_LIST) | awk -F':.*## ' '{printf "  %-18s %s\n", $$1, $$2}'

core: ## The sqlite-kv crate
	cd $(CORE) && $(CARGO) build --locked --release

addon: $(ADDON) ## The Node.js addon (packages/sql)

$(ADDON): $(wildcard packages/sql/src/*.rs $(CORE)/src/*.rs $(CORE)/src/*/*.rs) packages/sql/Cargo.toml $(CORE)/Cargo.toml
	cd packages/sql && npm run build

sdk-ts: $(SDK_TARBALL) ## The patched TypeScript SDK (packages/sdk)

$(SDK_TARBALL): $(wildcard packages/sdk/patches/*.patch) packages/sdk/setup.sh packages/sdk/inline-wasm.mjs
	packages/sdk/setup.sh

js: $(ADDON) $(SDK_TARBALL) ## The pnpm workspace, installed with the patched SDK and the addon
	pnpm install --force

examples-rust: ## The Rust example: the core crate's lookup
	cd $(CORE) && $(CARGO) build --locked --release --features simulation --example lookup

# --- Test

test: test-core test-addon test-sdk-ts test-ts ## Every test that needs no Restate server

test-core: ## The core crate, with the simulation campaigns
	cd $(CORE) && $(CARGO) test --locked
	cd $(CORE) && $(CARGO) test --locked --features simulation

test-addon: $(ADDON) ## The Node.js addon
	cd packages/sql && npm test

test-sdk-ts: $(SDK_TARBALL) ## The patched TypeScript SDK's own tests, and its type tests
	cd packages/sdk/.build/sdk-typescript/packages/libs/restate-sdk && npx tsc -p test/tsconfig.types.json && npx vitest run

test-ts: js ## The integrations, their storage and SQL in objects, without a Restate server (T=<filter>, such as T=langgraph)
	cd tests && pnpm exec vitest run $(T)

lint: lint-core lint-ts ## Every lint

lint-core: ## The Rust crates (the core and the addon): rustfmt --check and clippy
	cd $(CORE) && $(CARGO) fmt --check && $(CARGO) clippy --locked --all-targets --all-features -- -D warnings
	cd packages/sql && $(CARGO) fmt --check && $(CARGO) clippy --locked --all-targets -- -D warnings

lint-ts: js ## The TypeScript, JavaScript and Markdown: tsc, oxlint (type-aware) and oxfmt --check
	pnpm check

format: js ## Format everything: rustfmt, and oxfmt (TypeScript, JavaScript, Markdown)
	cd $(CORE) && $(CARGO) fmt
	cd packages/sql && $(CARGO) fmt
	pnpm format

crash-test: js $(RESTATE_SERVER) ## The integrations and SQL objects through crashes, against a Restate server
	cd tests && node crash.ts

concurrency-test: js $(RESTATE_SERVER) ## The context without journal replay, against a Restate server
	cd tests && node concurrency.ts

tools-test: js $(RESTATE_SERVER) ## Restate's own agent tools (call, signal, workflow), against a Restate server
	cd tests && node restate-tools.ts

restate-server: ## Get restate-server again: RESTATE_VERSION's release, or from RESTATE_IMAGE (docker), per RESTATE_FROM
	rm -f $(RESTATE_SERVER) && $(MAKE) $(RESTATE_SERVER)

# Into a temporary file, moved into place once it runs: a failed download or
# copy leaves no binary that make would take as up to date.
$(RESTATE_SERVER):
	mkdir -p $(dir $@)
	rm -f $@.tmp
ifeq ($(RESTATE_FROM),image)
	docker pull -q $(RESTATE_IMAGE)
	id=$$(docker create $(RESTATE_IMAGE)) && { docker cp -q $$id:/usr/local/bin/restate-server $@.tmp; status=$$?; docker rm $$id > /dev/null; exit $$status; }
else
	curl -fsSL https://github.com/restatedev/restate/releases/download/v$(RESTATE_VERSION)/restate-server-$(RESTATE_TARGET).tar.xz \
		| tar -xJ -O restate-server-$(RESTATE_TARGET)/restate-server > $@.tmp
endif
	chmod +x $@.tmp
	$@.tmp --version
	mv $@.tmp $@

clean: ## Remove build outputs
	rm -rf $(CARGO_TARGET_DIR) $(ADDON) packages/sdk/.build node_modules */node_modules packages/*/node_modules
