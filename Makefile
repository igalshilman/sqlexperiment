# Build and test everything, part by part:
#
#   core/        the state crate (core/state), and SQL on it (core/sql)
#   typescript/  the pnpm workspace: the Node.js addon, restate-fns, the
#                integrations, their tests and the examples
#   python/      the uv workspace: the Python extension, restate-fns, the
#                integrations and their tests (examples-python/ beside it)
#   rust/        the patched Rust SDK, and durable functions on it
#                (rust/functions: functions, functions-python, test-functions)
#
#   make          build everything
#   make test     every test that needs no Restate server (test-ts, test-py, ...)
#   make lint     every lint
#   make help     every target
#
# The Rust crates share one target directory.
export CARGO_TARGET_DIR ?= $(CURDIR)/target

CARGO ?= cargo
# The state, and SQL on it.
CORE := core/state core/sql
FUNCTIONS := rust/functions
# CPython for WASI, with its standard library: the engine of durable Python.
PY_ENGINE := $(CARGO_TARGET_DIR)/pywasm/out/python.wasm
ADDON := typescript/packages/sql/sql.node
SDK_RS := rust/sdk/.build/sdk-rust/.patched
# The Rust crates that rustfmt and clippy check (rust/functions has lint-functions).
RUST_CRATES := $(CORE) typescript/packages/sql python/sql
# The durable functions' runtime tests, and the builds they run in: write
# barriers checked after every turn (verify-dirty), and full scans.
FUNCTIONS_TESTS := --lib --test runs --test crashes --test sql --test handlers --test python --test fetch --test llm --test files --test transactions
FUNCTIONS_MODES := verify-dirty full-scan
# Formats the TypeScript, JavaScript and Markdown of every part, as .oxfmtrc.json says.
OXFMT := typescript/node_modules/.bin/oxfmt
# Lints the TypeScript of every part, as .oxlintrc.json says; its type-aware
# rules run tsgolint, from the pnpm workspace too.
OXLINT := OXLINT_TSGOLINT_PATH=$(CURDIR)/typescript/node_modules/.bin/tsgolint typescript/node_modules/.bin/oxlint
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

.PHONY: all help core functions functions-python python-engine addon sdk-rs js py wheels examples-rust \
	test test-core test-addon test-ts test-py test-sdk-rs test-functions \
	crash-test concurrency-test tools-test sql-bench crash-test-py concurrency-test-py lint lint-core lint-ts lint-py lint-functions format clean

# --- Build

all: core addon js py ## Build everything

help: ## List the targets
	@grep -E '^[a-z-]+:.*## ' $(MAKEFILE_LIST) | awk -F':.*## ' '{printf "  %-20s %s\n", $$1, $$2}'

core: ## The state crate and the SQL crate
	for crate in $(CORE); do (cd $$crate && $(CARGO) build --locked --release) || exit 1; done

functions: $(SDK_RS) ## Durable JavaScript in Restate objects (rust/functions): a binary serving the durable functions of examples-functions/ (or FUNCTIONS_PROGRAMS)
	cd $(FUNCTIONS) && $(CARGO) build --locked --release

functions-python: $(SDK_RS) $(PY_ENGINE) ## Durable Python in Restate objects: the same binary, serving the durable functions of examples-functions-python/
	cd $(FUNCTIONS) && FUNCTIONS_PROGRAMS=$(CURDIR)/examples-functions-python $(CARGO) build --locked --release

python-engine: $(PY_ENGINE) ## CPython for WASI, as durable Python's engine (target/pywasm/out): a few minutes the first time

$(PY_ENGINE): $(FUNCTIONS)/python/build.sh $(FUNCTIONS)/python/reactor.c
	$(FUNCTIONS)/python/build.sh

addon: $(ADDON) ## The Node.js addon (typescript/packages/sql)

$(ADDON): $(wildcard typescript/packages/sql/src/*.rs $(addsuffix /src/*.rs,$(CORE)) $(addsuffix /src/*/*.rs,$(CORE))) typescript/packages/sql/Cargo.toml $(addsuffix /Cargo.toml,$(CORE))
	cd typescript/packages/sql && npm run build

js: $(ADDON) ## The pnpm workspace, installed with the addon
	cd typescript && pnpm install

sdk-rs: $(SDK_RS) ## The patched Rust SDK's sources (rust/sdk)

