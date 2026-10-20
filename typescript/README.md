# TypeScript

SQL in Restate virtual objects, in TypeScript: the storage of
[`core/`](../core/README.md) as a Node.js addon, the patched TypeScript SDK,
and the agent integrations, in a pnpm workspace of its own
([`pnpm-workspace.yaml`](pnpm-workspace.yaml)).

| Path                                                            | What                                                                                                                                                              |
| --------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| [`packages/sql-node/`](packages/sql-node/README.md)             | **The Node.js addon**, the `sqlite-kv` npm package: the crate over a key/value store implemented in JavaScript.                                                   |
| [`packages/sdk-typescript/`](packages/sdk-typescript/README.md) | **The SDK changes.** Patches to the Restate TypeScript SDK: handlers without journal replay (on the SDK's explicit recovery), and `ctx.db` and `ctx.transaction`. |
| [`packages/integrations/`](packages/integrations/README.md)     | **The integrations**, the `restate-agents` package: pi, OpenCode, LangGraph, Claude (the Anthropic SDK) and the DeepSeek Harness, in virtual objects.             |
| [`examples-typescript/`](../examples-typescript/README.md)      | **The examples**, each a small file you run with node: beside this directory, and a member of this workspace.                                                     |
| [`tests/`](tests)                                               | **The tests**: the agents on scripted models, the storage conformance suites, SQL in objects, and the crash and concurrency tests against a real Restate server.  |

## Build and test

From the repository's root (see [its README](../README.md#build-and-test)):

```sh
make js                    # the workspace, installed with the patched SDK and the addon
make test-addon            # the addon's tests
make test-sdk-ts           # the patched SDK's own tests
make test-ts T=langgraph   # the tests here, without a Restate server, optionally filtered
make crash-test            # the integrations and SQL objects through crashes, against a Restate server
make concurrency-test      # the context without journal replay, against a Restate server
make tools-test            # Restate's own agent tools, against a Restate server
make lint-ts               # tsc, oxlint and oxfmt
```
