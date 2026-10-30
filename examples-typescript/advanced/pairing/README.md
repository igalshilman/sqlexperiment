# Pair programming

A coding agent you talk with about a project, for as long as the project
lasts. You chat with it from a terminal; it plans, edits and runs code in
the project's directory with pi's coding tools, has a reviewer read its
diff, and asks for your approval before it ships. The agent's loop runs in
Restate: each model step and each tool call is stored.

```
you ──▶ pair.ts (a terminal) ──▶ pair/<project> (pi) ──▶ WORKSPACE/<project>: read, write, edit, bash
          ▲   steer, approve        │  plan (a table of its own)
          └── follow ◀──────────────┤  reviewer/<project>.<task> (another model): what to fix
                                    └▶ approval (you) ──▶ shipping/<…> (a workflow): CI, then a pull request
```

| File                         | What                                                                                 |
| ---------------------------- | ------------------------------------------------------------------------------------ |
| [`pairing.ts`](pairing.ts)   | The pair and its reviewer, and the pair's handler of its own, `tasks`                |
| [`tools.ts`](tools.ts)       | The plan: a table of the project's database, which the pair's tools write            |
| [`shipping.ts`](shipping.ts) | The workflow that ships an approved change: CI, then a pull request (both stand-ins) |
| [`pair.ts`](pair.ts)         | The terminal: what you type goes to the pair, and its messages show as it works      |

What it shows:

- **A conversation that goes on**: a project is a key of the `pair`
  object, with its conversation in its database, and its files in a
  directory of its own. Leave and come back a day later: the terminal shows
  the conversation so far, and the pair has its files.
- **Steering**: a line you type while the pair works reaches it at its
  next tool call (`steer`), as "use pytest" would reach a person you pair
  with.
- **Tables of your own**: the plan is a table beside the agent's own
  (`schema: PLAN`), which its tools write and a shared handler reads while
  it works (`curl localhost:8080/pair/fizzbuzz/tasks`).
- **A second opinion**: the reviewer is an agent of its own, on another
  framework and model, with a conversation per task (`subagent`), so a
  second review of the same task sees the first.
- **A person in the loop**: the pair waits for your approval (`signal`),
  holding no process, for as long as you take.
- **Through crashes**: kill the process mid-message, and the pair carries
  on from its last stored step; a shipped change ships once, as the
  workflow is keyed by the tool call.

Run it from `examples-typescript/`, with `OPENAI_API_KEY`; the requests
are in [`pairing.ts`](pairing.ts)'s doc comment, and the terminal's
commands in [`pair.ts`](pair.ts)'s. The coding tools run real commands on
this machine: run it in a sandbox (a container, a VM).
