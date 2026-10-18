# Build and test everything, part by part:
#
#   core/        the sqlite-kv crate, the patch to the SDKs' shared core
#                (core/sdk-shared-core) that both SDKs build on, and the
#                durable QuickJS runtime (core/quickjs-durable; its own
#                targets: qjs, test-qjs, lint-qjs)
#   typescript/  the pnpm workspace: the patched TypeScript SDK, the Node.js
#                addon, the integrations and their tests; with the examples
#                (examples-typescript/, a member of the workspace)
#   python/      the uv workspace: the patched Python SDK, the Python
#                extension, the agent integrations and their tests; and the
#                examples (examples-python/, a project of their own)
#   rust/        the patched Rust SDK (rust/sdk), and durable JavaScript in
#                Restate objects on it (rust/quickjs-restate; its own
#                targets: goldens, qjs-restate, test-qjs-restate,
#                lint-qjs-restate)
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
SDK_TARBALL := typescript/packages/sdk-typescript/.build/restatedev-restate-sdk-1.17.3.tgz
CORE := core/sqlite-kv
QJS := core/quickjs-durable
QJS_RESTATE := rust/quickjs-restate
# The programs the binary serves: each <name>.js, with the golden dump
# <name>.golden that `make goldens` computes beside it.
export QJS_PROGRAMS ?= $(CURDIR)/$(QJS_RESTATE)/programs
GOLDENS := $(patsubst %.js,%.golden,$(wildcard $(QJS_PROGRAMS)/*.js))
ADDON := typescript/packages/sql-node/sqlite_kv.node
CORE_PATCH := core/sdk-shared-core/sdk-shared-core.patch
SDK_PY := python/sdk/.build/sdk-python/.patched
SDK_RS := rust/sdk/.build/sdk-rust/.patched
# Formats the TypeScript, JavaScript and Markdown of every part, as .oxfmtrc.json says.
OXFMT := typescript/node_modules/.bin/oxfmt
# Lints the TypeScript of every part, as .oxlintrc.json says; its type-aware
# rules run tsgolint, from the pnpm workspace too.
OXLINT := OXLINT_TSGOLINT_PATH=$(CURDIR)/typescript/node_modules/.bin/tsgolint typescript/node_modules/.bin/oxlint
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

.PHONY: all help core qjs qjs-restate goldens addon sdk-ts sdk-py sdk-rs js py examples-rust test test-core test-addon test-sdk-ts test-ts \
	test-sdk-py test-sdk-rs test-sql-py test-agents-py test-qjs lint lint-core lint-ts lint-py lint-qjs lint-qjs-restate format crash-test concurrency-test tools-test \
	crash-test-py concurrency-test-py test-qjs-restate restate-server wheels clean

# --- Build

all: core addon js py ## Build everything

help: ## List the targets
	@grep -E '^[a-z-]+:.*## ' $(MAKEFILE_LIST) | awk -F':.*## ' '{printf "  %-18s %s\n", $$1, $$2}'

core: ## The sqlite-kv crate
	cd $(CORE) && $(CARGO) build --locked --release

qjs: ## The durable QuickJS runtime (core/quickjs-durable), with its hosts (SQLite, and sqlite-kv)
	cd $(QJS) && $(CARGO) build --locked --release --features sqlite-host,sqlite-kv-host

addon: $(ADDON) ## The Node.js addon (typescript/packages/sql-node)

$(ADDON): $(wildcard typescript/packages/sql-node/src/*.rs $(CORE)/src/*.rs $(CORE)/src/*/*.rs) typescript/packages/sql-node/Cargo.toml $(CORE)/Cargo.toml
	cd typescript/packages/sql-node && npm run build

sdk-ts: $(SDK_TARBALL) ## The patched TypeScript SDK (typescript/packages/sdk-typescript)

$(SDK_TARBALL): $(wildcard typescript/packages/sdk-typescript/patches/*.patch) $(CORE_PATCH) typescript/packages/sdk-typescript/setup.sh typescript/packages/sdk-typescript/inline-wasm.mjs
	typescript/packages/sdk-typescript/setup.sh

js: $(ADDON) $(SDK_TARBALL) ## The pnpm workspace, installed with the patched SDK and the addon
	cd typescript && pnpm install --force

sdk-py: $(SDK_PY) ## The patched Python SDK's sources (python/sdk), for the uv workspace to build

$(SDK_PY): $(wildcard python/sdk/patches/*.patch) $(CORE_PATCH) python/sdk/setup.sh
	python/sdk/setup.sh

sdk-rs: $(SDK_RS) ## The patched Rust SDK's sources (rust/sdk)

qjs-restate: $(SDK_RS) goldens ## Durable JavaScript in Restate objects (rust/quickjs-restate): a binary serving QJS_PROGRAMS
	cd $(QJS_RESTATE) && $(CARGO) build --locked --release

goldens: $(GOLDENS) ## The programs' golden dumps (QJS_PROGRAMS/<name>.golden), computed again when their source or the runtime changes

# A dump is valid for one source and one build of the runtime: its Wasm, its
# bridge and its limits.
$(QJS_PROGRAMS)/%.golden: $(QJS_PROGRAMS)/%.js $(QJS)/assets/quickjs.wasm $(QJS)/assets/guest.js $(QJS)/src/vm.rs
	cd $(QJS) && $(CARGO) run --locked --release --quiet --bin golden -- $<

$(SDK_RS): $(wildcard rust/sdk/patches/*.patch) $(CORE_PATCH) rust/sdk/setup.sh
	rust/sdk/setup.sh

py: $(SDK_PY) ## The uv workspace (python/.venv): the patched Python SDK and the Python extension, rebuilt when their Rust changes
	cd python && uv sync --locked

# Built with zig (maturin[zig]), for glibc 2.28 and later: the wheels run on
# the usual Linux images (Debian, Ubuntu), whatever glibc builds them.
MATURIN_WHEEL := uv tool run --from 'maturin[zig]>=1.9,<2' maturin build --release --zig --compatibility manylinux_2_28 \
	-i python3.13 -o $(CURDIR)/dist

wheels: $(SDK_PY) ## Python wheels in dist/, for a Linux container (manylinux_2_28, CPython 3.13): the patched SDK, the extension and restate-agents
	rm -rf dist
	cd python/sdk/.build/sdk-python && $(MATURIN_WHEEL)
	cd python/sqlite-kv && $(MATURIN_WHEEL)
	uv build --wheel python/agents -o dist

examples-rust: ## The Rust example: the core crate's lookup
	cd $(CORE) && $(CARGO) build --locked --release --features simulation --example lookup

# --- Test

test: test-core test-addon test-sdk-ts test-ts test-sdk-py test-sql-py test-agents-py ## Every test that needs no Restate server

test-core: ## The core crate, with the simulation campaigns
	cd $(CORE) && $(CARGO) test --locked
	cd $(CORE) && $(CARGO) test --locked --features simulation

# Release builds: the guest interpreter is slow unoptimized. verify-dirty
# checks after every turn that the write barriers marked each changed page;
# full-scan is the uninstrumented build, which compares every page.
test-qjs: ## The durable QuickJS runtime: with dirty-page tracking (verified), then with full scans
	cd $(QJS) && $(CARGO) test --locked --release --features sqlite-host,sqlite-kv-host,verify-dirty --lib --tests
	cd $(QJS) && $(CARGO) test --locked --release --features sqlite-host,sqlite-kv-host,full-scan --lib --test recovery --test host --test sqlite --test namespace

test-addon: $(ADDON) ## The Node.js addon
	cd typescript/packages/sql-node && npm test

test-sdk-ts: $(SDK_TARBALL) ## The patched TypeScript SDK's own tests, and its type tests
	cd typescript/packages/sdk-typescript/.build/sdk-typescript/packages/libs/restate-sdk && npx tsc -p test/tsconfig.types.json && npx vitest run

test-ts: js ## The integrations, their storage and SQL in objects, without a Restate server (T=<filter>, such as T=langgraph)
	cd typescript/tests && pnpm exec vitest run $(T)

# Its own environment, with the extras its tests use; its tests against a
# Restate server in docker (the test harness) are left out.
test-sdk-py: $(SDK_PY) ## The patched Python SDK's own tests
	cd python/sdk/.build/sdk-python && uv run --extra test --extra serde --extra client --extra sql \
		pytest -o python_files='*.py' tests --ignore=tests/harness.py --ignore=tests/servercontext.py \
		--ignore=tests/ext.py -k 'not end_to_end'

test-sdk-rs: $(SDK_RS) ## The patched Rust SDK's unit tests
	cd rust/sdk/.build/sdk-rust && $(CARGO) test --lib

test-sql-py: py ## The Python extension
	cd python && uv run --locked pytest sqlite-kv/tests

test-agents-py: py ## The Python agent integrations (python/agents): the checkpointer conformance suite, and the agents on a scripted model
	cd python && uv run --locked pytest agents/tests

lint: lint-core lint-ts lint-py ## Every lint

lint-core: ## The Rust crates (the core, the addon and the Python extension): rustfmt --check and clippy
	cd $(CORE) && $(CARGO) fmt --check && $(CARGO) clippy --locked --all-targets --all-features -- -D warnings
	cd typescript/packages/sql-node && $(CARGO) fmt --check && $(CARGO) clippy --locked --all-targets -- -D warnings
	cd python/sqlite-kv && $(CARGO) fmt --check && $(CARGO) clippy --locked --all-targets -- -D warnings

lint-qjs: ## The durable QuickJS runtime: rustfmt --check and clippy, in both capture modes
	cd $(QJS) && $(CARGO) fmt --check
	cd $(QJS) && $(CARGO) clippy --locked --release --features sqlite-host,sqlite-kv-host,verify-dirty --all-targets -- -D warnings
	cd $(QJS) && $(CARGO) clippy --locked --release --features sqlite-host,sqlite-kv-host,full-scan --all-targets -- -D warnings

lint-qjs-restate: $(SDK_RS) goldens ## Durable JavaScript in Restate objects: rustfmt --check and clippy
	cd $(QJS_RESTATE) && $(CARGO) fmt --check && $(CARGO) clippy --locked --release --all-targets -- -D warnings

lint-ts: js ## The TypeScript, JavaScript and Markdown: tsc, oxlint (type-aware) and oxfmt --check
	cd typescript && pnpm typecheck
	$(OXLINT) --type-aware typescript examples-typescript
	$(OXFMT) --check

lint-py: py ## The Python (python/, examples-python/): ruff format --check and ruff check; and the patched SDK's ruff, as it configures it, and rustfmt
	uv run --project python --locked ruff format --check python examples-python
	uv run --project python --locked ruff check python examples-python
	cd python/sdk/.build/sdk-python && uv run --extra lint ruff format --check && uv run --extra lint ruff check \
		&& $(CARGO) fmt --check

format: js py ## Format everything: rustfmt, oxfmt (TypeScript, JavaScript, Markdown) and ruff (Python)
	cd $(CORE) && $(CARGO) fmt
	cd $(QJS) && $(CARGO) fmt
	cd $(QJS_RESTATE) && $(CARGO) fmt
	cd typescript/packages/sql-node && $(CARGO) fmt
	cd python/sqlite-kv && $(CARGO) fmt
	$(OXFMT)
	uv run --project python --locked ruff format python examples-python

crash-test: js $(RESTATE_SERVER) ## The integrations and SQL objects through crashes, against a Restate server
	cd typescript/tests && node crash.ts

concurrency-test: js $(RESTATE_SERVER) ## The context without journal replay, against a Restate server
	cd typescript/tests && node concurrency.ts

concurrency-test-py: py $(RESTATE_SERVER) ## Python: the context without journal replay, agents, Restate's tools and the examples (examples-python/), against a Restate server
	cd python && uv run --locked pytest tests

crash-test-py: py $(RESTATE_SERVER) ## Python: a bank and an agent through crashes, against a Restate server
	cd python && uv run --locked python tests/crash.py

tools-test: js $(RESTATE_SERVER) ## Restate's own agent tools (call, signal, workflow), against a Restate server
	cd typescript/tests && node restate-tools.ts

# One server at a time, on the default ports.
test-qjs-restate: $(SDK_RS) goldens $(RESTATE_SERVER) ## Durable JavaScript in Restate objects: runs, scopes and a crash after a commit, against a Restate server
	cd $(QJS_RESTATE) && $(CARGO) test --locked --release -- --include-ignored --test-threads 1

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
	rm -rf $(CARGO_TARGET_DIR) dist $(ADDON) typescript/packages/sdk-typescript/.build python/sdk/.build \
		typescript/node_modules typescript/*/node_modules typescript/packages/*/node_modules examples-typescript/node_modules \
		python/.venv python/sqlite-kv/python/sqlite_kv/*.so
