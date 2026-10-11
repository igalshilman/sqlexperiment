# A durable pi agent in a Restate virtual object

[pi-durable](https://www.npmjs.com/package/@earendil-works/pi-durable)
(`@earendil-works/pi-durable`) is a durable agent harness: it commits
conversations, model turns, tool calls and their state to storage before
anything is shown, and resumes unfinished work when the storage is opened
again. Here its storage is a Restate virtual object's SQL database
([SQL in Restate virtual objects](../node/README.md)).

- [`agent.mjs`](agent.mjs): an Agent object per session. Its `prompt` handler
  runs in one `ctx.sqlSession`, opens a pi-durable Harness over the session's
  database, submits the prompt and waits for the answer. A retried attempt
  (a crash, a lost connection) reopens the Harness and carries on:
  - a prompt is submitted once, by its id (pi-durable's `requestId`);
  - an answered prompt returns its answer;
  - unfinished work resumes: a model call is made again, and a tool call
    declared replay-safe runs again with the same call id.

  Its tools move money in the [ledger](../node/ledger.mjs), with the call id
  as the transfer id; the call to the ledger gets an idempotency key derived
  by the SDK.
- [`pi-storage.mjs`](pi-storage.mjs): pi-durable's SQLite storage on the
  session's database, through the `SqliteDatabase` facade pi-durable's
  storage expects. Each pi-durable commit is an SQL transaction, stored in
  order with everything else the handler does.
- [`pi-storage.test.mjs`](pi-storage.test.mjs): pi-durable's storage
  conformance suite on it (`npm run test:pi` in [`examples/`](..), or
  `make test-pi`): 24 tests pass.
- [`fake-model.mjs`](fake-model.mjs): a fake model for pi-ai, whose replies a
  function computes from the conversation, so an agent runs without an API
  key, deterministically, however many model calls retried attempts make:

  ```js
  import { createModels } from '@earendil-works/pi-ai/models';
  import { callTools, fakeProvider, lastText, say } from './fake-model.mjs';

  const models = createModels();
  models.setProvider(
    fakeProvider('teller', (context) =>
      /audit/.test(lastText(context)) ? callTools([['audit', {}]]) : say('How can I help?'),
    ),
  );
  ```

  The agent's default model is such a teller: "move 5 from 1 to 2" makes a
  transfer, "audit" audits the ledger. `PI_MODEL=anthropic/claude-sonnet-4-5`
  (with `ANTHROPIC_API_KEY`) picks a real model instead.

```sh
make examples-js   # from the repository root: the addon, the patched SDK, npm install
cd examples && npm run serve   # the agent, with the ledger its tools use
restate deployments register http://localhost:9080
curl localhost:8080/Ledger/bank/init --json '[8, 1000]'
curl localhost:8080/Agent/me/prompt --json '{"id": "p1", "text": "move 5 from 1 to 2"}'
```

The agents run through crashes in [the examples' crash test](../README.md#validation).
The Agent object keeps retrying rather than pausing after Restate's default
number of attempts, since a prompt is long work and each attempt resumes from
storage.