$(SDK_RS): $(wildcard rust/sdk/patches/*.patch) rust/sdk/setup.sh
	rust/sdk/setup.sh

py: ## The uv workspace (python/.venv): the Python extension, rebuilt when its Rust changes, restate-fns and the integrations
	cd python && uv sync --locked

# Built with zig (maturin[zig]), for glibc 2.28 and later: the wheels run on
# the usual Linux images (Debian, Ubuntu), whatever glibc builds them.
MATURIN_WHEEL := uv tool run --from 'maturin[zig]>=1.9,<2' maturin build --release --zig --compatibility manylinux_2_28 \
	-i python3.13 -o $(CURDIR)/dist

wheels: ## Python wheels in dist/, for a Linux container (manylinux_2_28, CPython 3.13): the extension, restate-fns and restate-agents
	rm -rf dist
	cd python/sql && $(MATURIN_WHEEL)
	uv build --wheel python/fns -o dist
	uv build --wheel python/agents -o dist

examples-rust: ## The Rust example: the SQL crate's lookup
	cd core/sql && $(CARGO) build --locked --release --features simulation --example lookup

# --- Test

test: test-core test-addon test-ts test-py ## Every test that needs no Restate server

test-core: ## The state and SQL crates, with the simulation campaigns
	for crate in $(CORE); do (cd $$crate && $(CARGO) test --locked && $(CARGO) test --locked --features simulation) || exit 1; done

test-addon: $(ADDON) ## The Node.js addon
	cd typescript/packages/sql && npm test

test-ts: js ## The integrations, their storage and SQL in objects, without a Restate server (T=<filter>, such as T=langgraph)
	cd typescript/tests && pnpm exec vitest run $(T)

test-py: py ## The Python extension, restate-fns and the integrations (the agents on a scripted model), without a Restate server (T=<filter>, such as T=langgraph)
	cd python && uv run --locked pytest sql/tests fns/tests agents/tests $(if $(T),-k $(T))

test-sdk-rs: $(SDK_RS) ## The patched Rust SDK's unit tests
	cd rust/sdk/.build/sdk-rust && $(CARGO) test --lib

lint: lint-core lint-ts lint-py ## Every lint

lint-core: ## The Rust crates (the core, the addon and the Python extension): rustfmt --check and clippy
	for crate in $(RUST_CRATES); do \
		(cd $$crate && $(CARGO) fmt --check && $(CARGO) clippy --locked --all-targets --all-features -- -D warnings) || exit 1; \
	done

lint-functions: $(SDK_RS) ## Durable JavaScript and Python in Restate objects: rustfmt --check and clippy, in both capture modes
	cd $(FUNCTIONS) && $(CARGO) fmt --check
	for mode in $(FUNCTIONS_MODES); do \
		(cd $(FUNCTIONS) && $(CARGO) clippy --locked --release --features $$mode --all-targets -- -D warnings) || exit 1; \
	done

lint-ts: js ## The TypeScript, JavaScript and Markdown: tsc, oxlint (type-aware) and oxfmt --check
	cd typescript && pnpm typecheck
	$(OXLINT) --type-aware typescript examples-typescript
	$(OXFMT) --check

lint-py: py ## The Python (python/, examples-python/, the durable functions in Python and their bridge): ruff format --check and ruff check
	uv run --project python --locked ruff format --check python examples-python examples-functions-python rust/functions/assets
	uv run --project python --locked ruff check python examples-python examples-functions-python rust/functions/assets

format: js py ## Format everything: rustfmt, oxfmt (TypeScript, JavaScript, Markdown) and ruff (Python)
	for crate in $(RUST_CRATES) $(FUNCTIONS); do (cd $$crate && $(CARGO) fmt) || exit 1; done
	$(OXFMT)
	uv run --project python --locked ruff format python examples-python examples-functions-python rust/functions/assets

crash-test: js $(RESTATE_SERVER) ## The integrations and SQL objects through crashes, against a Restate server
	cd typescript/tests && node crash.ts

concurrency-test: js $(RESTATE_SERVER) ## Handlers with SQL: concurrent awaits, recovery, ctx.db and its transactions, against a Restate server
	cd typescript/tests && node concurrency.ts

concurrency-test-py: py $(RESTATE_SERVER) ## Python: handlers without journal replay, agents, Restate's tools and the examples (examples-python/), against a Restate server
	cd python && uv run --locked pytest tests

crash-test-py: py $(RESTATE_SERVER) ## Python: a bank and an agent through crashes, against a Restate server
	cd python && uv run --locked python tests/crash.py

sql-bench: js $(RESTATE_SERVER) ## SQL in objects against a Restate server: latency, throughput and stored bytes, for databases up to 100 MB (core/sql/benches/RESTATE_RESULTS.md)
	cd typescript/tests && node sql/bench.ts

tools-test: js $(RESTATE_SERVER) ## Restate's own agent tools (call, signal, workflow), against a Restate server
	cd typescript/tests && node restate-tools.ts

# Release builds: the guest interpreter is slow unoptimized. With verify-dirty,
# native page tracking and write barriers (FUNCTIONS_TRACKING=barriers); then
# full scans. The programs against a Restate server run one at a time, on the
# default ports.
test-functions: $(SDK_RS) $(PY_ENGINE) $(RESTATE_SERVER) ## Durable JavaScript and Python in Restate objects: the runtime in every page-tracking mode, then the programs against a Restate server
	cd $(FUNCTIONS) && $(CARGO) test --locked --release --features verify-dirty $(FUNCTIONS_TESTS)
	cd $(FUNCTIONS) && FUNCTIONS_TRACKING=barriers $(CARGO) test --locked --release --features verify-dirty $(FUNCTIONS_TESTS)
	cd $(FUNCTIONS) && $(CARGO) test --locked --release --features full-scan $(FUNCTIONS_TESTS)
	cd $(FUNCTIONS) && $(CARGO) test --locked --release --test restate -- --include-ignored --test-threads 1

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
	rm -rf $(CARGO_TARGET_DIR) dist $(ADDON) \
		typescript/node_modules typescript/*/node_modules typescript/packages/*/node_modules examples-typescript/node_modules \
		python/.venv python/sql/python/restate_sql/*.so
