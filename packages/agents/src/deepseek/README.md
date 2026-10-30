# DeepSeek Harness

The [DeepSeek Harness](https://github.com/deepseek-ai/deepseek-harness)
(`@deepseek-ai/dsh-*`) is an agent harness built from plugins on
[cordis](https://www.npmjs.com/package/@deepseek-ai/cordis): an agent loop, a
model service, a tool registry, and an event-sourced session that is the
source of truth for everything the agent did. Here the session is in the
agent's SQL database ([SQL in Restate virtual objects](../../../sql/README.md)),
and is the object's key. The integration pins the harness's `next` release
line, 0.2.1-rc.2, exactly: its packages are release candidates that require
each other's exact versions.

## Use

```ts
import { serve } from 'restate-agents';
import * as deepseek from 'restate-agents/deepseek';
import { weather } from './weather.ts';

const assistant = deepseek.agent({ name: 'assistant', system: 'Answer briefly.', tools: [weather] });
await serve(assistant);
```

| Option   | What                                                                                                                                                           |
| -------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `model`  | `provider/model-id`; or else `MODEL`, or else `deepseek/deepseek-flash` (see below)                                                                            |
| `system` | The system prompt                                                                                                                                              |
| `tools`  | `tool()`s and Restate's tools of `restate-agents`, or the harness's `defineTool`s                                                                              |
| `output` | A zod object: the answer as data, which the agent replies with as JSON, and `chat` answers with (see [Structured answers](../../README.md#structured-answers)) |

## Models

DeepSeek's models go to DeepSeek, with `DEEPSEEK_API_KEY`. Any other model
pi-ai knows goes through the harness's pi-ai adapter
(`@deepseek-ai/dsh-llm-pi-ai`), with its provider's usual key: nothing else
to set up.

```ts
deepseek.agent({ name: 'assistant' }); // deepseek/deepseek-flash, with DEEPSEEK_API_KEY
deepseek.agent({ name: 'assistant', model: 'openai/gpt-6-sol' }); // with OPENAI_API_KEY
deepseek.agent({ name: 'assistant', model: 'anthropic/claude-sonnet-5-5' }); // with ANTHROPIC_API_KEY
```

A provider or model pi-ai does not know fails when the agent is defined; a
missing or wrong key fails the invocation with a terminal error (424).

For an object of your own, `deepseek.chat(options)` and
`deepseek.messages()` are the handlers; `options` may be a function of the
invocation's context (each user's system prompt, say). `deepseek.prompt(ctx,
text, options)` answers a prompt in a handler of your own, once, however
often the handler runs; each prompt of an invocation is a prompt of its own.

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
  the call it retries (the bank agent's `transfer` takes `retryOf`). Restate's own tools
  (`call`, `send`, `workflow`, `subagent`, `signal`) need none: made again
  with the same input, the call is the one the cut-off call made, which
  matters for a long one, such as a wait for an approval, since a suspension
  (after Restate's inactivity timeout) restarts the handler too.
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
cd packages/examples && DEEPSEEK_API_KEY=… node basic/deepseek.ts
cd packages/examples && MODEL=openai/gpt-6-sol OPENAI_API_KEY=… node basic/deepseek.ts   # on GPT
```

With a Restate server, and the agent registered with it: see
[Run an example](../../../examples/README.md#run-an-example).

## Tests

The tests run on a scripted model, [`test/fakes/deepseek.ts`](../../test/fakes/deepseek.ts),
which makes a fresh tool call id for every call, as a model does
(`pnpm --filter restate-agents test deepseek`, from the repository root):

- [`test/deepseek.test.ts`](../../test/deepseek.test.ts):
  each point above, with crashes that stop an attempt in a tool call, or
  at any of its state requests; and
  `messages`. Without the checkpoint policy four of its tests fail, and with
  a transfer tool that ignores `retryOf` three do.
- [`test/deepseek.ts`](../../test/deepseek.ts): the
  bank agent, whose `transfer` uses `retryOf`, or its own call id, as the
  transfer id, through crashes in [the crash test](../../../e2e/crash.ts)
  (`pnpm --filter e2e crash`): every message moves money exactly once.
