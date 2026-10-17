# Python

SQL in Restate virtual objects, in Python: the same storage and the same
patched shared core as the TypeScript side, in a uv workspace of its own
([`pyproject.toml`](pyproject.toml)).

| Path                                | What                                                                                                                                                                                                               |
| ----------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| [`sqlite-kv/`](sqlite-kv/README.md) | **The extension**, the `sqlite-kv` Python package: the crate over a key/value store implemented in Python (PyO3), as the [Node.js addon](../typescript/packages/sql-node/README.md) is for JavaScript.             |
| [`sdk/`](sdk/README.md)             | **The SDK changes.** Patches to the Restate Python SDK, over [the patched shared core](../core/sdk-shared-core/README.md): handlers without journal replay, and `ctx.db` and `ctx.transaction()` on the extension. |
| [`agents/`](agents/README.md)       | **The agent integrations**, the `restate-agents` Python package: LangGraph and Pydantic AI agents in virtual objects, their state in the object's database.                                                        |
| [`examples/`](examples/README.md)   | **The examples**, each a small file you run with uv: notes in SQL, and agents on LangGraph and Pydantic AI; a project of their own, as yours would be.                                                             |
| [`tests/`](tests)                   | **The tests** against a real Restate server: the examples, the context without journal replay, Restate's tools for agents, and a bank through crashes.                                                             |

## Build and test

From the repository's root (see [its README](../README.md#build-and-test)):

```sh
make py                    # the workspace, in .venv here: the patched SDK and the extension
make test-sdk-py           # the patched SDK's own tests
make test-sql-py           # the extension's tests
make test-agents-py        # the agent integrations' tests, on a scripted model
make concurrency-test-py   # the examples, the context without journal replay, agents and Restate's tools, against a Restate server
make crash-test-py         # the bank and an agent through crashes, against a Restate server
make lint-py               # ruff
make wheels                # dist/: Linux wheels of the SDK, the extension and restate-agents, for a container (Modal)
```
