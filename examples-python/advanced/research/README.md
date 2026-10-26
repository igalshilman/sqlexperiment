# Deep research

A question goes in; a report with its sources comes out, after a person
approved the plan. A run is a Restate workflow; each agent is a virtual
object of its own, with its conversation in its database.

```
                    ┌─────────── you: approve, or say what to change ───────────┐
                    ▼                                                            │
question ──▶ planner (Pydantic AI) ──▶ plan ──▶ review ─────────────────────────┘
                                                 │ approved
                         ┌───────────────────────┼───────────────────────┐
                         ▼                       ▼                       ▼
               researcher r1.0          researcher r1.1          researcher r1.2     (LiteLLM, on Wikipedia, at once)
                         └───────────────────────┼───────────────────────┘
                                                 ▼
                              writer (LiteLLM) ⇄ critic (Pydantic AI)  ──▶ report
```

| File                         | What                                                                                      |
| ---------------------------- | ----------------------------------------------------------------------------------------- |
| [`research.py`](research.py) | The workflow: plan, review, research at once, write and review; `review` and `progress`   |
| [`roles.py`](roles.py)       | The agents, each with typed handlers of its own (`plan`, `investigate`, `draft`, `judge`) |
| [`wiki.py`](wiki.py)         | The researchers' tools: Wikipedia's search and articles, through its public API           |

What it shows:

- **A person in the loop, for as long as it takes**: the run waits on a
  durable promise (`ctx.promise`), with no process held. Each round of
  changes is a promise of its own.
- **Agents at once**: the researchers are calls the workflow makes together
  and gathers (`restate.gather`), each in its own object
  (`researcher/r1.0`, `researcher/r1.1`...), where you can read or follow
  its conversation (`uv run basic/follow.py researcher r1.0`).
- **Typed agents**: the planner and the critic answer with pydantic models
  (Pydantic AI's `output_type`), which the workflow branches on.
- **Conversations that go on**: the planner changes the plan it made, the
  writer revises the report it wrote, and the critic checks its notes were
  followed: each keeps its conversation across the calls of a run.
- **Through crashes**: each step of the workflow is stored; each agent
  carries on from its last stored step. Cancel the run, and the calls it
  waits on are cancelled with it.

Run it from `examples-python/`, with `OPENAI_API_KEY`; the requests are in
[`research.py`](research.py)'s docstring.
