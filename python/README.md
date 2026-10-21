# Python

SQL in Restate virtual objects, in Python: the same storage as the TypeScript
side, in a uv workspace of its own
([`pyproject.toml`](pyproject.toml)).

| Path                                | What                                                                                                                                                                                                                               |
| ----------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| [`sqlite-kv/`](sqlite-kv/README.md) | **The extension**, the `sqlite-kv` Python package: the crate over a key/value store implemented in Python (PyO3), as the [Node.js addon](../typescript/packages/sql-node/README.md) is for JavaScript.                             |
| [`fns/`](fns/README.md)             | **restate-fns**, the `restate-fns` Python package: the Restate SDK with SQL in virtual objects, `ctx.db` and `ctx.transaction()` on the extension, as [in TypeScript](../typescript/packages/fns/README.md), on the published SDK. |
| [`agents/`](agents/README.md)       | **The agent integrations**, the `restate-agents` Python package: LangGraph and Pydantic AI agents in virtual objects, their state in the object's database.                                                                        |
| [`tests/`](tests)                   | **The tests** against a real Restate server: the examples, the context without journal replay, Restate's tools for agents, and a bank through crashes.                                                                             |

The examples are beside this directory, in
[`examples-python/`](../examples-python/README.md): each a small file you run with uv.

## Build and test

From the repository's root (see [its README](../README.md#build-and-test)):

```sh
make py                    # the workspace, in .venv here: the extension, restate-fns and the integrations
make test-sql-py           # the extension's and restate-fns's tests
make test-agents-py        # the agent integrations' tests, on a scripted model
make concurrency-test-py   # the examples, the context without journal replay, agents and Restate's tools, against a Restate server
make crash-test-py         # the bank and an agent through crashes, against a Restate server
make lint-py               # ruff
make wheels                # dist/: Linux wheels of the SDK, the extension, restate-fns and restate-agents, for a container (Modal)
```
