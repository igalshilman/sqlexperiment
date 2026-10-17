# Python

SQL in Restate virtual objects, in Python: the same storage and the same
patched shared core as the TypeScript side, in a uv workspace of its own
([`pyproject.toml`](pyproject.toml)).

| Path                                | What                                                                                                                                                                                                                   |
| ----------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| [`sqlite-kv/`](sqlite-kv/README.md) | **The extension**, the `sqlite-kv` Python package: the crate over a key/value store implemented in Python (PyO3), as the [Node.js addon](../packages/sql-node/README.md) is for JavaScript.                            |
| [`sdk/`](sdk/README.md)             | **The SDK changes.** Patches to the Restate Python SDK, over [the patched shared core](../packages/sdk-shared-core/README.md): handlers without journal replay, and `ctx.db` and `ctx.transaction()` on the extension. |
| [`agents/`](agents/README.md)       | **The agent integrations**, the `restate-agents` Python package: LangGraph agents in virtual objects, their checkpoints in the object's database.                                                                      |
| [`examples/`](#examples)            | **The examples**, each a file you run with uv.                                                                                                                                                                         |
| [`tests/`](tests)                   | **The tests** against a real Restate server: the examples and the context without journal replay, and the bank through crashes.                                                                                        |

## Examples

Each serves on port 9080 (or `PORT`). From here, after `make py` at the
repository's root:

| File                              | What                                                                                                                                                                            |
| --------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| [`notes.py`](examples/notes.py)   | SQL in a virtual object, with `ctx.db`                                                                                                                                          |
| [`bank.py`](examples/bank.py)     | One object per account, and transfers that move money exactly once: the debit and the credit's send in one `ctx.transaction()`                                                  |
| [`chat.py`](examples/chat.py)     | A durable chat agent on LangGraph, with a tool (`ANTHROPIC_API_KEY`)                                                                                                            |
| [`teller.py`](examples/teller.py) | A bank teller agent on LangGraph, whose tool moves money in the bank through Restate, once per tool call (`ANTHROPIC_API_KEY`)                                                  |
| [`memory.py`](examples/memory.py) | An assistant on LangGraph that remembers facts about you in its own SQL table, through its tools (`ctx.db`), with handlers of your own beside the agent's (`ANTHROPIC_API_KEY`) |

```sh
uv run examples/bank.py
npx @restatedev/restate-server                                         # Restate, in another terminal
npx @restatedev/restate deployments register http://localhost:9080
curl localhost:8080/account/alice/deposit --json '{"id": "d1", "amount": 100}'
curl localhost:8080/account/alice/transfer --json '{"id": "t1", "to": "bob", "amount": 30}'
curl localhost:8080/account/bob/balance
```

## Build and test

From the repository's root (see [its README](../README.md#build-and-test)):

```sh
make py                    # the workspace, in .venv here: the patched SDK and the extension
make test-sdk-py           # the patched SDK's own tests
make test-sql-py           # the extension's tests
make test-agents-py        # the agent integrations' tests, on a scripted model
make concurrency-test-py   # the examples, the context without journal replay and an agent, against a Restate server
make crash-test-py         # the bank and an agent through crashes, against a Restate server
make lint-py               # ruff and mypy
```
