# A durable pi agent in a Restate virtual object

[pi-durable](https://www.npmjs.com/package/@earendil-works/pi-durable)
(`@earendil-works/pi-durable`) is a durable agent harness: it commits
conversations, model turns, tool calls and their state to storage before
anything is shown, and resumes unfinished work when the storage is opened
again. Here its storage is a Restate virtual object's SQL database
([SQL in Restate virtual objects](../../../sdk/SQL.md)).

- [`index.ts`](index.ts): `restate-agents/pi`. `withPi(ctx, { models,
  model, tools, workspace }, work)` runs `work` with a pi-durable Harness
  over the object's database, `ctx.db`, and closes it at the end.
  `pi.prompt(id, text)` answers a prompt once per id: it submits the prompt,
  waits for the answer, records it, and returns its text. A retried attempt
  (a crash, a lost connection) opens the Harness again and carries on:
  - a prompt is submitted once, by its id (pi-durable's `requestId`);
  - an answered prompt returns its answer;
  - unfinished work resumes: a model call is made again, and a tool call
    declared replay-safe runs again with the same call id.

  `prompt(ctx, text, options)` answers a message once per invocation, its id
  the invocation's (`ctx.request().id`), and `chat(options)` makes a handler
  of it, which takes a message (a string) and returns the answer:

  ```js
  const assistant = pi.agent({ name: 'assistant', tools: [weather] }); // a virtual object
  handlers: { chat: pi.chat() } // a handler of an object of your own
  // or, for more than a prompt:
  chat: (ctx, message) => withPi(ctx, { tools }, (pi) => pi.prompt(ctx.request().id, message)),
  ```

  A caller that resends a message uses an idempotency key. With a
  `workspace` directory, the agent also gets pi-durable's coding tools over
  it, on the local file system.

- [`storage.ts`](storage.ts) (`restate-agents/pi/storage`): pi-durable's
  SQLite storage on the session's database, through the `SqliteDatabase`
  facade pi-durable's storage expects. Each pi-durable commit is an SQL
  transaction, stored in order with everything else the handler does.
- [`tests/storage/pi.test.ts`](../../../../tests/storage/pi.test.ts): pi-durable's storage
  conformance suite on it (`make test-ts T=pi` from the repository root): 24
  tests pass. [`tests/storage/pi-workspace.test.ts`](../../../../tests/storage/pi-workspace.test.ts):
  `withPi` with a workspace, whose coding tools write to the local file system.
- [`tests/agents/pi.ts`](../../../../tests/agents/pi.ts): an Agent object per session,
  with one handler, `chat`, whose tools move money in the
  [ledger](../../../../tests/sql/ledger.ts), with the call id as the transfer id; each call
  to the ledger has an idempotency key made from the request id
  (`ctx.request().id`) and the tool call id. [Its tests](../../../../tests/agents/pi.test.ts)
  run the handler through crashes.
- [`examples/chat.ts`](../../../../examples/chat.ts): a durable chat agent with a tool, `pi.agent({ name: 'assistant', tools: [weather] })`; [`examples/coding.ts`](../../../../examples/coding.ts): a pi
  coordinator that hands coding tasks to OpenCode coders with `subagent` (of `restate-agents`).

The model is `model`, or else `MODEL`, by default
`anthropic/claude-sonnet-5-5` (with `ANTHROPIC_API_KEY`). The tests use a
scripted one, [`tests/fakes/pi.ts`](../../../../tests/fakes/pi.ts), which needs no key.

```sh
make js   # from the repository root: the addon, the patched SDK, pnpm install
cd examples && ANTHROPIC_API_KEY=… node chat.ts   # a chat session, on port 9080
curl localhost:8080/assistant/alice/chat -d 'Hi, I am Alice'
```

A Restate server runs beside it, with the agent registered: see
[Run an example](../../../../examples/README.md#run-an-example).

The agents run through crashes in [the crash test](../../../../tests/crash.ts) (`make crash-test`).
The test Agent object keeps retrying rather than pausing after Restate's default
number of attempts, since a prompt is long work and each attempt resumes from
storage.
