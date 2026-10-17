# restate-agents

Durable agents on Restate: an agent of one of five frameworks (pi, OpenCode,
LangGraph, Claude, the DeepSeek Harness) in a Restate virtual object, its
conversation in the object's own SQL database. A crash, a restart or a
person who takes a day to approve changes nothing: each message is answered
once, and each tool call is made once.

- [Start here](#start-here)
- [Choose a framework](#choose-a-framework)
- [Calling an agent](#calling-an-agent)
- [Tools](#tools)
- [Signals](#signals)
- [Models](#models)
- [Your own object](#your-own-object)
- [Build your own](#build-your-own)
- [Words](#words)

## Start here

A chat agent with a tool, the whole of
[`examples/chat.ts`](../../examples/chat.ts):

```ts
import { serve, tool } from 'restate-agents';
import * as pi from 'restate-agents/pi';
import { z } from 'zod';

const weather = tool({
  name: 'get_weather',
  description: 'The current weather in a city.',
  input: z.object({ city: z.string() }),
  run: ({ city }) => `Sunny and 21°C in ${city}`, // a stand-in for a real weather API
});

const assistant = pi.agent({ name: 'assistant', tools: [weather] });

await serve(assistant);
```

Run it, from [`examples/`](../../examples/README.md), after `make js` at the
repository's root ([Build your own](#build-your-own)):

```sh
ANTHROPIC_API_KEY=… node chat.ts                                     # the agent, on port 9080
npx @restatedev/restate-server                                        # Restate, in another terminal
npx @restatedev/restate deployments register http://localhost:9080   # tell Restate where the agent is
curl localhost:8080/assistant/alice/chat -d 'Hi, I am Alice. Is it warm in Lisbon?'
curl localhost:8080/assistant/alice/chat -d 'What is my name?'
curl localhost:8080/assistant/alice/messages                          # the conversation so far
```

`assistant` is a virtual object: each key (`alice`, `bob`) is a
conversation of its own, with a database of its own, and answers one
message at a time. `serve` serves it on PORT (default 9080).

## Choose a framework

The same agent runs on each of them: `agent({ name, ...options })`, with
the handlers `chat` (a message in, the answer out) and `messages` (the
conversation so far), and `signal` and `pending` ([Signals](#signals)).

| Import                                                | Framework                       | Choose it when                                                                                |
| ----------------------------------------------------- | ------------------------------- | --------------------------------------------------------------------------------------------- |
| [`restate-agents/pi`](src/pi/README.md)               | pi-durable                      | You want the default: any provider, coding tools on request, and its Harness (compact, reset) |
| [`restate-agents/opencode`](src/opencode/README.md)   | OpenCode 2.0, embedded          | You want a coding agent: OpenCode's own tools (read, edit, bash, grep...) in a directory      |
| [`restate-agents/langgraph`](src/langgraph/README.md) | LangGraph                       | You use LangChain: its tools, or a chat model of your own                                     |
| [`restate-agents/claude`](src/claude/README.md)       | the Anthropic SDK's tool runner | You want Claude with nothing in between: the lightest, the API's own messages                 |
| [`restate-agents/deepseek`](src/deepseek/README.md)   | DeepSeek Harness                | You want DeepSeek's models, on DeepSeek's harness                                             |

Every framework takes `model`, `system` and `tools`; pi and OpenCode take a
`workspace` too, a directory for their coding tools. `system` is the whole
system prompt for Claude and LangGraph, and is added to the framework's own
(which tells the model about its tools) for pi, OpenCode and DeepSeek.

## Calling an agent

Through Restate's ingress (port 8080), the request's body is the message,
as text or as a JSON string, and the response is the answer, as a JSON
string:

```sh
curl localhost:8080/assistant/alice/chat -d 'Hi, I am Alice'       # "Hello Alice! ..."
```

A key answers one message at a time: a message sent while another is
answered waits for it.

**Send now, fetch the answer later.** A turn can take longer than an HTTP
request should wait (a tool that waits for a person, say). Add `/send` to
send without waiting: the response has the invocation's id, and
`/restate/invocation/<id>/attach` waits for its answer, as often as you
like:

```sh
curl localhost:8080/assistant/alice/chat/send -d 'Plan my week'      # {"invocationId": "inv_1f…", ...}
curl localhost:8080/restate/invocation/inv_1f…/attach               # the answer, once there is one
```

**Send again safely.** A caller that may send a message again (after a
timeout, say) gives it an idempotency key: the same key is the same
invocation, answered once, however often it is sent. Its answer is also at
the key:

```sh
curl localhost:8080/assistant/alice/chat/send -H 'idempotency-key: msg-1' -d 'Plan my week'
curl localhost:8080/restate/invocation/assistant/alice/chat/msg-1/attach
```

**The conversation** is `messages`, a page at a time, for a chat UI: what
was said, in order, with the tool calls the agent made and their outputs.
Without a body, it is the latest 50 messages:

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

It reads the framework's own storage, and waits, as a message does, for a
message being answered.

**From code.** From a Restate handler, through the agent's definition:

```ts
const answer = await ctx.objectClient(assistant, 'alice').chat('Hi, I am Alice');
```

From any other code, with Restate's client (`npm install
@restatedev/restate-sdk-clients`, [its docs](https://docs.restate.dev/develop/ts/clients)):

```ts
import * as clients from '@restatedev/restate-sdk-clients';

const restate = clients.connect({ url: 'http://localhost:8080' });
const answer = await restate.objectClient(assistant, 'alice').chat('Hi, I am Alice');
```

## Tools

Any agent's `tools` take the tools of `restate-agents`, which work the same
in every framework, and a framework's own tools:

```ts
import { call, send, signal, subagent, tool, workflow } from 'restate-agents';
```

| Tool                                                           | What the model gets                                                                                                                                   |
| -------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------- |
| `tool({ name, description, input, run })`                      | A tool of your own: `input` is a zod object; `run(input, { ctx, callId, idempotencyKey })` returns text or JSON, or a promise of it                   |
| `call(target, handler, { description, input?, key? })`         | Calls a handler of a service, virtual object or workflow, and answers with its result. Of an object, the model chooses the key, unless `key` gives it |
| `send(target, handler, { description, input?, key?, delay? })` | Sends to a handler without waiting: a one-way call, after `delay` if given                                                                            |
| `workflow(target, { description, input?, key? })`              | Runs a workflow to the end, and answers with its result: a workflow of its own per tool call, unless `key` gives its id                               |
| `signal(name, { description, input? })`                        | Waits for a signal (a person's approval, another system's result) without holding a process, and answers with its value ([Signals](#signals))         |
| `subagent(agent, { description?, handler?, name? })`           | Hands a named task to another agent (of any framework), keyed `<this key>.<task>`, and answers with its answer: the same name continues the task      |

Handler names are checked against the target's handlers: a typo does not
compile. A `key` is a string, or a function of the invocation's context
that gives one.

**Errors.** A `restate.TerminalError` that a tool throws, or that a handler
it calls fails with (an unknown order, say), is the tool's answer to the
model, `Error: <message>`, and the agent goes on. Any other error fails the
attempt, and Restate retries it: the tool call runs again.

**Once.** A crash runs an unfinished tool call again with the same call id.
A tool that must not run twice passes its `idempotencyKey` on (made of the
invocation's id and the call id), as Restate's tools do with each call they
make: run again, it is the same call. OpenCode and the DeepSeek Harness tell
the model that a cut-off tool call was interrupted instead, and the model
makes it again as a new tool call: Restate's tools then make the same call,
if the input is the same (so a wait for an approval outlasts a restart); a
tool of your own needs a way for the model to name the call it retries
(see their READMEs).

**Tools for each invocation** (each user's, say) come with the options of
`chat`: `pi.chat(async (ctx) => ({ tools: await toolsOf(ctx) }))`.

## Signals

A `signal()` tool waits for a value from outside: a person's approval, say.
With an `input`, the model says what it waits for:

```ts
signal('approval', {
  description: 'Wait for a manager to approve a refund.',
  input: z.object({ order: z.string(), amount: z.number() }),
});
```

Every agent (every `object()`) has `pending`, which lists its waits, and a
`signal` handler, which resolves one; both answer at once, while the agent
waits. Each wait has an id, the signal's name, the model's `input` and when
it started:

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

`signal` resolves the wait of an `id`, or else the oldest wait for a `name`,
and answers `{"resolved": <its id>}`, or `{"resolved": null}` if none
waited. A signal no one waits for is dropped, so a stale approval approves
nothing later; with `"keep": true` it is kept for the next wait for its
name:

```sh
curl localhost:8080/support/alice/signal --json '{"id": "inv_1f…:toolu_01", "value": "approved"}'
curl localhost:8080/support/alice/signal --json '{"name": "approval", "value": "approved", "keep": true}'
```

A wait ends with its agent's invocation: a cancelled one leaves `pending`.
The waits are kept by the `signals` object, keyed `<agent>:<key>`, which
`serve` serves beside the agents; an endpoint of your own (`restate.serve`,
`restate.endpoint()`) binds `signals` too. See
[`examples/support.ts`](../../examples/support.ts).

## Models

Every agent's `model` is `provider/model-id`, or else the `MODEL`
environment variable, or else Anthropic's `anthropic/claude-sonnet-5-5`, with
`ANTHROPIC_API_KEY` (DeepSeek's `deepseek/deepseek-flash`, with
`DEEPSEEK_API_KEY`, for the DeepSeek Harness):

```ts
pi.agent({ name: 'assistant', model: 'openai/gpt-6-luna' }); // with OPENAI_API_KEY
```

pi and OpenCode take any provider they know; Claude and LangGraph take
Anthropic's models (and LangGraph any LangChain chat model, as `model`).

A wrong model fails at once: a model that is not `provider/model-id`, or
another provider's for an agent of one provider's models, when the agent is
defined; a missing or wrong API key, or a model the provider does not know,
with a terminal error that says what to check, rather than retries while
the caller waits.

## Your own object

`agent()` is an object with `chat` and `messages`. For more, make the
object yourself, with `object()`, and the framework's handlers beside your
own: here, an assistant whose tools keep facts in its own table
([`examples/memory.ts`](../../examples/memory.ts)):

```ts
import type * as restate from '@restatedev/restate-sdk';
import { object, serve, tool } from 'restate-agents';
import * as pi from 'restate-agents/pi';
import { z } from 'zod';

const facts = async (ctx: restate.SqlObjectContext) => {
  await ctx.db.execute('CREATE TABLE IF NOT EXISTS facts (fact TEXT PRIMARY KEY)');
  return ctx.db;
};

const remember = tool({
  name: 'remember',
  description: 'Remember a fact about the user.',
  input: z.object({ fact: z.string() }),
  run: async ({ fact }, { ctx }) => {
    await (await facts(ctx)).execute('INSERT OR IGNORE INTO facts VALUES (?)', [fact]);
    return 'remembered';
  },
});

const assistant = object({
  name: 'assistant',
  handlers: {
    chat: pi.chat({ tools: [remember] }),
    messages: pi.messages(),
    facts: async (ctx) => (await facts(ctx)).query('SELECT fact FROM facts'),
  },
});

await serve(assistant);
```

**Options for each invocation.** `chat` takes options, or a function of the
invocation's context that gives them: each user's model and persona from
the object's database, say ([`examples/harness.ts`](../../examples/harness.ts)):

```ts
chat: pi.chat(async (ctx) => ({ model: (await settingsOf(ctx)).model, tools: [clock] })),
```

**A handler of your own.** `prompt(ctx, text, options)` answers a prompt in
any handler, once, however often the handler runs; each prompt of an
invocation is a prompt of its own. `chatHandler(answer)` makes a handler
that takes its message as `chat` does (text, or a JSON string):

```ts
summarize: chatHandler(async (ctx, topic) => pi.prompt(ctx, `Summarize what we said about ${topic}`)),
```

pi and OpenCode also have `withPi(ctx, options, work)` and
`withOpenCode(ctx, options, work)`, for more than a prompt: the Harness's own
API, such as compacting the conversation (see their READMEs).

**What `object()` sets up.** `ctx.db` in every handler; retries for weeks
(50 ms after a crash, then twice as long each time, up to 10 s apart), since
each attempt resumes from the database; and the SQL storage hidden from the
journal in Restate's UI. Its `options` (such as `retryPolicy`) and
`metadata` add to these. Its own handlers cannot be named `signal` or
`pending`, which every object has.

## Build your own

`restate-agents` is not published: it runs on a Restate SDK with
[patches](../sdk-typescript/README.md) (SQL in virtual objects), which this
repository builds. Build it once, at the repository's root, with `make js`:
it needs Rust (with the `wasm32-unknown-unknown` target and `wasm-bindgen`
0.2.121), Node 24 or later, and pnpm; `nix develop` has them all (see
[Build and test](../../README.md#build-and-test)).

Then the quickest start is a file in [`examples/`](../../examples/README.md),
whose package has `restate-agents`, the SDK and zod. Or a package of your own
in the pnpm workspace: a directory listed in
[`pnpm-workspace.yaml`](../../pnpm-workspace.yaml), with

```json
{
  "type": "module",
  "dependencies": { "@restatedev/restate-sdk": "1.17.3", "restate-agents": "workspace:*", "zod": "4.6.5" }
}
```

and `pnpm install` (the workspace swaps in the patched SDK). Node runs
TypeScript as it is: `node agent.ts`.

## Words

- **Object, key.** A Restate virtual object: `assistant/alice` is the
  object `assistant`'s key `alice`, with its own conversation and database,
  answering one message at a time.
- **Invocation.** One call of a handler: a message to `chat`. Restate runs
  it until it completes, through crashes and restarts.
- **Attempt.** One run of an invocation. A crash ends an attempt, and
  Restate starts another, which resumes from the database: what an
  invocation does "once" is done once however many attempts it takes.
