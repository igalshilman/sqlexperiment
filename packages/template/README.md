# A tutorial in files

One concept per file, in order: a virtual object, then SQL in it, then
agents. Each file builds on the ones before it, and runs on its own.

| Lesson                                       | Concept                                                   |
| -------------------------------------------- | --------------------------------------------------------- |
| [`01_greeter.ts`](src/01_greeter.ts)         | A virtual object: `restate.sessionObject`, called by key  |
| [`02_database.ts`](src/02_database.ts)       | A SQLite database per key: `ctx.db`, `schema`             |
| [`03_views.ts`](src/03_views.ts)             | Views: reading beside the writer                          |
| [`04_schema.ts`](src/04_schema.ts)           | A schema that grows: a step per change                    |
| [`05_transaction.ts`](src/05_transaction.ts) | All or nothing: `ctx.db.transaction`                      |
| [`06_retries.ts`](src/06_retries.ts)         | Retries run the handler again: an id makes it change once |
| [`07_calls.ts`](src/07_calls.ts)             | Calling another object, with an idempotency key           |
| [`08_sends.ts`](src/08_sends.ts)             | Sends that commit with the rows: `tx.send`                |
| [`09_prompt.ts`](src/09_prompt.ts)           | A model in a handler: `pi.prompt`, answered once          |
| [`10_agent.ts`](src/10_agent.ts)             | An agent: a conversation per key                          |
| [`11_tool.ts`](src/11_tool.ts)               | A tool the model calls                                    |
| [`12_memory.ts`](src/12_memory.ts)           | An agent in your own object: its tools use your tables    |
| [`13_call.ts`](src/13_call.ts)               | Your objects as tools: `call`                             |
| [`14_approval.ts`](src/14_approval.ts)       | A person in the loop: `signal`                            |
| [`15_subagents.ts`](src/15_subagents.ts)     | Agents that delegate: `subagent`                          |
| [`16_pi_durable.ts`](src/16_pi_durable.ts)   | pi-durable by hand: its Harness over `ctx.db`             |

## Run a lesson

Once, at the repository's root: `pnpm install && pnpm build` (Node 24 or
later, pnpm, and Rust for the SQL addon). Then, from here:

```sh
node src/01_greeter.ts                                                         # the lesson, on port 9080
npx @restatedev/restate-server                                                 # Restate, in another terminal
npx @restatedev/restate -y deployments register --force http://localhost:9080  # tell Restate where it is
curl localhost:8080/greeter/alice/greet
```

Each lesson's comment says what to call. Every lesson serves on port 9080:
stop one, start the next, and register it again. Lessons 09 to 16 ask
OpenAI's models, with the API key in `OPENAI_API_KEY`.

Restate's UI, at <http://localhost:9070>, shows each invocation and what it
did. An invocation that an earlier lesson left running (a wait for an
approval, say) is retried against the lesson that now serves port 9080:
cancel it there.

## Then

- [`restate-session-objects`](../objects/README.md) and
  [`restate-agents`](../agents/README.md): everything the lessons touch,
  in full.
- [The examples](../examples/README.md): applications that put it together.
