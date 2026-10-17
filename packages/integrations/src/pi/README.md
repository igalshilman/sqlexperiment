# pi

[pi-durable](https://www.npmjs.com/package/@earendil-works/pi-durable) is a
durable agent harness: it commits conversations, model turns and tool calls
to storage, and resumes unfinished work when the storage is opened again.
Here its storage is the agent's SQL database, in the Restate virtual object
([SQL in Restate virtual objects](../../../sdk-typescript/SQL.md)).

## Use

```ts
import { serve } from 'restate-agents';
import * as pi from 'restate-agents/pi';

const assistant = pi.agent({ name: 'assistant', system: 'Answer briefly.', tools: [weather] });
await serve(assistant);
```

| Option      | What                                                                                                                           |
| ----------- | ------------------------------------------------------------------------------------------------------------------------------ |
| `model`     | `provider/model-id`, of any provider pi-ai knows; or else `MODEL`, or else `anthropic/claude-sonnet-5-5` (`ANTHROPIC_API_KEY`) |
| `models`    | A pi-ai model registry, for a provider of your own                                                                             |
| `system`    | The system prompt                                                                                                              |
| `tools`     | `tool()`s and Restate's tools of `restate-agents`, or pi-durable's `defineTool`s                                               |
| `workspace` | A directory for pi-durable's coding tools (`read`, `write`, `edit`, `bash`), on the local file system; use a sandbox           |

For an object of your own, `pi.chat(options)` and `pi.messages()` are the
handlers. For more than a prompt, `withPi(ctx, options, work)` runs `work`
with the Harness, on the same options as `chat` (each for this invocation):
`prompt(text)` answers a prompt once per invocation, and `conversation()` is
the object's conversation, on the model, for pi-durable's own API (`reset`,
`compact`, `entries`...):

```ts
chat: chatHandler(async (ctx, message) => pi.prompt(ctx, message, await optionsOf(ctx))),
reset: (ctx, handoff: string) =>
  pi.withPi(ctx, {}, async ({ conversation, context }) => (await conversation()).reset(handoff, context)),
```

A conversation goes on with the model of each invocation: a `model` (or
MODEL) changed since it started is the model of its next turn.
[`examples/harness.ts`](../../../../examples/harness.ts) keeps each user's
model and persona in the object's database.

## Through crashes

Each pi-durable commit is an SQL transaction in the object's database
([`storage.ts`](storage.ts)), stored in order with everything else the
handler does. A retried attempt opens the Harness again and carries on:

- a prompt is submitted once, by its id (pi-durable's `requestId`, the
  invocation's id);
- an answered prompt returns its answer;
- unfinished work resumes: a model call is made again, and a tool call runs
  again with the same call id, which a tool passes on to stay idempotent.

The files of a `workspace` are not in the database: they last as long as the
disk does, and a coding tool a crash cut off is not run again (the model is
told it was interrupted).

## Run

```sh
cd examples && ANTHROPIC_API_KEY=… node chat.ts
```

With a Restate server, and the agent registered with it: see
[Run an example](../../../../examples/README.md#run-an-example).
[`examples/chat.ts`](../../../../examples/chat.ts) is a chat agent with a
tool, and [`examples/coding.ts`](../../../../examples/coding.ts) a pi
coordinator that hands coding tasks to OpenCode coders.

## Tests

The tests run on a scripted model, [`tests/fakes/pi.ts`](../../../../tests/fakes/pi.ts),
which needs no key (`make test-ts T=pi`, from the repository root):

- [`tests/storage/pi.test.ts`](../../../../tests/storage/pi.test.ts):
  pi-durable's storage conformance suite on the object's database;
  [`pi-workspace.test.ts`](../../../../tests/storage/pi-workspace.test.ts),
  `withPi` with a workspace.
- [`tests/agents/pi-bank.test.ts`](../../../../tests/agents/pi-bank.test.ts):
  the bank agent, with crashes at any of a message's state requests.
- [`tests/agents/pi.test.ts`](../../../../tests/agents/pi.test.ts): the chat,
  its system prompt, `messages`, and sub-agents.
- [`tests/agents/pi.ts`](../../../../tests/agents/pi.ts): the bank agent,
  whose tools move money in the [ledger](../../../../tests/sql/ledger.ts)
  with the tool call id as the transfer id, through crashes in
  [the crash test](../../../../tests/crash.ts) (`make crash-test`).
