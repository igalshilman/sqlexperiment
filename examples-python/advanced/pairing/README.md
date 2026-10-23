# Pair programming

A coding agent you talk with about a project, for as long as the project
lasts. You chat with it from a terminal; it plans, edits and runs code in a
sandbox of the project's own, has a reviewer read its diff, and asks for
your approval before it ships. The agent's loop runs in Restate, not in a
coding harness: each model step and each tool call is stored.

```
you ──▶ pair.py (a terminal) ──▶ pair/<project> (Claude) ──▶ sandbox: files, git, tests
          ▲   steer, approve        │  plan (its tasks table)
          └── follow ◀──────────────┤  reviewer/<project>.<task> (another model): what to fix
                                    └▶ approval (you) ──▶ shipping/<…> (a workflow): CI, then a pull request
```

| File                         | What                                                                                              |
| ---------------------------- | ------------------------------------------------------------------------------------------------- |
| [`pairing.py`](pairing.py)   | The pair and its reviewer; the pair's handlers of its own: `tasks`, `files`, `compact`, `discard` |
| [`tools.py`](tools.py)       | The sandbox, an `edit_file` tool on it, and the plan: a table of the project's database           |
| [`shipping.py`](shipping.py) | The workflow that ships an approved change: CI, then a pull request (both stand-ins)              |
| [`pair.py`](pair.py)         | The terminal: what you type goes to the pair, and its messages show as it works                   |

What it shows:

- **A conversation that goes on**: a project is a key of the `pair` object,
  with its conversation in its database, and its files in a sandbox of its
  own (Modal, or a directory here). Leave and come back a day later: the
  terminal shows the conversation so far, and the pair has its files.
- **Steering**: a line you type while the pair works reaches it at its
  next tool call (`steer`), as "use pytest" would reach a person you pair
  with.
- **Tables of your own**: the plan is a table beside the agent's own
  (`schema=TASKS`), which its tools write and a shared handler reads while
  it works (`curl localhost:8080/pair/fizzbuzz/tasks`).
- **A second opinion**: the reviewer is an agent of its own, on another
  model, with a conversation per task (`subagent`), so a second review of
  the same task sees the first.
- **A person in the loop**: the pair waits for your approval (`signal`),
  holding no process, for as long as you take.
- **Through crashes**: kill the process mid-message, and the pair carries on
  from its last stored step; a shipped change ships once, as the workflow
  is keyed by the tool call.

Run it from `examples-python/`, with `ANTHROPIC_API_KEY` (and
`MODAL_TOKEN_ID` and `MODAL_TOKEN_SECRET` for a Modal sandbox); the requests
are in [`pairing.py`](pairing.py)'s docstring, and the terminal's commands in
[`pair.py`](pair.py)'s.
