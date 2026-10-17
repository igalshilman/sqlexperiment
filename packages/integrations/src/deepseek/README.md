# DeepSeek Harness

The [DeepSeek Harness](https://github.com/deepseek-ai/deepseek-harness)
(`@deepseek-ai/dsh-*`) is an agent harness built from plugins on
[cordis](https://www.npmjs.com/package/@deepseek-ai/cordis): an agent loop, a
model service, a tool registry, and an event-sourced session that is the
source of truth for everything the agent did. Here the session is in the
agent's SQL database ([SQL in Restate virtual objects](../../../sdk-typescript/SQL.md)),
and is the object's key. The integration pins the harness's `next` release
line, 0.2.0-rc.2, exactly: its packages are release candidates that require
each other's exact versions.

## Use

```ts
import { serve } from 'restate-agents';
import * as deepseek from 'restate-agents/deepseek';

const assistant = deepseek.agent({ name: 'assistant', system: 'Answer briefly.', tools: [weather] });
await serve(assistant);
```

| Option   | What                                                                                         |
| -------- | -------------------------------------------------------------------------------------------- |
| `model`  | `deepseek/model-id`; or else `MODEL`, or else `deepseek/deepseek-flash` (`DEEPSEEK_API_KEY`) |
| `system` | The system prompt                                                                            |
| `tools`  | `tool()`s and Restate's tools of `restate-agents`, or the harness's `defineTool`s            |

For an object of your own, `deepseek.chat(options)` and
`deepseek.messages()` are the handlers.

## Through crashes

The persistence ([`persistence.ts`](persistence.ts),
`SqlSessionPersistence`) keeps each session's header, and its event log, one
row per event; a flush, the harness's durability barrier, stores what was
appended before it in one SQL transaction. The handler is not replayed: a
retried attempt boots the harness again and resumes the session from the
stored log. The harness does not resume an interrupted turn: resuming closes
it, and leaves the model to decide what to do about it. So:

- **A tool runs only once its call is stored.** The harness's checkpoint
  policy flushes the session before each model request, each tool body and
  each step.
- **A tool call stored without a result has an unknown outcome.** Resuming
  gives each such call an error result that says so, and tells the model to
  retry only an idempotent operation; the handler then asks the agent to
  continue. A tool that must not run twice needs a way for a retry to name
  the call it retries (the bank agent's `transfer` takes `retryOf`).
- **A tool's error fails the attempt**, and Restate retries it; a
  `restate.TerminalError` of a `restate-agents` tool is its answer to the
  model. The failed attempt stores nothing more, so its retry finds the call
  without a result, as above.
- **A failed turn fails the attempt**: a model request that failed, or a
  checkpoint that could not store the log, and Restate retries.
- **A message is answered once per invocation**: the message carries the
  invocation id, and a retried attempt returns the answer of the turn that
  finished it. A message that a crash interrupted is finished before the next
  one starts.

## Run

```sh
cd examples && DEEPSEEK_API_KEY=… node deepseek.ts
```

With a Restate server, and the agent registered with it: see
[Run an example](../../../../examples/README.md#run-an-example).

## Tests

The tests run on a scripted model, [`tests/fakes/deepseek.ts`](../../../../tests/fakes/deepseek.ts),
which makes a fresh tool call id for every call, as a model does
(`make test-ts T=deepseek`, from the repository root):

- [`tests/agents/deepseek.test.ts`](../../../../tests/agents/deepseek.test.ts):
  each point above, with crashes that stop an attempt in a tool call; and
  `messages`. Without the checkpoint policy four of its tests fail, and with
  a transfer tool that ignores `retryOf` three do.
- [`tests/agents/deepseek.ts`](../../../../tests/agents/deepseek.ts): the
  bank agent, whose `transfer` uses `retryOf`, or its own call id, as the
  transfer id, through crashes in [the crash test](../../../../tests/crash.ts)
  (`make crash-test`): every message moves money exactly once.
