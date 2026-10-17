# Claude

The Anthropic SDK's tool runner (`client.beta.messages.toolRunner` in
[`@anthropic-ai/sdk`](https://github.com/anthropics/anthropic-sdk-typescript))
is a light agent harness: the tool-calling loop, in the process, with nothing
stored. It keeps the conversation in its params, so an agent built on it is
durable when the conversation is: here it is in the agent's SQL database
([SQL in Restate virtual objects](../../../sdk-typescript/SQL.md)).

It is lighter than the Claude Agent SDK, which runs Claude Code as a
subprocess. Claude Code's own tools (bash, read, write, edit, glob, grep)
come to the runner as `betaAgentToolset20260401` from
`@anthropic-ai/sdk/tools/agent-toolset/node`; the agent takes whatever tools
you give it.

## Use

```ts
import { serve } from 'restate-agents';
import * as claude from 'restate-agents/claude';

const assistant = claude.agent({ name: 'assistant', system: 'Answer briefly.', tools: [weather] });
await serve(assistant);
```

| Option   | What                                                                                                   |
| -------- | ------------------------------------------------------------------------------------------------------ |
| `model`  | `anthropic/model-id`; or else `MODEL`, or else `anthropic/claude-sonnet-5-5` (`ANTHROPIC_API_KEY`)     |
| `system` | The system prompt                                                                                      |
| `tools`  | `tool()`s and Restate's tools of `restate-agents`, or the SDK's runnable tools (such as `betaZodTool`) |

For an object of your own, `claude.chat(options)` and `claude.messages()`
are the handlers.

## Through crashes

The conversation ([`conversation.ts`](conversation.ts)) is the messages in
the object's database, in order, each with the invocation it belongs to, and
the answer to each. The handler is not replayed: a retried attempt runs it
again from the start, with the stored messages.

- **Each reply is stored before its tools run, and their results before the
  next request.** So a crash during the tools resumes them with the same tool
  call ids, without asking the model again.
- **A tool is idempotent by its tool call id** (`callId` of `tool()`'s
  `run`, or `callId(context)` for the SDK's tools), which it passes on, such
  as in an idempotency key.
- **A tool's error fails the attempt**, and Restate retries it; a
  `restate.TerminalError` of a `restate-agents` tool is its answer to the
  model.
- **A message is answered once per invocation**: its answer is stored with
  the model's last reply, and a retried attempt returns it. A message that a
  crash interrupted is finished before the next one starts.

## Run

```sh
cd examples && ANTHROPIC_API_KEY=… node claude.ts
```

With a Restate server, and the agent registered with it: see
[Run an example](../../../../examples/README.md#run-an-example).

## Tests

The tests run on a scripted model, [`tests/fakes/claude.ts`](../../../../tests/fakes/claude.ts)
(`make test-ts T=claude`, from the repository root):

- [`tests/agents/claude.test.ts`](../../../../tests/agents/claude.test.ts):
  each point above, with crashes that stop an attempt in a tool call, or at
  any of its state requests, and abandon it, keeping what it committed; and
  `messages`.
- [`tests/agents/claude.ts`](../../../../tests/agents/claude.ts): the bank
  agent, whose tools move money in the [ledger](../../../../tests/sql/ledger.ts),
  through crashes in [the crash test](../../../../tests/crash.ts)
  (`make crash-test`): every message moves money exactly once.
