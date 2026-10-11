# Build and test everything: the core crate, the Restate Rust integration, the
# Node.js addon, the patched TypeScript SDK, and the examples.
#
#   make              build everything
#   make test         unit and integration tests (no Restate server needed)
#   make crash-test   the crash campaigns, against RESTATE_SERVER
#   make help         every target
#
# The Rust workspaces share one target directory.
export CARGO_TARGET_DIR ?= $(CURDIR)/target

CARGO ?= cargo
SDK_TARBALL := vendor/typescript/.build/restatedev-restate-sdk-1.17.3.tgz
ADDON := node/sqlite_kv.node

.PHONY: all help core restate addon sdk-ts examples examples-rust examples-node \
	test test-core test-restate test-addon test-sdk-ts lint crash-test crash-test-rust \
	crash-test-node clean

all: core restate addon examples ## Build everything

help: ## List the targets
	@grep -E '^[a-z-]+:.*## ' $(MAKEFILE_LIST) | awk -F':.*## ' '{printf "  %-18s %s\n", $$1, $$2}'

# --- Build

core: ## The sqlite-kv crate
	$(CARGO) build --locked --release

restate: ## The Restate Rust integration (with the vendored, patched SDK)
	$(CARGO) build --locked --release --manifest-path restate/Cargo.toml

addon: $(ADDON) ## The Node.js addon

$(ADDON): $(wildcard node/src/*.rs src/*.rs src/*/*.rs) node/Cargo.toml Cargo.toml
	cd node && npm run build

sdk-ts: $(SDK_TARBALL) ## The patched TypeScript SDK

$(SDK_TARBALL): $(wildcard vendor/typescript/patches/*.patch) vendor/typescript/setup.sh vendor/typescript/inline-wasm.mjs
	vendor/typescript/setup.sh

examples: examples-rust examples-node ## Every example

examples-rust: ## The Rust examples: the ledger, and the core crate's lookup
	$(CARGO) build --locked --release --manifest-path examples/rust/ledger/Cargo.toml
	$(CARGO) build --locked --release --features simulation --example lookup

examples-node: $(ADDON) $(SDK_TARBALL) ## The Node.js examples, installed with the patched SDK
	cd examples/node && rm -rf node_modules/@restatedev/restate-sdk package-lock.json && npm install

# --- Test

test: test-core test-restate test-addon test-sdk-ts ## Every test that needs no Restate server

test-core: ## The core crate, with the simulation campaigns
	$(CARGO) test --locked
	$(CARGO) test --locked --features simulation

test-restate: ## The Restate Rust integration
	$(CARGO) test --locked --manifest-path restate/Cargo.toml

test-addon: $(ADDON) ## The Node.js addon
	cd node && npm test

test-sdk-ts: $(SDK_TARBALL) ## The patched TypeScript SDK's own tests
	cd vendor/typescript/.build/sdk-typescript/packages/libs/restate-sdk && npx vitest run

lint: ## Clippy on the core crate
	$(CARGO) clippy --locked --all-targets --all-features -- -D warnings

crash-test: crash-test-rust crash-test-node ## The crash campaigns (needs RESTATE_SERVER)

crash-test-rust: examples-node ## The Rust ledger, then the Node.js ledger, through crashes
	@test -n "$(RESTATE_SERVER)" || (echo "set RESTATE_SERVER to a restate-server binary" && exit 1)
	$(CARGO) test --locked --release --manifest-path examples/rust/ledger/Cargo.toml --test crash -- --nocapture
	LEDGER_COMMAND="node $(CURDIR)/examples/node/ledger.mjs" \
		$(CARGO) test --locked --release --manifest-path examples/rust/ledger/Cargo.toml --test crash -- --nocapture

crash-test-node: examples-node ## Every Node.js example through crashes
	@test -n "$(RESTATE_SERVER)" || (echo "set RESTATE_SERVER to a restate-server binary" && exit 1)
	cd examples/node && node test.mjs

clean: ## Remove build outputs
	rm -rf $(CARGO_TARGET_DIR) $(ADDON) vendor/typescript/.build examples/node/node_modules
