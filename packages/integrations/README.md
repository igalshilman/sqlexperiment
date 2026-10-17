# restate-agents

Agent frameworks in Restate virtual objects: each object (one per key) keeps
the agent's state in its SQL database (`ctx.db`, of the patched SDK in
[`../sdk`](../sdk/SQL.md)), and each message is answered once per invocation,
however often an attempt crashes.

```ts
import { serve } from 'restate-agents';
import * as pi from 'restate-agents/pi';

const assistant = pi.agent({ name: 'assistant', tools: [weather] });
await serve(assistant);
```

| Import                                                | Framework                       | Its state in the database | `chat(options)`               |
| ----------------------------------------------------- | ------------------------------- | ------------------------- | ----------------------------- |
| [`restate-agents/pi`](src/pi/README.md)               | pi-durable                      | its storage               | `model`, `tools`, `workspace` |
| [`restate-agents/opencode`](src/opencode/README.md)   | OpenCode 2.0, embedded          | its database              | `model`, `tools`, `directory` |
| [`restate-agents/langgraph`](src/langgraph/README.md) | LangGraph                       | its checkpoints           | `model`, `tools`              |
| [`restate-agents/claude`](src/claude/README.md)       | the Anthropic SDK's tool runner | the conversation          | `model`, `system`, `tools`    |
| [`restate-agents/deepseek`](src/deepseek/README.md)   | DeepSeek Harness                | its session's event log   | `model`, `system`, `tools`    |

Each framework has `agent({ name, ...options })`, a virtual object with one
handler, `chat`, which takes a message and returns the answer; and
`chat(options)`, that handler, for an object of your own. `object()`, of
`restate-agents`, is `restate.object` with SQL set up: `ctx.db` in every
handler, retries without end (each attempt resumes from the database), and
the SQL storage hidden from the journal in Restate's UI; its `options` and
`metadata` add to these.
A tool may be given as itself, or as a function of the invocation's context,
`(ctx) => tool`, to reach other services through it. A tool that must not run
twice passes its tool call id on (as an idempotency key, say): a crash runs
an unfinished tool call again, with the same id.

The models are real ones. Every agent's `model` is `provider/model-id`, or
else the `MODEL` environment variable, or else Anthropic's
`anthropic/claude-sonnet-5-5`, with `ANTHROPIC_API_KEY` (DeepSeek's
`deepseek/deepseek-flash`, with `DEEPSEEK_API_KEY`, for the DeepSeek
Harness). pi and OpenCode take any provider they know (such as
`openai/gpt-6-luna`, with `OPENAI_API_KEY`); Claude and LangGraph take
Anthropic's models (or LangGraph, any LangChain chat model). The tests ([`../../tests`](../../tests/README.md)) run
them on scripted models instead.

## Tools

Any agent's `tools` take a framework's own tools, and the tools of
`restate-agents`, which work the same in every framework:

```ts
import { call, send, signal, subagent, tool, workflow } from 'restate-agents';
```

| Tool                                                           | What the model gets                                                                                                                                   |
| -------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------- |
| `tool({ name, description, input, run })`                      | A tool of your own: `input` is a zod object, `run(input, { ctx, callId })` returns text or JSON                                                       |
| `call(target, handler, { description, input?, key? })`         | Calls a handler of a service, virtual object or workflow, and answers with its result. Of an object, the model chooses the key, unless `key` gives it |
| `send(target, handler, { description, input?, key?, delay? })` | Sends to a handler without waiting: a one-way call, after `delay` if given                                                                            |
| `workflow(target, { description, input? })`                    | Runs a workflow to the end, a workflow of its own per tool call, and answers with its result                                                          |
| `signal(name, { description })`                                | Waits for a signal (a person's approval, another system's result) without holding a process, and answers with its value                               |
| `subagent(agent, { description?, handler? })`                  | Hands a named task to another agent (of any framework), keyed `<this key>.<task>`, and answers with its answer: the same name continues the task      |

Handler names are checked against the target's handlers: a typo does not
compile.

A tool whose handler fails with a `restate.TerminalError` (an unknown
order, say) answers the model with `Error: <message>`, and the agent goes
on; any other error fails the attempt, which Restate retries.

Each Restate call has an idempotency key made of the invocation's id and the
model's tool call id: a crash runs an unfinished tool call again, and it
makes the same call, once.

Every agent (every `object()`) has a `signal` handler, which resolves a
signal, and `pending`, which lists what it waits for; both run while the
agent waits. A signal resolved before the agent waits is kept for it:

```sh
curl localhost:8080/support/alice/pending
curl localhost:8080/support/alice/signal --json '{"name": "approval", "value": "approved"}'
```

Underneath, signals are kept by the `signals` object, keyed
`<agent>:<key>`; `serve(...services)` serves the agents with it, on PORT
(default 9080). See [`examples/support.ts`](../../examples/support.ts).
