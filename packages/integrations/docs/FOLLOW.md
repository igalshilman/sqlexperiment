# Following a conversation: a design

Status: built. Decided: no token streaming; `follow()` on Restate's client.

## Goal

A chat UI sees what the agent does as it happens: each message, tool call
and result once it is stored, a wait for approval, the turn's end. It does
so with Restate's client, without polling, from any number of browsers,
and a client that reconnects catches up. The conversation (`messages`) stays
the one source of truth.

Token streaming (the reply's text while the model writes it) is not part of
it: see [Tokens](#tokens).

## Where we started

A UI polled `status` and `messages` once a second, whether or not anything
changed (it now follows: see
[`examples/ui`](../../examples/ui/app.ts)).

And a reader saw a running turn's progress late. Measured against
restate-server, with a handler that writes a row every 100 ms and a shared
handler that reads it:

| The turn writes                            | A reader sees it        |
| ------------------------------------------ | ----------------------- |
| `ctx.db` or `ctx.set`, and awaits plain JS | only once the turn ends |
| the same, then awaits a Restate operation  | 30 to 110 ms later      |

The SDK sends a turn's writes when the turn awaits a Restate operation. So
`messages` shows a tool call when the turn next calls Restate (its steering
read, since #108), and a turn without tool calls shows nothing until it
answers.

## The reference: restatedev/agent

[restatedev/agent](https://github.com/restatedev/agent)'s client follows a
conversation this way (its `docs/protocol.md`, "History and notifications"):

- Each agent has a **revision**, which every change of what a client reads
  increases (`publish`): its history, approvals, profile, schedules.
- `watch({ revision, timeoutSeconds })` answers once there is a revision
  newer than `revision`, or at the timeout: it waits on an awakeable, which
  `publish` resolves, in a window of at most a minute. A client that retries
  a window keeps its idempotency key, so it joins the wait already parked in
  Restate.
- Notifications carry no data: they tell the client to read again. The
  client's `follow()` reads the revision, then the history from a cursor,
  then watches, in a loop.
- No token streaming: its model calls are journaled whole. Its AG-UI
  endpoint (an SSE stream for frontends such as CopilotKit) is a view of the
  history, read the same way.

## Proposal

The same, on what we have:

1. **A revision, in the agent's `signals` object** (keyed `<agent>:<key>`,
   which already keeps its waits and steering), with its subscribers:
   `publish` increases it and resolves each subscriber behind it.
2. **`watch` and `notifications`, handlers every object has** (beside
   `status`, `stop`, `steer`): `notifications()` is `{ revision }`, and
   `watch({ revision, timeoutSeconds })` answers `{ revision }` once it
   is newer, or at the timeout (at most 60 s).
3. **The turn publishes where its conversation changes:** when it starts and
   ends, before and after each tool call (the framework's wrappers, where it
   reads steering now), when a tool waits for a signal and when one resolves
   it, and when a message steers it. Each `publish` is a call to Restate, so
   it also sends the turn's writes: a reader sees the tool call it announces
   (which fixes the delay above).
4. **`follow()` in `restate-agents/client`**: on Restate's client, an async
   iterator of `{ status, messages, cursor }` (each new or changed message,
   and the status), from a cursor, until its signal aborts: a loop of
   `sync`s (below). A network failure retries, a window's retry keeps its
   idempotency key.
   [`examples/ui`](../../examples/ui/app.ts) uses it instead of polling.

What a client then does:

```ts
const stop = new AbortController();
for await (const { status, messages } of follow(restate, agent('support'), 'alice', { signal: stop.signal })) {
  render(status, messages);
}
```

### A log, read with a cursor, and `sync`

As the reference keeps a public history apart from the model's context, the
conversation as clients read it is a log in the object's database
(`restate_agent_log`, `log.ts`), which each publish takes the framework's
transcript into (`transcribing` in `common.ts`, through the framework as the
turn holds it open):

- Each message has an `id` that never changes, and a `seq`, its last
  change's sequence number: the cursor. A message that grows (a tool call
  with its output) keeps its `id` and takes a new `seq`; a transcript that
  is replaced (compacted, reset) leaves its messages in the log, and the ones
  after them are new.
- `clear` forgets the log, not where it got to (`restate-agents.log.start`
  in the agent's state): the next conversation's ids and cursors go on from
  there, and a page gives `first`, its first id, so a follower drops what it
  kept from before.
- `messages({ cursor })` gives what is new or changed since a cursor, and the
  next one; `messages({ limit, before })` a page by `id`, and the cursor to
  follow on from.
- `sync({ cursor, revision, timeoutSeconds })` is `watch`, then the revision,
  the status and the messages since the cursor, in one request: as the
  reference's web app has `snapshot` and `sync` for a page and its long poll.

### Cost

One call to `signals` per publish (a turn with three tool calls publishes
about ten times), and one parked invocation per watching client, which costs
nothing while it waits. An agent nobody watches pays the publishes only.

## Tokens

Neither the reference nor this proposal streams tokens. A turn's model call
answers whole, and the text arrives with its message. If a UI needs tokens
later, they belong on a channel of the app's own (a WebSocket, SSE, a
pub/sub), fed from the turn's process: each framework has the stream there
(pi-durable's `watchEvents`, the Claude tool runner's `stream`, LangGraph's
`streamMode: 'messages'`, OpenCode's `session.text.*`, the DeepSeek
Harness's `text-delta`). That would be an option of `chat` (a callback with
one event type for the five), not something kept in Restate.

## Decisions

1. Is following the conversation (messages and status, as they are stored)
   enough for now, without tokens?
2. `follow()` in `restate-agents/client`, on Restate's client (its
   `objectClient`, our `agent()` types): or a client of our own, as the
   reference has (`createAgentClient`)?
