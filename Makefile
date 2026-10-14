# Build and test everything: the core crate, the Node.js addon, the patched
# TypeScript SDK, and the examples.
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
SDK_TARBALL := vendor/typescript/.build/restatedev-restate-sdk-1.17.3.tgz
CORE := sqlite-kv
ADDON := $(CORE)/node/sqlite_kv.node
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

.PHONY: all help core addon sdk-ts examples examples-rust examples-js \
	test test-core test-addon test-sdk-ts test-node test-pi test-langgraph test-claude test-deepseek test-opencode test-gateway lint lint-core lint-examples format crash-test concurrency-test restate-server clean

all: core addon examples ## Build everything

help: ## List the targets
	@grep -E '^[a-z-]+:.*## ' $(MAKEFILE_LIST) | awk -F':.*## ' '{printf "  %-18s %s\n", $$1, $$2}'

# --- Build

core: ## The sqlite-kv crate
	cd $(CORE) && $(CARGO) build --locked --release

addon: $(ADDON) ## The Node.js addon

$(ADDON): $(wildcard $(CORE)/node/src/*.rs $(CORE)/src/*.rs $(CORE)/src/*/*.rs) $(CORE)/node/Cargo.toml $(CORE)/Cargo.toml
	cd $(CORE)/node && npm run build

sdk-ts: $(SDK_TARBALL) ## The patched TypeScript SDK

$(SDK_TARBALL): $(wildcard vendor/typescript/patches/*.patch) vendor/typescript/setup.sh vendor/typescript/inline-wasm.mjs
	vendor/typescript/setup.sh

examples: examples-rust examples-js ## Every example

examples-rust: ## The Rust example: the core crate's lookup
	cd $(CORE) && $(CARGO) build --locked --release --features simulation --example lookup

examples-js: $(ADDON) $(SDK_TARBALL) ## The TypeScript examples (node/, pi/, langgraph/, claude/, deepseek/, opencode/), installed with the patched SDK
	cd examples && rm -rf node_modules/@restatedev/restate-sdk package-lock.json && npm install

# --- Test

test: test-core test-addon test-sdk-ts test-node test-pi test-langgraph test-claude test-deepseek test-opencode test-gateway ## Every test that needs no Restate server

test-core: ## The core crate, with the simulation campaigns
	cd $(CORE) && $(CARGO) test --locked
	cd $(CORE) && $(CARGO) test --locked --features simulation

test-addon: $(ADDON) ## The Node.js addon
	cd $(CORE)/node && npm test

test-sdk-ts: $(SDK_TARBALL) ## The patched TypeScript SDK's own tests, and its type tests
	cd vendor/typescript/.build/sdk-typescript/packages/libs/restate-sdk && npx tsc -p test/tsconfig.types.json && npx vitest run

test-node: examples-js ## The ledger's handlers and ctx.transaction's sends, without a Restate server
	cd examples && npm run test:node

test-pi: examples-js ## pi-durable's storage conformance suite on the pi example's storage
	cd examples && npm run test:pi

test-langgraph: examples-js ## LangGraph's checkpointer conformance suite on the LangGraph example's checkpointer, and the agent through crashes
	cd examples && npm run test:langgraph

test-claude: examples-js ## The Claude agent (the Anthropic SDK's tool runner) through crashes
	cd examples && npm run test:claude

test-deepseek: examples-js ## The DeepSeek Harness agent through crashes
	cd examples && npm run test:deepseek

test-opencode: examples-js ## The OpenCode 2.0 agent through crashes
	cd examples && npm run test:opencode

test-gateway: examples-js ## The tool gateway's registry and scripted model
	cd examples && npm run test:gateway

lint: lint-core lint-examples ## Every lint

lint-core: ## Clippy on the core crate
	cd $(CORE) && $(CARGO) clippy --locked --all-targets --all-features -- -D warnings

lint-examples: examples-js ## The TypeScript examples: tsc, oxlint (type-aware) and oxfmt --check
	cd examples && npm run check

format: examples-js ## Format the TypeScript examples with oxfmt
	cd examples && npm run format

crash-test: examples-js $(RESTATE_SERVER) ## Every TypeScript example through crashes, against a Restate server
	cd examples && node test.ts

concurrency-test: examples-js $(RESTATE_SERVER) ## The context without journal replay, against a Restate server
	cd examples && node concurrency.ts


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
	rm -rf $(CARGO_TARGET_DIR) $(ADDON) vendor/typescript/.build examples/node_modules
