# restate-agents

Agent frameworks in Restate virtual objects: each object (one per key) keeps
the agent's state in its SQL database (`ctx.db`, of the patched SDK in
[`../sdk-typescript`](../sdk-typescript/SQL.md)), and each message is answered once per invocation,
however often an attempt crashes.

```ts
import { serve } from 'restate-agents';
import * as pi from 'restate-agents/pi';

const assistant = pi.agent({ name: 'assistant', tools: [weather] });
await serve(assistant);
```

| Import                                                | Framework                       | Its state in the database | `chat(options)`                         |
| ----------------------------------------------------- | ------------------------------- | ------------------------- | --------------------------------------- |
| [`restate-agents/pi`](src/pi/README.md)               | pi-durable                      | its storage               | `model`, `system`, `tools`, `workspace` |
| [`restate-agents/opencode`](src/opencode/README.md)   | OpenCode 2.0, embedded          | its database              | `model`, `system`, `tools`, `workspace` |
| [`restate-agents/langgraph`](src/langgraph/README.md) | LangGraph                       | its checkpoints           | `model`, `system`, `tools`              |
| [`restate-agents/claude`](src/claude/README.md)       | the Anthropic SDK's tool runner | the conversation          | `model`, `system`, `tools`              |
| [`restate-agents/deepseek`](src/deepseek/README.md)   | DeepSeek Harness                | its session's event log   | `model`, `system`, `tools`              |

