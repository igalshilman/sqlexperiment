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
ADDON := node/sqlite_kv.node

.PHONY: all help core addon sdk-ts examples examples-rust examples-js \
	test test-core test-addon test-sdk-ts test-pi lint crash-test concurrency-test clean

all: core addon examples ## Build everything

help: ## List the targets
	@grep -E '^[a-z-]+:.*## ' $(MAKEFILE_LIST) | awk -F':.*## ' '{printf "  %-18s %s\n", $$1, $$2}'

# --- Build

core: ## The sqlite-kv crate
	$(CARGO) build --locked --release

addon: $(ADDON) ## The Node.js addon

$(ADDON): $(wildcard node/src/*.rs src/*.rs src/*/*.rs) node/Cargo.toml Cargo.toml
	cd node && npm run build

sdk-ts: $(SDK_TARBALL) ## The patched TypeScript SDK

$(SDK_TARBALL): $(wildcard vendor/typescript/patches/*.patch) vendor/typescript/setup.sh vendor/typescript/inline-wasm.mjs
	vendor/typescript/setup.sh

examples: examples-rust examples-js ## Every example

examples-rust: ## The Rust example: the core crate's lookup
	$(CARGO) build --locked --release --features simulation --example lookup

examples-js: $(ADDON) $(SDK_TARBALL) ## The JavaScript examples (node/, pi/), installed with the patched SDK
	cd examples && rm -rf node_modules/@restatedev/restate-sdk package-lock.json && npm install

# --- Test

test: test-core test-addon test-sdk-ts test-pi ## Every test that needs no Restate server

test-core: ## The core crate, with the simulation campaigns
	$(CARGO) test --locked
	$(CARGO) test --locked --features simulation

test-addon: $(ADDON) ## The Node.js addon
	cd node && npm test

test-sdk-ts: $(SDK_TARBALL) ## The patched TypeScript SDK's own tests, and its type tests
	cd vendor/typescript/.build/sdk-typescript/packages/libs/restate-sdk && npx tsc -p test/tsconfig.types.json && npx vitest run

test-pi: examples-js ## pi-durable's storage conformance suite on the pi example's storage
	cd examples && npm run test:pi

lint: ## Clippy on the core crate
	$(CARGO) clippy --locked --all-targets --all-features -- -D warnings

crash-test: examples-js ## Every JavaScript example through crashes (needs RESTATE_SERVER)
	@test -n "$(RESTATE_SERVER)" || (echo "set RESTATE_SERVER to a restate-server binary" && exit 1)
	cd examples && node test.mjs

concurrency-test: examples-js ## The context without journal replay: plain promises, one await point (needs RESTATE_SERVER)
	@test -n "$(RESTATE_SERVER)" || (echo "set RESTATE_SERVER to a restate-server binary" && exit 1)
	cd examples && node concurrency.mjs

clean: ## Remove build outputs
	rm -rf $(CARGO_TARGET_DIR) $(ADDON) vendor/typescript/.build examples/node_modules
