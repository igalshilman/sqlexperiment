# restate-agents

Agent frameworks in Restate virtual objects: each object (one per key) keeps
the agent's state in its SQL database (`ctx.db`, of the patched SDK in
[`../sdk`](../sdk/SQL.md)), and each message is answered once per invocation,
however often an attempt crashes.

```ts
import * as restate from '@restatedev/restate-sdk';
import * as pi from 'restate-agents/pi';

const session = restate.object({ name: 'session', handlers: { chat: pi.chat() }, options: restate.sqlOptions });
await restate.serve({ services: [session] });
```

| Import                                                | Framework                       | Its state in the database | `chat(options)`                                                                                           |
| ----------------------------------------------------- | ------------------------------- | ------------------------- | --------------------------------------------------------------------------------------------------------- |
| [`restate-agents/pi`](src/pi/README.md)               | pi-durable                      | its storage               | `model`, `tools`, `workspace`; and `subagent(object, handler)`, a tool that hands a task to another agent |
| [`restate-agents/opencode`](src/opencode/README.md)   | OpenCode 2.0, embedded          | its database              | `model`, `tools`, `directory`                                                                             |
| [`restate-agents/langgraph`](src/langgraph/README.md) | LangGraph                       | its checkpoints           | `model`, `tools`                                                                                          |
| [`restate-agents/claude`](src/claude/README.md)       | the Anthropic SDK's tool runner | the conversation          | `model`, `system`, `tools`                                                                                |
| [`restate-agents/deepseek`](src/deepseek/README.md)   | DeepSeek Harness                | its session's event log   | `model`, `system`, `tools`                                                                                |

`chat(options)` makes a handler, `(ctx, message: string) => Promise<string>`.
A tool may be given as itself, or as a function of the invocation's context,
`(ctx) => tool`, to reach other services through it. A tool that must not run
twice passes its tool call id on (as an idempotency key, say): a crash runs
an unfinished tool call again, with the same id.

The models are real ones: by default Anthropic's `claude-sonnet-5-5`, with
`ANTHROPIC_API_KEY` (DeepSeek's `deepseek-flash`, with `DEEPSEEK_API_KEY`, for
the DeepSeek Harness). The tests ([`../../tests`](../../tests/README.md)) run
them on scripted models instead.
