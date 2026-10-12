# A durable pi agent in a Restate virtual object

[pi-durable](https://www.npmjs.com/package/@earendil-works/pi-durable)
(`@earendil-works/pi-durable`) is a durable agent harness: it commits
conversations, model turns, tool calls and their state to storage before
anything is shown, and resumes unfinished work when the storage is opened
again. Here its storage is a Restate virtual object's SQL database
([SQL in Restate virtual objects](../node/README.md)).

- [`with-pi.ts`](with-pi.ts): `withPi(ctx, { models, model, tools }, work)`
  runs `work` with a pi-durable Harness over the object's database, in one
  `ctx.sqlSession`, and closes it at the end. `pi.prompt(id, text)` answers a
  prompt once per id: it submits the prompt, waits for the answer, records it,
  and returns its text. A retried attempt (a crash, a lost connection) opens
  the Harness again and carries on:
  - a prompt is submitted once, by its id (pi-durable's `requestId`);
  - an answered prompt returns its answer;
  - unfinished work resumes: a model call is made again, and a tool call
    declared replay-safe runs again with the same call id.
- [`agent.ts`](agent.ts): an Agent object per session, with one handler:

  ```js
  prompt: (ctx, { id, text }) =>
    withPi(ctx, { models, model, tools: bank(ctx) }, async (pi) => ({ answer: await pi.prompt(id, text) })),
  ```

  Its tools move money in the [ledger](../node/ledger.ts), with the call id
  as the transfer id; each call to the ledger has an idempotency key made
  from the request id (`ctx.request().id`) and the tool call id.

- [`pi-storage.ts`](pi-storage.ts): pi-durable's SQLite storage on the
  session's database, through the `SqliteDatabase` facade pi-durable's
  storage expects. Each pi-durable commit is an SQL transaction, stored in
  order with everything else the handler does.
- [`pi-storage.test.ts`](pi-storage.test.ts): pi-durable's storage
  conformance suite on it (`npm run test:pi` in [`examples/`](..), or
  `make test-pi`): 24 tests pass.
- [`utils.ts`](utils.ts): the models. A fake model for pi-ai,
  `fakeProvider(name, respond)`, whose replies a function computes from the
  conversation, so an agent runs without an API key, deterministically,
  however many model calls retried attempts make; the scripted teller built on
  it ("move 5 from 1 to 2" makes a transfer, "audit" audits the ledger); and
  `modelFromEnv()`, which picks the teller, or a real model with
  `PI_MODEL=anthropic/claude-sonnet-4-5` (and `ANTHROPIC_API_KEY`).

```sh
make examples-js   # from the repository root: the addon, the patched SDK, npm install
cd examples && npm run serve   # the agent, with the ledger its tools use
docker run --rm -p 8080:8080 -p 9070:9070 --add-host=host.docker.internal:host-gateway ghcr.io/restatedev/restate:1.7.13   # Restate (see the repository's README, Requirements), in another terminal
restate deployments register http://host.docker.internal:9080   # as the server's container sees this host
curl localhost:8080/Ledger/bank/init --json '[8, 1000]'
curl localhost:8080/Agent/me/prompt --json '{"id": "p1", "text": "move 5 from 1 to 2"}'
```

The agents run through crashes in [the examples' crash test](../README.md#validation).
The Agent object keeps retrying rather than pausing after Restate's default
number of attempts, since a prompt is long work and each attempt resumes from
storage.
