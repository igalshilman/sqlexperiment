# Build and test everything: the core crate, the Node.js addon, the patched
# TypeScript SDK, and the examples.
#
#   make              build everything
#   make test         unit and integration tests (no Restate server needed)
#   make crash-test   the crash campaign, against a Restate server
#   make concurrency-test   the context without journal replay, against a Restate server
#   make help         every target
#
# The Rust workspaces (the crate, the addon) share one target directory.
export CARGO_TARGET_DIR ?= $(CURDIR)/target

CARGO ?= cargo
SDK_TARBALL := vendor/typescript/.build/restatedev-restate-sdk-1.17.3.tgz
CORE := sqlite-kv
ADDON := $(CORE)/node/sqlite_kv.node
# The tests against a Restate server need service protocol V8 (state reads that
# are not journaled), which is not on restate's main yet: by default, the
# server of its service-protocol-v8 image, taken out with docker. Or set
# RESTATE_SERVER to a restate-server binary with V8.
RESTATE_IMAGE ?= ghcr.io/restatedev/restate:service-protocol-v8
RESTATE_SERVER ?= $(CARGO_TARGET_DIR)/restate/restate-server
export RESTATE_SERVER

.PHONY: all help core addon sdk-ts examples examples-rust examples-js \
	test test-core test-addon test-sdk-ts test-pi test-langgraph lint crash-test concurrency-test restate-server clean

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

examples-js: $(ADDON) $(SDK_TARBALL) ## The JavaScript examples (node/, pi/, langgraph/), installed with the patched SDK
	cd examples && rm -rf node_modules/@restatedev/restate-sdk package-lock.json && npm install

# --- Test

test: test-core test-addon test-sdk-ts test-pi test-langgraph ## Every test that needs no Restate server

test-core: ## The core crate, with the simulation campaigns
	cd $(CORE) && $(CARGO) test --locked
	cd $(CORE) && $(CARGO) test --locked --features simulation

test-addon: $(ADDON) ## The Node.js addon
	cd $(CORE)/node && npm test

test-sdk-ts: $(SDK_TARBALL) ## The patched TypeScript SDK's own tests, and its type tests
	cd vendor/typescript/.build/sdk-typescript/packages/libs/restate-sdk && npx tsc -p test/tsconfig.types.json && npx vitest run

test-pi: examples-js ## pi-durable's storage conformance suite on the pi example's storage
	cd examples && npm run test:pi

test-langgraph: examples-js ## LangGraph's checkpointer conformance suite on the LangGraph example's checkpointer, and the agent through crashes
	cd examples && npm run test:langgraph

lint: ## Clippy on the core crate
	cd $(CORE) && $(CARGO) clippy --locked --all-targets --all-features -- -D warnings

crash-test: examples-js $(RESTATE_SERVER) ## Every JavaScript example through crashes, against a Restate server
	cd examples && node test.mjs

concurrency-test: examples-js $(RESTATE_SERVER) ## The context without journal replay, against a Restate server
	cd examples && node concurrency.mjs

restate-server: ## Take restate-server out of RESTATE_IMAGE again (needs docker)
	rm -f $(RESTATE_SERVER) && $(MAKE) $(RESTATE_SERVER)

$(RESTATE_SERVER):
	mkdir -p $(dir $@)
	docker pull -q $(RESTATE_IMAGE)
	id=$$(docker create $(RESTATE_IMAGE)) && docker cp -q $$id:/usr/local/bin/restate-server $@; docker rm $$id > /dev/null
	$@ --version

clean: ## Remove build outputs
	rm -rf $(CARGO_TARGET_DIR) $(ADDON) vendor/typescript/.build examples/node_modules
