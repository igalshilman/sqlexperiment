# SQL in Restate virtual objects

An experiment: each Restate virtual object gets its own SQLite-compatible
database, stored in the object's state, and its handlers run without journal
replay, so a retried attempt runs again against the state as stored.

## What it looks like

An agent whose tools are Restate itself: it looks an order up with a
service, waits for a manager's approval, and refunds with a workflow. Its
conversation is in its own SQL database, and a crash, or a manager who takes
a day, changes nothing ([`examples/support.ts`](examples/support.ts)):

```ts
const support = pi.agent({
  name: 'support',
  tools: [
    call(orders, 'status', { description: 'The status of an order.', input: z.object({ order: z.string() }) }),
    signal('approval', { description: "Wait for a manager's decision on a refund." }),
    workflow(refund, { description: 'Refund an order.', input: z.object({ order: z.string(), amount: z.number() }) }),
  ],
});

await serve(orders, refund, support);
```

```sh
curl localhost:8080/support/alice/chat -d 'Order 103 arrived broken, can I get my $30 back?'
curl localhost:8080/support/alice/signal --json '{"name": "approval", "value": "approved"}'
```

The same agent runs on pi, OpenCode, LangGraph, Claude (the Anthropic SDK)
or the DeepSeek Harness: see [`restate-agents`](packages/integrations/README.md)
and [the examples](examples/README.md).

## Layers

Each on the one before, then the examples and the tests:

| Path                                                            | What                                                                                                                                                                                                                                                                                                                                                                                                                                                                                             |
| --------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| [`sqlite-kv/`](sqlite-kv/README.md)                             | **The storage.** SQL over an asynchronous key/value store, for namespaces with one owner at a time: a Rust crate with its tests, benchmarks, [design](sqlite-kv/docs/DESIGN.md) and [TLA+ specification](sqlite-kv/spec/README.md). It knows nothing about Restate.                                                                                                                                                                                                                              |
| [`packages/sql-node/`](packages/sql-node/README.md)             | **The Node.js addon**, the `sqlite-kv` npm package: the crate over a key/value store implemented in JavaScript.                                                                                                                                                                                                                                                                                                                                                                                  |
| [`packages/sdk-typescript/`](packages/sdk-typescript/README.md) | **The SDK changes.** Patches to the Restate TypeScript SDK, over [the patched shared core](packages/sdk-shared-core/README.md) (`JournalReplay::Disabled`): handlers without journal replay (`journalReplay: false`), their own context type with plain promises, and `ctx.db` and `ctx.transaction` on the addon. [How SQL in virtual objects works](packages/sdk-typescript/SQL.md).                                                                                                           |
| [`packages/integrations/`](packages/integrations/README.md)     | **The integrations**, the `restate-agents` package: agent frameworks in virtual objects, their state in the object's database, each a `chat()` handler: [pi](packages/integrations/src/pi/README.md) (`restate-agents/pi`), [OpenCode](packages/integrations/src/opencode/README.md), [LangGraph](packages/integrations/src/langgraph/README.md), [Claude](packages/integrations/src/claude/README.md) (the Anthropic SDK) and [DeepSeek Harness](packages/integrations/src/deepseek/README.md). |
| [`examples/`](examples/README.md)                               | **The examples**, each a small file you run with node: a chat on each framework, a coding team, notes in SQL, and [a tool gateway](examples/gateway/README.md).                                                                                                                                                                                                                                                                                                                                  |
| [`examples-py/`](examples-py/README.md)                         | **The Python examples**, each a small file you run with uv: notes in SQL, and agents on LangGraph.                                                                                                                                                                                                                                                                                                                                                                                               |
| [`tests/`](tests)                                               | **The tests**: the agents with bank tools on scripted models, the storage conformance suites, SQL in objects (a ledger and a shop), and the crash and concurrency tests against a real Restate server.                                                                                                                                                                                                                                                                                           |
| [`python/`](python/README.md)                                   | **The Python side**, its own uv workspace: [the Python extension](python/sqlite-kv/README.md) (the `sqlite-kv` Python package), [the Python SDK changes](python/sdk/README.md) (patches to the Restate Python SDK, over the same shared core: handlers without journal replay, `ctx.db` and `ctx.transaction()`), [agents](python/agents/README.md) (`restate-agents` for Python: LangGraph), and their tests.                                                                                   |

## Requirements

|                | Version                                                                                                                |
| -------------- | ---------------------------------------------------------------------------------------------------------------------- |
| Restate server | 1.7.13, the latest release (or later)                                                                                  |
| TypeScript SDK | 1.17.3 with [the patches](packages/sdk-typescript/README.md), over the shared core's unreleased main; `make` builds it |
| Python SDK     | 1.0.5 with [the patches](python/sdk/README.md), over the same shared core; `make` builds it                            |

```sh
docker run --rm -p 8080:8080 -p 9070:9070 --add-host=host.docker.internal:host-gateway ghcr.io/restatedev/restate:1.7.13
```

Or run `restate-server` directly: `make restate-server` downloads the release
binary for this platform to `target/restate/restate-server`.

## Build and test

With [Nix](https://nixos.org) (flakes), `nix develop` gives a shell with every
tool below, on macOS and Linux ([`flake.nix`](flake.nix)):

```sh
nix develop       # or `direnv allow`, with use flake in .envrc
make              # build everything: the crate, the addon, the patched SDK, the pnpm and uv workspaces
make test         # every test that needs no Restate server
make test-ts T=langgraph   # the TypeScript tests (vitest in tests/), optionally filtered
make test-sdk-py  # the patched Python SDK's own tests
make test-sql-py  # the Python extension's tests
make crash-test         # the integrations and SQL objects through crashes, against a Restate server
make concurrency-test   # the context without journal replay, against a Restate server
make crash-test-py concurrency-test-py   # the same, in Python
make help
```

Without Nix: besides Rust, the Node.js parts need node 24 or later (the
examples and tests are TypeScript that node runs as it is, and OpenCode needs 24) and pnpm, which installs the workspace (`make js`); the patched TypeScript
SDK also needs the `wasm32-unknown-unknown` target and
`wasm-bindgen` 0.2.121 (`cargo install wasm-bindgen-cli --version 0.2.121`).
The Python parts need Python 3.13 and [uv](https://docs.astral.sh/uv/), which
installs [their workspace](python/README.md) into `python/.venv` (`make py`),
building the patched SDK and the extension with maturin.

The tests against a Restate server download its release binary on first use
(see the [`Makefile`](Makefile): `RESTATE_VERSION`, `RESTATE_FROM`, `RESTATE_SERVER`).
