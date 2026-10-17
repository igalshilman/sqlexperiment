# Build and test everything: the core crate, and the pnpm workspace: the
# patched TypeScript SDK (packages/sdk-typescript, over the patched shared core
# of packages/sdk-shared-core), the Node.js addon (packages/sql-node), the
# integrations (packages/integrations), the tests (tests/) and the examples;
# and, in python/, the uv workspace: the patched Python SDK (python/sdk, over
# the same shared core), the Python extension (python/sqlite-kv), the Python
# agent integrations (python/agents), and their tests; and the Python
# examples (examples-py).
#
#   make              build everything
#   make test         unit and integration tests (no Restate server needed)
#   make crash-test   the crash campaign, against RESTATE_SERVER
#   make concurrency-test   the context without journal replay, against RESTATE_SERVER
#   make help         every target
#
# The Rust workspaces (the crate, the addon, the Python extension) share one
# target directory.
export CARGO_TARGET_DIR ?= $(CURDIR)/target

CARGO ?= cargo
SDK_TARBALL := packages/sdk-typescript/.build/restatedev-restate-sdk-1.17.3.tgz
CORE := sqlite-kv
ADDON := packages/sql-node/sqlite_kv.node
CORE_PATCH := packages/sdk-shared-core/sdk-shared-core.patch
SDK_PY := python/sdk/.build/sdk-python/.patched
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

.PHONY: all help core addon sdk-ts sdk-py js py examples-rust test test-core test-addon test-sdk-ts test-ts \
	test-sdk-py test-sql-py test-agents-py lint lint-core lint-ts lint-py format crash-test concurrency-test tools-test \
	crash-test-py concurrency-test-py restate-server clean

# --- Build

all: core addon js py ## Build everything

help: ## List the targets
	@grep -E '^[a-z-]+:.*## ' $(MAKEFILE_LIST) | awk -F':.*## ' '{printf "  %-18s %s\n", $$1, $$2}'

core: ## The sqlite-kv crate
	cd $(CORE) && $(CARGO) build --locked --release

addon: $(ADDON) ## The Node.js addon (packages/sql-node)

$(ADDON): $(wildcard packages/sql-node/src/*.rs $(CORE)/src/*.rs $(CORE)/src/*/*.rs) packages/sql-node/Cargo.toml $(CORE)/Cargo.toml
	cd packages/sql-node && npm run build

sdk-ts: $(SDK_TARBALL) ## The patched TypeScript SDK (packages/sdk-typescript)

$(SDK_TARBALL): $(wildcard packages/sdk-typescript/patches/*.patch) $(CORE_PATCH) packages/sdk-typescript/setup.sh packages/sdk-typescript/inline-wasm.mjs
	packages/sdk-typescript/setup.sh

js: $(ADDON) $(SDK_TARBALL) ## The pnpm workspace, installed with the patched SDK and the addon
	pnpm install --force

sdk-py: $(SDK_PY) ## The patched Python SDK's sources (python/sdk), for the uv workspace to build

$(SDK_PY): $(wildcard python/sdk/patches/*.patch) $(CORE_PATCH) python/sdk/setup.sh
	python/sdk/setup.sh

py: $(SDK_PY) ## The uv workspace (python/.venv): the patched Python SDK and the Python extension, rebuilt when their Rust changes
	cd python && uv sync --locked

examples-rust: ## The Rust example: the core crate's lookup
	cd $(CORE) && $(CARGO) build --locked --release --features simulation --example lookup

# --- Test

test: test-core test-addon test-sdk-ts test-ts test-sdk-py test-sql-py test-agents-py ## Every test that needs no Restate server

test-core: ## The core crate, with the simulation campaigns
	cd $(CORE) && $(CARGO) test --locked
	cd $(CORE) && $(CARGO) test --locked --features simulation

test-addon: $(ADDON) ## The Node.js addon
	cd packages/sql-node && npm test

test-sdk-ts: $(SDK_TARBALL) ## The patched TypeScript SDK's own tests, and its type tests
	cd packages/sdk-typescript/.build/sdk-typescript/packages/libs/restate-sdk && npx tsc -p test/tsconfig.types.json && npx vitest run

test-ts: js ## The integrations, their storage and SQL in objects, without a Restate server (T=<filter>, such as T=langgraph)
	cd tests && pnpm exec vitest run $(T)

# Its own environment, with the extras its tests use; its tests against a
# Restate server in docker (the test harness) are left out.
test-sdk-py: $(SDK_PY) ## The patched Python SDK's own tests
	cd python/sdk/.build/sdk-python && uv run --extra test --extra serde --extra client --extra sql \
		pytest -o python_files='*.py' tests --ignore=tests/harness.py --ignore=tests/servercontext.py \
		--ignore=tests/ext.py -k 'not end_to_end'

test-sql-py: py ## The Python extension
	cd python && uv run --locked pytest sqlite-kv/tests

test-agents-py: py ## The Python agent integrations (python/agents): the checkpointer conformance suite, and the agents on a scripted model
	cd python && uv run --locked pytest agents/tests

lint: lint-core lint-ts lint-py ## Every lint

lint-core: ## The Rust crates (the core, the addon and the Python extension): rustfmt --check and clippy
	cd $(CORE) && $(CARGO) fmt --check && $(CARGO) clippy --locked --all-targets --all-features -- -D warnings
	cd packages/sql-node && $(CARGO) fmt --check && $(CARGO) clippy --locked --all-targets -- -D warnings
	cd python/sqlite-kv && $(CARGO) fmt --check && $(CARGO) clippy --locked --all-targets -- -D warnings

lint-ts: js ## The TypeScript, JavaScript and Markdown: tsc, oxlint (type-aware) and oxfmt --check
	pnpm check

lint-py: py ## The Python (python/, examples-py/): ruff format --check and ruff check; and the patched SDK's ruff, as it configures it, and rustfmt
	uv run --project python --locked ruff format --check python examples-py
	uv run --project python --locked ruff check python examples-py
	cd python/sdk/.build/sdk-python && uv run --extra lint ruff format --check && uv run --extra lint ruff check \
		&& $(CARGO) fmt --check

format: js py ## Format everything: rustfmt, oxfmt (TypeScript, JavaScript, Markdown) and ruff (Python)
	cd $(CORE) && $(CARGO) fmt
	cd packages/sql-node && $(CARGO) fmt
	cd python/sqlite-kv && $(CARGO) fmt
	pnpm format
	uv run --project python --locked ruff format python examples-py

crash-test: js $(RESTATE_SERVER) ## The integrations and SQL objects through crashes, against a Restate server
	cd tests && node crash.ts

concurrency-test: js $(RESTATE_SERVER) ## The context without journal replay, against a Restate server
	cd tests && node concurrency.ts

concurrency-test-py: py $(RESTATE_SERVER) ## Python: the context without journal replay, an agent and the examples (examples-py), against a Restate server
	cd python && uv run --locked pytest tests

crash-test-py: py $(RESTATE_SERVER) ## Python: the bank example through crashes, against a Restate server
	cd python && uv run --locked python tests/crash.py

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
	rm -rf $(CARGO_TARGET_DIR) $(ADDON) packages/sdk-typescript/.build python/sdk/.build node_modules */node_modules packages/*/node_modules \
		python/.venv python/sqlite-kv/python/sqlite_kv/*.so
