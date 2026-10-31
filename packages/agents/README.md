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
[`examples/basic/chat.ts`](../examples/basic/chat.ts):

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

Run it, from [`packages/examples/`](../examples/README.md), after
`pnpm install && pnpm build` at the repository's root
([Build your own](#build-your-own)):

```sh
OPENAI_API_KEY=… node basic/chat.ts                                          # the agent, on port 9080
npx @restatedev/restate-server  # Restate, in another terminal
npx @restatedev/restate deployments register http://localhost:9080           # tell Restate where the agent is
curl localhost:8080/assistant/alice/chat -d 'Hi, I am Alice. Is it warm in Lisbon?'
curl localhost:8080/assistant/alice/chat -d 'What is my name?'
curl localhost:8080/assistant/alice/messages                                 # the conversation so far
```

`assistant` is a virtual object: each key (`alice`, `bob`) is a
conversation of its own, with a database of its own, and answers one
message at a time. `serve` serves it on PORT (default 9080), with the
`signals` object every agent uses (its waits and steering); an endpoint of
your own (`restate.serve`, `restate.endpoint()`) binds `signals` too.
`RESTATE_SERVE_ONLY=triage,planner` serves only the services it names, so
that one program (or image) deploys in parts; `signals` too, in one of them
(`RESTATE_SERVE_ONLY=triage,planner,signals`): the agents reach it through
Restate, wherever it is.

## Choose a framework

The same agent runs on each of them: `agent({ name, ...options })`, with
the handler `chat` (a message in, the answer out), and the views
`messages` (the conversation so far), `status` (what it is doing), `stop`
and `steer` (the message it answers), `clear` (forget it all), `sync`,
`notifications` and `watch` (to follow it), and `signal` and `pending`
([Signals](#signals)).

| Import                                                | Framework                       | Choose it when                                                                                |
| ----------------------------------------------------- | ------------------------------- | --------------------------------------------------------------------------------------------- |
| [`restate-agents/pi`](src/pi/README.md)               | pi-durable                      | You want the default: any provider, coding tools on request, and its Harness (compact, reset) |
| [`restate-agents/opencode`](src/opencode/README.md)   | OpenCode 2.0, embedded          | You want a coding agent: OpenCode's own tools (read, edit, bash, grep...) in a directory      |
| [`restate-agents/langgraph`](src/langgraph/README.md) | LangGraph                       | You use LangChain: its tools, or a chat model of your own                                     |
| [`restate-agents/claude`](src/claude/README.md)       | the Anthropic SDK's tool runner | You want Claude with nothing in between: the lightest, the API's own messages                 |
| [`restate-agents/deepseek`](src/deepseek/README.md)   | DeepSeek Harness                | You want DeepSeek's harness: DeepSeek's models, or any pi-ai knows                            |

Each framework's packages are optional peer dependencies of
restate-agents: a project installs restate-agents, whose required peers
(restate-sessions, the Restate SDK and zod) npm and pnpm install with it, and
the packages of the frameworks it imports, so that `restate-agents/pi`
brings pi alone:

```sh
npm install restate-agents @earendil-works/pi-durable @earendil-works/pi-ai @earendil-works/chord   # pi
npm install restate-agents @anthropic-ai/sdk                                                        # Claude
npm install restate-agents @langchain/core @langchain/langgraph @langchain/langgraph-checkpoint \
  @langchain/openai                       # LangGraph, and the provider of its models: or @langchain/anthropic
npm install restate-agents @opencode/core@2.0.32 @opencode/plugin@2.0.32 @opencode/sdk@2.0.32 \
  @opencode/server@2.0.32 effect@4.0.1    # OpenCode, which pins its packages, and effect
npm install restate-agents @earendil-works/pi-ai @deepseek-ai/cordis \
  @deepseek-ai/{dsh-agent,dsh-agent-loop,dsh-llm,dsh-llm-deepseek-api-key,dsh-llm-pi-ai,dsh-session}@0.2.1-rc.2 \
  @deepseek-ai/{dsh-session-checkpoint-policy,dsh-session-persistence,dsh-session-projection}@0.2.1-rc.2 \
  @deepseek-ai/{dsh-system-prompt,dsh-tools}@0.2.1-rc.2   # DeepSeek
```

OpenCode is pinned: restate-agents stores its tables in the object's
database, as its version lays them out. A subpath whose packages are
missing fails as it is imported, naming the first it lacks. The packages
are not published yet:
[`pnpm --filter e2e packaging`](../e2e/packaging.ts) installs them from
their tarballs, with pi alone. In this repository the frameworks' packages
are restate-agents' development dependencies, so every example runs as it is.

Every framework takes `model`, `system`, `tools` and `output` (a
[structured answer](#structured-answers)); pi and OpenCode take a
`workspace` too, a directory for their coding tools. `system` is the whole
system prompt for Claude and LangGraph, and is added to the framework's own
(which tells the model about its tools) for pi, OpenCode and DeepSeek.

Every agent takes these too, beside its framework's options:

| Option        | What                                                                                                                                                                            |
| ------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `description` | What it is, for the UI, and for a parent's model: `subagent(agent)` tells it                                                                                                    |
| `background`  | It works unwatched (a subagent, say): its turn tells its `signals` at its end only, not at each step, so a follower sees each turn once it ended, and a steer waits for the end |
| `schema`      | Tables of your own in its database ([Build your own](#build-your-own))                                                                                                          |

## Calling an agent

Through Restate's ingress (port 8080), the request's body is the message,
as text or as a JSON string, and the response is the answer, as a JSON
string:

```sh
curl localhost:8080/assistant/alice/chat -d 'Hi, I am Alice'       # "Hello Alice! ..."
```

A key answers one message at a time: a message sent while another is
answered waits for it. A message that fails for good answers with an error:
HTTP 424 if the model's provider refused it (a missing or wrong API key, an
unknown model), 500 otherwise. A refused message is over: the next message
is answered on its own, not after it.

**Send now, fetch the answer later.** A turn can take longer than an HTTP
request should wait (a tool that waits for a person, say). Add `/send` to
send without waiting: the response has the invocation's id, and
`/restate/invocation/<id>/attach` waits for its answer, as often as you
like:

```sh
curl localhost:8080/assistant/alice/chat/send -d 'Plan my week'      # {"invocationId": "inv_1f…", ...}
curl localhost:8080/restate/invocation/inv_1f…/attach               # the answer, once there is one
```

**Stop a turn.** `stop` stops the message being answered, such as for a
UI's Stop button: it cancels its invocation (the one `status` says), and
answers its id, or null if no message was answered. To interrupt with a new
message, send it after `stop`: it is answered once the stopped turn ended.

```sh
curl localhost:8080/assistant/alice/stop                             # {"stopped": "inv_1f…"}
curl localhost:8080/assistant/alice/chat -d 'Actually, plan only Monday'
```

`stop` is Restate's cancellation, as Restate's admin API (port 9070) cancels
any invocation by its id: `curl -X PATCH localhost:9070/invocations/inv_1f…/cancel`.

The turn ends cancelled whenever the cancellation comes (during a model
request too), and the agent does not go on: each framework stops its turn
and records the cut-off tool calls as cancelled, in its own way, and the
next message starts afresh. The calls its tools made (a wait for an
approval, a sub-agent's task, a workflow) are cancelled with it. `chat`
uses Restate's explicit cancellation for this (`explicitCancellation: true`,
with `cancelled(ctx)` and `cancelCalls(ctx)` of `restate-sessions/internal`).

**Steer a turn.** `steer` adds to the message being answered, such as for
a message typed while the agent works: the agent takes it at its next tool
call, as its framework steers a turn (pi-durable's, OpenCode's and the
DeepSeek Harness's steering), and goes on with it. `steer` answers with
the invocation that answers the message (to attach to), and whether it
steers one:

```sh
curl localhost:8080/assistant/alice/steer -d 'Only Monday, please'    # {"invocationId": "inv_1f…", "steered": true}
```

A turn that ends before it takes a message (no tool call came) answers it
as a message of its own, once it ended; with no message answered, `steer`
is a message of its own (`"steered": false`, and its invocation). Claude's
and LangGraph's turns take no steering: their messages are answered next.
A stopped turn answers none of them.

A steered message waits in the agent's `signals` object (`steer`), and the
turn reads it there after each tool call (`steering`), and gives it to the
framework by its id, which the framework keeps: a retried attempt neither
loses nor repeats one.

**Forget a conversation.** `clear` starts a key over, such as for a UI's
New chat button: it stops the message being answered (as `stop`), then
forgets the conversation, the key's database, and what its
`signals` hold (waits, kept signals, steered messages). It answers as `stop`
does. The next message starts anew; its ids and cursors go on from where
the forgotten conversation got to, so a follower (`follow`, `sync`) reads on,
and drops the messages it kept from before `first` (which a page and each
change of `follow` give).

```sh
curl localhost:8080/assistant/alice/clear                            # {"stopped": null}
```

A message sent before `clear` is answered first, then forgotten too.

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
  "more": false,
  "cursor": 4
}
```

The conversation as clients read it is a log in the object's database,
which each change the agent publishes goes into. A message's `id` never
changes; `cursor` is where the log is now. `limit` caps a page; `before` an
id gives older messages (scrolling back, while `more` says there are older
ones still); and `cursor` gives the messages new or changed since it, by
when they changed, with the next cursor: a message that grew (a tool call
with its output) comes again, with its `id`. A compacted or reset
conversation keeps what was said, and the summary or handoff after it (the
model's context is what changed):

```sh
curl localhost:8080/assistant/alice/messages --json '{"limit": 20, "before": 40}'
curl localhost:8080/assistant/alice/messages --json '{"cursor": 4}'
```

It answers at once, also while a message is answered: with the tool calls
made so far.

**What it is doing.** `status` says which message the agent answers, if
any (its invocation id, to attach to its answer, and since when), and what
its tools wait for ([Signals](#signals)). The message is in the agent's
`signals` object, which `chat` tells when it starts, and when it answers or
fails for good (Restate's UI shows it there):

```sh
curl localhost:8080/support/alice/status
```

```json
{
  "answering": { "invocationId": "inv_1f…", "since": "2026-10-17T15:00:00.000Z" },
  "waiting": [
    { "id": "inv_1f…:toolu_01", "name": "approval", "input": { "order": "103" }, "since": "2026-10-17T15:00:02.000Z" }
  ]
}
```

In a tool part, `error: true` says the output is the tool's error (`Error:
<message>`), for a UI to show it as one.

**Follow it.** Instead of polling `messages` and `status`, a UI waits for
the next change with `sync`: given the `revision` of its last sync, it
answers once something changed (a message or tool call stored, a wait, a
turn's start and end), or at its timeout (50 s by default, at most 60), with
the new revision, the `status`, and the messages since its `cursor`, in one
request. It waits in Restate, holding no process; sent again with the same
idempotency key, it joins the same wait. `follow()` of
`restate-agents/client` (below) is a loop of syncs:

```sh
curl localhost:8080/support/alice/sync --json '{}'                              # {"revision": 7, "cursor": 31, "status": ..., "messages": [...], "more": false}
curl localhost:8080/support/alice/sync --json '{"cursor": 31, "revision": 7}'   # the same, once something changed
```

`notifications` and `watch({ revision })` are the revision alone, and
the wait alone. The model's reply comes whole, with its message: there is no
token streaming.

**From code.** From a Restate handler, through the agent's definition:

```ts
const answer = await ctx.rpc(assistant, 'alice').chat('Hi, I am Alice');
```

From any other code, a web app included, with Restate's own client
([`@restatedev/restate-sdk-clients`](https://docs.restate.dev/develop/ts/clients)).
`agent(name)` of `restate-agents/client` is the agent's definition as the
client takes it: its name, typed with an agent's handlers. It imports
Restate's client and nothing of the agent's code or the SDK, so a web app
bundles it as it is:

```ts
import * as clients from '@restatedev/restate-sdk-clients';
import { agent } from 'restate-agents/client';

const restate = clients.connect({ url: 'http://localhost:8080' });
const alice = restate.objectClient(agent('assistant'), 'alice');

const sent = await restate
  .objectSendClient(agent('assistant'), 'alice')
  .chat('Plan my week', clients.rpc.sendOpts({ idempotencyKey: 'msg-1' }));
const { answering, waiting } = await alice.status(); // while it works
await alice.steer('Only Monday, please'); // typed while it works
await alice.stop(); // the Stop button
const { messages } = await alice.messages({ limit: 20 }); // the tool calls so far
const answer = await restate.result(sent); // once it answered
```

`follow(restate, agent(name), key)` gives the conversation as it changes:
what the agent is doing, and the messages new or changed since the last
time (a message may come again, changed, such as a tool call with its
output: keep them by `id`), until its `signal` aborts. A network failure
does not end it, and a wait it retries joins the one already parked in
Restate:

```ts
import { agent, follow } from 'restate-agents/client';

const stop = new AbortController();
const messages = new Map<number, ChatMessage>();
for await (const change of follow(restate, agent('assistant'), 'alice', { signal: stop.signal })) {
  for (const m of change.messages) messages.set(m.id, m);
  render(change.status, [...messages.values()]);
}
```

Each change has `waiting` too, what the agent waits for, and `asText(message)`
is a message to read, as a terminal shows it: its text, and its tool calls
with their outputs.

`restate-agents/client` also has the types of what the handlers answer
(`Messages`, `Status`, `Waiting`...); `agent<H>(name)` types an object of
your own with its handlers `H`.
[`examples/ui`](../examples/ui/app.ts) is a chat page on it: the
conversation as the agent works, its approvals, steering and Stop.

## Tools

Any agent's `tools` take the tools of `restate-agents`, which work the same
in every framework, and a framework's own tools:

```ts
import { call, send, signal, subagent, tool, workflow } from 'restate-agents';
```

| Tool                                                           | What the model gets                                                                                                                                                                          |
| -------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `tool({ name, description, input, run })`                      | A tool of your own: `input` is a zod object; `run(input, { ctx, callId, idempotencyKey })` returns text or JSON, or a promise of it                                                          |
| `call(target, handler, { description, input?, key? })`         | Calls a handler of a service, virtual object or workflow, and answers with its result. Of an object, the model chooses the key, unless `key` gives it                                        |
| `send(target, handler, { description, input?, key?, delay? })` | Sends to a handler without waiting: a one-way call, after `delay` if given                                                                                                                   |
| `workflow(target, { description, input?, key? })`              | Runs a workflow to the end, and answers with its result: a workflow of its own per tool call, unless `key` gives its id                                                                      |
| `signal(name, { description, input? })`                        | Waits for a signal (a person's approval, another system's result) without holding a process, and answers with its value ([Signals](#signals))                                                |
| `subagent(agent, { description?, handler?, name? })`           | Hands a named task to another agent (of any framework), keyed `<this key>.<task>`, and answers with its answer: the same name continues the task. Its description is the agent's, by default |

Handler names are checked against the target's handlers: a typo does not
compile. A `key` is a string, or a function of the invocation's context
that gives one.

**Errors.** A `restate.TerminalError` that a tool throws, or that a handler
it calls fails with (an unknown order, say), is the tool's answer to the
model, `Error: <message>`, and the agent goes on. So are arguments that do
not fit the tool's `input` (`run` never sees them), and a call of a tool the
agent does not have: the model's to fix. Any other error fails the attempt,
and Restate retries it: the tool call runs again.

**Once.** A crash runs an unfinished tool call again with the same call id.
A tool that must not run twice passes its `idempotencyKey` on (made of the
invocation's id and the call id), as Restate's tools do with each call they
make: run again, it is the same call. OpenCode and the DeepSeek Harness tell
the model that a cut-off tool call was interrupted instead, and the model
makes it again as a new tool call: Restate's tools then make the same call,
if the input is the same (so a wait for an approval outlasts a restart), and
so is a call that answered just before the crash, until the framework has
stored its result. They keep these calls in the object's database (the
table `restate_agent_calls`) for this. A tool of your own needs a way for the model to name the call it
retries (see their READMEs).

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

Every agent (every `object()`) has the views `pending`, which lists its
waits, and `signal`, which resolves one; both answer at once, while the agent
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
`serve` serves beside the agents. See
[`examples/intermediate/support.ts`](../examples/intermediate/support.ts).

## Structured answers

With `output`, a zod object, an agent answers with data rather than text:
its reply is its answer, as JSON of the schema (which its system prompt
asks for, with the schema), and `chat` answers with the value, checked
against the schema: the answer is the call's return value, as with any
handler. Every framework takes it.

```ts
const Plan = z.object({ summary: z.string(), parts: z.array(z.string()) });
const planner = claude.agent({ name: 'planner', output: Plan });

const plan = await ctx.rpc(planner, 'ENG-1').chat('Plan the work on ENG-1'); // a Plan
```

A reply that is not such JSON (a code fence around it is fine) is answered
with what is wrong with it, once, for the model to reply again; then the
invocation fails, with a terminal error. A retried attempt resumes the
conversation, and reads the same reply. In a handler of your own, `prompt(ctx, text, {
output })` answers with it too. As a subagent's, the parent's model gets it
as JSON.

## Webhooks

`webhook()` takes another system's webhooks (Linear's, GitHub's...) into
agents: a service whose `webhook` handler checks each delivery's signature,
and delivers it to the agents of `route`, at once (the other system does not
wait for the agent):

```ts
import { serve, webhook } from 'restate-agents';

const issues = webhook({
  name: 'linear', // at /linear/webhook
  auth: { header: 'linear-signature', secret: process.env.LINEAR_WEBHOOK_SECRET! },
  idempotencyHeader: 'linear-delivery', // a delivery sent again reaches the same message
  route: [triage],
  key: ({ data }) => data.identifier, // its conversation; undefined drops it
});
await serve(issues, triage);
```

| Option              | What                                                                                         |
| ------------------- | -------------------------------------------------------------------------------------------- |
| `name`              | The service: `/<name>/webhook`                                                               |
| `auth`              | `{ header, secret }`: the HMAC-SHA256 of the body, in hex (or `sha256=…`); else 401          |
| `idempotencyHeader` | The header of the delivery's id, which a delivery sent again has too                         |
| `route`             | The agents each delivery goes to                                                             |
| `key`               | The agents' key, from the body (its JSON), or undefined to drop it; by default, the delivery |
| `message`           | What of the body the agents read; by default, all of it                                      |
| `description`       | What it is, for the UI                                                                       |

A delivery goes through the agent's `signals`, which knows what the agent is
doing: to the message it answers, if it answers one, which takes it at its
next step (as `steer`); else as a message of its own (as `chat`). A turn
that ends, or is stopped, before it takes it answers it after: a delivery is
never dropped, as a steer to a stopped turn is.

The same delivery is yours from any handler (a session object's, or a
service's or workflow's of the SDK), for events of other kinds (a queue, a
schedule): `deliver(ctx, agent, key, text, { idempotencyKey })`.
Every `object()` and `agent()` knows its companion, `companionOf(agent, key)`
(its `signals` object and key), so whoever holds an agent can reach it.

The agent's message says where it comes from, with the body as JSON:

```
<webhook name="linear" delivery="5d2…">
{ "action": "create", "type": "Issue", "data": { … } }
</webhook>
```

The handler's metadata says what it is, for the admin API and the UI:
`restate-agents.kind` (`webhook`), `.routes` (`triage`), `.auth`
(`hmac-sha256:linear-signature`) and `.idempotency-header`. See
[`examples/advanced/factory`](../examples/advanced/factory/README.md).

## Models

Every agent's `model` is `provider/model-id`, or else the `MODEL`
environment variable, or else OpenAI's `openai/gpt-6-sol`, with
`OPENAI_API_KEY` (Anthropic's `anthropic/claude-sonnet-5-5`, with
`ANTHROPIC_API_KEY`, for Claude; DeepSeek's `deepseek/deepseek-flash`, with
`DEEPSEEK_API_KEY`, for the DeepSeek Harness):

```ts
pi.agent({ name: 'assistant', model: 'anthropic/claude-sonnet-5-5' }); // with ANTHROPIC_API_KEY
```

pi, OpenCode and the DeepSeek Harness take any provider they know (the
DeepSeek Harness, pi-ai's); LangGraph OpenAI's and Anthropic's models (and
any LangChain chat model, as `model`); Claude Anthropic's.

A wrong model fails at once: a model that is not `provider/model-id`, or
another provider's for an agent of one provider's models, when the agent is
defined; a missing or wrong API key, or a model the provider does not know,
with a terminal error that says what to check, rather than retries while
the caller waits.

## Your own object

`agent()` is an object with `chat` and `messages`. For more, make the
object yourself, with `object()`, and the framework's handlers beside your
own: here, an assistant whose tools keep facts in its own table
([`examples/basic/memory.ts`](../examples/basic/memory.ts)):

```ts
import { object, serve, tool } from 'restate-agents';
import * as pi from 'restate-agents/pi';
import { z } from 'zod';

const remember = tool({
  name: 'remember',
  description: 'Remember a fact about the user.',
  input: z.object({ fact: z.string() }),
  run: async ({ fact }, { ctx }) => {
    await ctx.db.execute('INSERT OR IGNORE INTO facts VALUES (?)', [fact]);
    return 'remembered';
  },
});

const assistant = object({
  name: 'assistant',
  schema: ['CREATE TABLE facts (fact TEXT PRIMARY KEY)'], // tables of your own, beside the agent's (optional)
  handlers: {
    chat: pi.chat({ tools: [remember] }),
  },
  views: {
    messages: pi.messages(),
    facts: (ctx) => ctx.db.query('SELECT fact FROM facts'),
  },
});

await serve(assistant);
```

Your tables share the database with the agent's, whose names all start with
`restate_` (`restate_pi_tasks`, `restate_claude_messages`,
`restate_agent_log`...): any other name is yours. OpenCode's tables are in a
database of their own, beside it, as OpenCode boots only on a database that
is empty or its own.

**Options for each invocation.** `chat` takes options, or a function of the
invocation's context that gives them: each user's model and persona from
the object's database, say ([`examples/intermediate/harness.ts`](../examples/intermediate/harness.ts)):

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
`metadata` add to these. It is a `restate.sessionObject` of
[`restate-sessions`](../sessions/README.md): `handlers` run one at a time per
key, and `views` beside them. Its own cannot be named `signal`, `pending`,
`status`, `stop`, `steer`, `notifications` or `watch`, views every object
has, nor `sync`, `clear` or `erase`, which an agent has (one with a
`messages` view). A view of yours reads the database too: `ctx.db` is the
database as committed, beside a message being answered.

## Build your own

`restate-agents` is not published: it runs on
[`restate-sessions`](../sessions/README.md) (SQL in virtual objects, with its
Node.js addon), which this repository builds.
Build them once, at the repository's root, with `pnpm install && pnpm build`:
it needs Rust (for the addon), Node 24 or later, and pnpm (see
[Build and test](../../README.md#build-and-test)).

Then the quickest start is a file in [`packages/examples/`](../examples/README.md),
whose package has `restate-agents`, the SDK and zod. Or a package of your own
in the pnpm workspace: a directory under `packages/`
([`pnpm-workspace.yaml`](../../pnpm-workspace.yaml)), with

```json
{
  "type": "module",
  "dependencies": { "@restatedev/restate-sdk": "1.17.4", "restate-agents": "workspace:*", "zod": "4.6.5" }
}
```

and `pnpm install`. Node runs
TypeScript as it is: `node agent.ts`.

## Tests

[`test/`](test) has each framework's agent on a scripted model
([`test/fakes/`](test/fakes/fakes.ts)), which needs no API key, through
crashes of the handler, and each framework's storage on a database in memory,
with the framework's own conformance suite where it has one
([`test/storage/`](test/storage)). Each framework's README lists its own.

```sh
pnpm --filter restate-agents test             # all of them, without a Restate server
pnpm --filter restate-agents test langgraph   # one framework's
```

The same agents run against a real Restate server, through crashes, in
[the crash test](../e2e/README.md).

## Words

- **Object, key.** A Restate virtual object: `assistant/alice` is the
  object `assistant`'s key `alice`, with its own conversation and database,
  answering one message at a time.
- **Invocation.** One call of a handler: a message to `chat`. Restate runs
  it until it completes, through crashes and restarts.
- **Attempt.** One run of an invocation. A crash ends an attempt, and
  Restate starts another, which resumes from the database: what an
  invocation does "once" is done once however many attempts it takes.
