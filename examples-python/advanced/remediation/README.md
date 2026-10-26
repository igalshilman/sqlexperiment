# Remediation supervisor

A security finding goes in; a ticket and a pull request come out, or a page
to the engineer on call. A finding is a Restate workflow, which supervises
agents that propose and audit fixes, under a policy and with deadlines.

```
finding ──▶ fast executor ──▶ judge ──▶ accepted? ──yes──▶ policy: sign-off? ──▶ ticket + pull request
              (deadline)        │                               │ (two days)
                                no: reasons                     no one / refused
                                ▼                               ▼
            careful executor ──▶ judge ──▶ no ──────────────▶ page the engineer on call
              (deadline)
```

| File                               | What                                                                                              |
| ---------------------------------- | ------------------------------------------------------------------------------------------------- |
| [`remediation.py`](remediation.py) | The workflow: the ladder, the deadlines, the policy gate, the audit trail; `sign_off` and `trail` |
| [`roles.py`](roles.py)             | The agents, on Pydantic AI, typed: two executors (a fix) and a judge (a verdict)                  |
| [`systems.py`](systems.py)         | Stand-ins for the inventory, advisories, ticketing, code host and pager, as Restate services      |
| [`finding.json`](finding.json)     | A finding to start with                                                                           |

What it shows:

- **Deadlines on agents**: each executor's call races a durable timer
  (`restate.select`); one past its deadline is cancelled, and its work with it.
- **An escalation ladder**: a rejected fix goes to the next, more careful
  executor, with the judge's reasons, rather than to the same one again.
- **A policy gate**: a fix of a critical finding, or a risky one, waits for
  a person's sign-off, for two days at most, with no process held.
- **An audit trail**: every proposal, verdict and sign-off is a decision in
  the workflow's state (`trail`), stored once and in order, as Restate's
  journal is.
- **Exactly once, outside**: the ticket and the pull request are opened in
  `ctx.run` steps of their services, so a retried call opens no second one.

Run it from `examples-python/`, with `OPENAI_API_KEY`; the requests are in
[`remediation.py`](remediation.py)'s docstring. `DEADLINE_SECONDS=5` makes the
fast executor time out, to see the ladder at work.