Each framework has `agent({ name, ...options })`: a virtual object, one per
key, with the handlers `chat` (a message in, the answer out), `messages`
(the conversation so far, a page at a time), and `signal` and `pending` (see
[Tools](#tools)).

## Your own object

`chat(options)` and `messages()` are those handlers, for an object of your
own, beside handlers of yours. `object()` is `restate.object` set up for
them: `ctx.db` in every handler; retries for weeks (50 ms after a crash,
then twice as long each time, up to 10 s apart), since each attempt resumes
from the database; and the SQL storage hidden from the journal in Restate's
UI. Its `options` (such as `retryPolicy`) and `metadata` add to these. Its
own handlers cannot be named `signal` or `pending`, which every object has.

```ts
import { object } from 'restate-agents';

const assistant = object({
  name: 'assistant',
  handlers: {
    chat: pi.chat({ tools: [remember, recall] }),
    messages: pi.messages(),
    facts: async (ctx) => ctx.db.query('SELECT fact FROM facts'),
  },
});
```

A `chat` of your own, `chatHandler(answer)`, takes its message as the
agents' do (text, or a JSON string), and answers with
`answer(ctx, message)`.

See [`examples/memory.ts`](../../examples/memory.ts), and
[`examples/harness.ts`](../../examples/harness.ts) for pi's Harness
configured per user.

## Models

Every agent's `model` is `provider/model-id`, or else the `MODEL`
environment variable, or else Anthropic's `anthropic/claude-sonnet-5-5`, with
`ANTHROPIC_API_KEY` (DeepSeek's `deepseek/deepseek-flash`, with
`DEEPSEEK_API_KEY`, for the DeepSeek Harness):

```ts
pi.agent({ name: 'assistant', model: 'openai/gpt-6-luna' }); // with OPENAI_API_KEY
```

pi and OpenCode take any provider they know; Claude and LangGraph take
Anthropic's models (and LangGraph any LangChain chat model, as `model`). The
tests ([`../../tests`](../../tests/README.md)) run the agents on scripted
models.

A wrong model fails at once: a model that is not `provider/model-id`, or
another provider's for an agent of one provider's models, when the agent is
defined; a missing or wrong API key, or a model the provider does not know,
with a terminal error that says what to check, rather than retries while
the caller waits.

## Calling an agent

Through Restate's ingress, the request's body is the message (text, or a
JSON string), and the response is the answer:

```sh
curl localhost:8080/assistant/alice/chat -d 'Hi, I am Alice'
```

A caller that may send a message again (after a timeout, say) gives it an
idempotency key: the same key is the same invocation, answered once, however
often it is sent. To send without waiting, add `/send`:

```sh
curl localhost:8080/assistant/alice/chat -H 'idempotency-key: msg-1' -d 'Hi, I am Alice'
curl localhost:8080/assistant/alice/chat/send -d 'Summarize our chat later'
```

The conversation so far is `messages`, a page at a time, for a chat UI:
what was said, in order, with the tool calls the agent made and their
outputs. Without a body, it is the latest 50 messages:

```sh
curl localhost:8080/assistant/alice/messages
```

```json
{
  "messages": [
    { "id": 0, "role": "user", "parts": [{ "type": "text", "text": "Weather in Paris?" }] },
    {
      "id": 1,
      "role": "assistant",
      "parts": [
        { "type": "tool", "id": "toolu_01", "name": "weather", "input": { "city": "Paris" }, "output": "18°C, sunny" }
      ]
    },
    { "id": 2, "role": "assistant", "parts": [{ "type": "text", "text": "18°C and sunny." }] }
  ],
  "more": false
}
```

A message's `id` is its position in the conversation. `limit` caps a page;
`before` an id gives older messages (scrolling back, while `more` says there
are older ones still), and `after` an id newer ones (polling for what came
since):

```sh
curl localhost:8080/assistant/alice/messages --json '{"limit": 20, "before": 40}'
curl localhost:8080/assistant/alice/messages --json '{"after": 59}'
```

It reads the framework's own storage, and is exclusive: it waits for a
message being answered, and does not show a turn while it runs.

From a Restate handler, through the agent's definition:

```ts
const answer = await ctx.objectClient(assistant, 'alice').chat('Hi, I am Alice');
```

From any other code, with Restate's client
([`@restatedev/restate-sdk-clients`](https://docs.restate.dev/develop/ts/clients)):

```ts
import * as clients from '@restatedev/restate-sdk-clients';

const restate = clients.connect({ url: 'http://localhost:8080' });
const answer = await restate.objectClient(assistant, 'alice').chat('Hi, I am Alice');
```

## Tools

Any agent's `tools` take a framework's own tools, and the tools of
`restate-agents`, which work the same in every framework. Tools for each
invocation come from `chat(async (ctx) => ({ tools }))`. A crash runs an unfinished tool
call again with the same call id, which a tool that must not run twice
passes on (as an idempotency key, say):

```ts
import { call, send, signal, subagent, tool, workflow } from 'restate-agents';
```

| Tool                                                           | What the model gets                                                                                                                                                  |
| -------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `tool({ name, description, input, run })`                      | A tool of your own: `input` is a zod object, `run(input, { ctx, callId })` returns text or JSON                                                                      |
| `call(target, handler, { description, input?, key? })`         | Calls a handler of a service, virtual object or workflow, and answers with its result. Of an object, the model chooses the key, unless `key` gives it                |
| `send(target, handler, { description, input?, key?, delay? })` | Sends to a handler without waiting: a one-way call, after `delay` if given                                                                                           |
| `workflow(target, { description, input? })`                    | Runs a workflow to the end, a workflow of its own per tool call, and answers with its result                                                                         |
| `signal(name, { description, input? })`                        | Waits for a signal (a person's approval, another system's result) without holding a process, and answers with its value; `input` is what the model says it waits for |
| `subagent(agent, { description?, handler? })`                  | Hands a named task to another agent (of any framework), keyed `<this key>.<task>`, and answers with its answer: the same name continues the task                     |

Handler names are checked against the target's handlers: a typo does not
compile.

A tool whose handler fails with a `restate.TerminalError` (an unknown
order, say) answers the model with `Error: <message>`, and the agent goes
on; any other error fails the attempt, which Restate retries.

Each Restate call has an idempotency key made of the invocation's id and the
model's tool call id: a crash runs an unfinished tool call again, and it
makes the same call, once. On the frameworks that tell the model a cut-off
tool call was interrupted instead (OpenCode, the DeepSeek Harness), the
model makes it again as a new tool call: with the same input, it is the
same Restate call, which it waits for again. So a wait for an approval
outlasts a suspension (after Restate's inactivity timeout), which restarts
the handler.

Every agent (every `object()`) has `pending`, which lists its waits, and a
`signal` handler, which resolves one; both run while the agent waits. Each
wait has an id, the signal's name, the model's `input` (such as the refund
to approve) and when it started:

```sh
curl localhost:8080/support/alice/pending
```

```json
[
  {
    "id": "inv_1f…:toolu_01",
    "name": "approval",
    "input": { "order": "103", "amount": 30 },
    "since": "2026-10-17T15:00:00.000Z"
  }
]
```

`signal` resolves the wait of an `id`, or else the oldest wait for a
`name`, and answers with the id it resolved, or `null` if none waited. A
signal no one waits for is dropped, so a stale approval approves nothing
later; with `"keep": true` it is kept for the next wait for its name:

```sh
curl localhost:8080/support/alice/signal --json '{"id": "inv_1f…:toolu_01", "value": "approved"}'
curl localhost:8080/support/alice/signal --json '{"name": "approval", "value": "approved", "keep": true}'
```

A wait ends with its agent's invocation: a cancelled one leaves `pending`.

Underneath, signals are kept by the `signals` object, keyed
`<agent>:<key>`; `serve(...services)` serves the agents with it, on PORT
(default 9080). See [`examples/support.ts`](../../examples/support.ts).
