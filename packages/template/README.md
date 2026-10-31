# Tutorial: a wallet, from a database to agents

In this tutorial you build one application step by step: a wallet for every
user. It starts as a Restate virtual object, gets its own SQL database,
learns to move money safely between wallets, and ends with an AI assistant
that pays your friends, asks you before large transfers, and delegates to a
savings coach.

Each lesson is one file that runs on its own: the wallet so far, trimmed to
what the lesson needs, plus one new idea. The comment at the top of each file
explains the idea, and lists the commands to try it.

## Before you start

You need Node 24 or later, pnpm, and Rust (for the SQL addon). Once, at the
repository's root:

```sh
pnpm install && pnpm build
```

Then, from this directory, run a lesson, start Restate, and tell Restate
where the lesson is:

```sh
node src/01_object.ts                                                           # the lesson, on port 9080
npx @restatedev/restate-server                                                  # Restate, in another terminal
npx @restatedev/restate -y deployments register --force http://localhost:9080   # tell Restate where it is
curl localhost:8080/wallet/alice/hello
```

To move on, stop the lesson, start the next one, and run the `register`
command again. Keep Restate running: the wallets you filled in one lesson are
still there in the next, just as a real application keeps its data across
releases.

Lessons 09 to 16 use OpenAI's models, with your API key in `OPENAI_API_KEY`.

## Part 1 · A database per key

| Lesson                                 | You learn                                              |
| -------------------------------------- | ------------------------------------------------------ |
| [`01_object.ts`](src/01_object.ts)     | A virtual object: handlers that Restate calls by key   |
| [`02_database.ts`](src/02_database.ts) | `ctx.db`: every wallet has its own SQL database        |
| [`03_views.ts`](src/03_views.ts)       | Views: reading the database while a handler is running |
| [`04_schema.ts`](src/04_schema.ts)     | Changing the tables of databases that already exist    |

## Part 2 · Changes that happen once

| Lesson                                       | You learn                                                     |
| -------------------------------------------- | ------------------------------------------------------------- |
| [`05_transaction.ts`](src/05_transaction.ts) | Transactions: spending fails as a whole, leaving no trace     |
| [`06_retries.ts`](src/06_retries.ts)         | Retries run a handler again: an id makes a deposit count once |
| [`07_calls.ts`](src/07_calls.ts)             | Calling another object, with an idempotency key               |
| [`08_sends.ts`](src/08_sends.ts)             | Transfers: a send that is delivered only if the debit commits |

## Part 3 · Agents

| Lesson                                           | You learn                                               |
| ------------------------------------------------ | ------------------------------------------------------- |
| [`09_prompt.ts`](src/09_prompt.ts)               | Asking a model from a handler, once                     |
| [`10_agent.ts`](src/10_agent.ts)                 | An assistant in every wallet, with its own conversation |
| [`11_tools.ts`](src/11_tools.ts)                 | Tools that read the wallet's database                   |
| [`12_tool_writes.ts`](src/12_tool_writes.ts)     | A tool that moves money, once per tool call             |
| [`13_restate_tools.ts`](src/13_restate_tools.ts) | Handlers as tools: paying a friend by their handle      |
| [`14_approval.ts`](src/14_approval.ts)           | A person in the loop: confirming large transfers        |
| [`15_subagents.ts`](src/15_subagents.ts)         | Delegating to another agent: a savings coach            |

## Appendix

| Lesson                                     | You learn                                          |
| ------------------------------------------ | -------------------------------------------------- |
| [`16_pi_durable.ts`](src/16_pi_durable.ts) | What `pi.prompt` does, written out with pi-durable |

## Tips

- Restate's UI, at <http://localhost:9070>, shows every request and what it
  did, including the retries of lesson 06.
- A request that waits (such as lesson 14's confirmation) keeps waiting
  after you move on, and Restate retries it against whatever lesson serves
  port 9080 then. Cancel it in the UI if it gets in the way.
- To start over with empty wallets, stop Restate and delete the
  `restate-data` directory it created.

## Next

- [`restate-sessions`](../sessions/README.md) and
  [`restate-agents`](../agents/README.md) cover everything the lessons use,
  in full.
- [The examples](../examples/README.md) are larger applications built the
  same way.
