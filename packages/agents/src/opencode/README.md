# OpenCode

[OpenCode](https://opencode.ai) 2.0 can run embedded: `@opencode/sdk` hosts
the whole OpenCode server in your process, with no HTTP listener. Here it
runs in the agent's virtual object, and OpenCode's database (sessions,
messages, the inbox, durable events) is an SQL database of the key's own,
in the object's state beside `ctx.db` (`databaseOf` of restate-fns): OpenCode
boots only on a database that is empty or its own, so its tables are alone
there, and `ctx.db` keeps the conversation's log and the tables of yours
([SQL in Restate virtual objects](../../../sql/README.md)). The integration
pins OpenCode 2.0.27 exactly, with the Effect release it is built on (4.0.1);
OpenCode needs Node 24.

## Use

```ts
import { serve } from 'restate-agents';
import * as opencode from 'restate-agents/opencode';

const coder = opencode.agent({ name: 'coder', workspace: '/work/coder' });
await serve(coder);
```

| Option      | What                                                                                                                                                                                                                                                   |
| ----------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `model`     | `provider/model-id`, of any provider OpenCode serves; or else `MODEL`, or else `openai/gpt-6-sol` (`OPENAI_API_KEY`)                                                                                                                                   |
| `system`    | The system prompt                                                                                                                                                                                                                                      |
| `tools`     | `tool()`s and Restate's tools of `restate-agents`, or OpenCode tools of your own (`OpenCodeTool`)                                                                                                                                                      |
| `output`    | A zod object: the answer as data, which the agent replies with as JSON, and `chat` answers with (see [Structured answers](../../README.md#structured-answers))                                                                                         |
| `workspace` | A directory of the local machine to work in, with OpenCode's own tools (read, write, edit, bash, glob, grep...), or a function of the context that gives one (`(ctx) => \`/work/${ctx.key}\``); use a sandbox. Without one, the agent has only `tools` |
| `plugins`   | OpenCode plugins of your own (`@opencode/plugin`), such as one that denies edits outside `src/` ([`examples/intermediate/review.ts`](../../../examples/intermediate/review.ts))                                                                        |

For an object of your own, `opencode.chat(options)` and
`opencode.messages()` are the handlers; `options` may be a function of the
invocation's context, such as a directory for each key:

```ts
chat: opencode.chat((ctx) => ({ workspace: `/work/${ctx.key}` })),
```

`opencode.prompt(ctx, text, options)` answers a prompt in a handler of your
own. For more than a prompt, `withOpenCode(ctx, options, work)` runs `work`
with OpenCode: `prompt(text)` answers a prompt once, and `host` is OpenCode's
client.

## Through crashes

[`db.ts`](db.ts) (`restate-agents/opencode/db`) boots OpenCode's embedded
host on its database: an Effect `SqlClient` through
`Database.configuredClient`, OpenCode's seam for an injected client, each
OpenCode transaction an SQL transaction. The handler is not replayed: a
retried attempt boots OpenCode again, on the stored database, and OpenCode
resumes interrupted work on its own: it records an execution claim when a
turn starts, and resumes every claimed turn when it boots.

- **Restate decides how often a turn is retried.** OpenCode fails a turn
  after ten resumes; here `openOpenCode` replaces its restart service with
  one without a limit, and the object's retry policy decides.
- **A tool runs only once its call is stored.** OpenCode stores the model's
  reply, with its tool calls, before the tools run.
- **A tool call that was running has an interrupted outcome.** Resuming gives
  it an "interrupted" result, and asks the model to continue without
  repeating completed work. A tool that must not run twice needs a way for a
  retry to name the call it retries (the bank agent's `transfer` takes
  `retryOf`). Restate's own tools
  (`call`, `send`, `workflow`, `subagent`, `signal`) need none: made again
  with the same input, the call is the one the cut-off call made, which
  matters for a long one, such as a wait for an approval, since a suspension
  (after Restate's inactivity timeout) restarts the handler too.
- **A tool's error fails the attempt**, and Restate retries it; a
  `restate.TerminalError` of a `restate-agents` tool is its answer to the
  model. The failed attempt stores nothing more, so its retry finds the call
  interrupted, as above.
- **A message is answered once per invocation**: the invocation id is its
  OpenCode message id, and a retried attempt returns the answer of the turn
  that answered it. A prompt that a crash interrupted is finished before the
  next one starts.

## Notes

- **Embedded.** OpenCode runs in the handler's own process: its client calls
  the server's HTTP router through an in-process `fetch`. Without a
  `workspace` it spawns nothing (the workerd profile's shell, terminal, file
  and version control services are disabled); with one, its own tools run on
  the local machine. The only other traffic out is the model's, to its
  provider's API.
- **The Promise API's `OpenCode.create(options)` ignores its second
  argument** (the layer replacements), though its types declare it; the
  Effect API's `create` honours it.
- **A prompt waits in the inbox before it is a message**, and a prompt
  queued while a turn runs is answered in that turn's run. So a new prompt
  waits for an unfinished one.
- **Cost.** A prompt makes some hundreds of SQL statements; booting OpenCode
  takes about 100 ms per attempt, after a one-time import of about 2 s per
  process.

## Run

```sh
cd packages/examples && OPENAI_API_KEY=… node basic/opencode.ts
```

With a Restate server, and the agent registered with it: see
[Run an example](../../../examples/README.md#run-an-example).
[`examples/intermediate/coding.ts`](../../../examples/intermediate/coding.ts) has OpenCode coders,
each with a workspace, under a pi coordinator.

## Tests

The tests run on a scripted model, [`test/fakes/opencode.ts`](../../test/fakes/opencode.ts),
an AI SDK language model that OpenCode calls through a plugin's `language`
hook, and makes a fresh tool call id for every call (`pnpm --filter restate-agents test opencode`,
from the repository root):

- [`test/storage/opencode.test.ts`](../../test/storage/opencode.test.ts):
  the Effect `SqlClient` of `db.ts`.
- [`test/opencode.test.ts`](../../test/opencode.test.ts):
  each point above, with crashes that stop an attempt in a tool call, or
  at any of its state requests; the
  system prompt; and `messages`. With a transfer tool that ignores `retryOf`,
  four of its tests fail.
- [`test/opencode.ts`](../../test/opencode.ts): the
  bank agent, through crashes in [the crash test](../../../e2e/crash.ts)
  (`pnpm --filter e2e crash`): every message moves money exactly once.
