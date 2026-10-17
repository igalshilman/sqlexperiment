# Streaming answers: a design

Status: proposal, for the owner to decide (see [Decisions](#decisions)).

## Goal

A chat UI shows the answer while the model writes it, not only once the
turn ends: the reply's text as it streams, and which tool runs. It does so
with Restate's own client and nothing else by default, on all five
frameworks, without weakening what a turn guarantees (answered once, through
crashes).

## Where we are

A UI polls `status` and `messages` (see [`examples/ui`](../../../examples/ui/app.ts)).
`messages` reads the agent's database as stored, beside the running turn.

What a reader sees of a running turn, measured against restate-server with a
handler that writes a row every 100 ms and a shared handler reading it every
200 ms:

| The turn writes                            | A reader sees it        |
| ------------------------------------------ | ----------------------- |
| `ctx.db` or `ctx.set`, and awaits plain JS | only once the turn ends |
| the same, then awaits a Restate operation  | 30 to 110 ms later      |

The SDK sends a turn's writes when the turn awaits a Restate operation (a
call, a `ctx.get`, a signal's wait). So today a turn shows progress at those
points only: at each tool call (a Restate tool's call, or the steering read
since #108), and not during a model's reply, however long. A turn without
tool calls shows nothing until it answers.

## Constraints

- Restate's ingress answers an invocation once, with its result: a handler
  has no way to send partial output to its caller.
- The live stream exists only in the turn's process: each framework emits it
  there (pi-durable's `watchEvents` `text_delta`, the Claude tool runner's
  `stream`, LangGraph's `streamMode: 'messages'`, OpenCode's
  `session.text.*`, the DeepSeek Harness's `text-delta`), and none stores
  it.
- A reader is another invocation (a shared handler), maybe in another
  process: it sees only what the turn sent to Restate.
- Tokens need no durability. The transcript (`messages`) is the record; a
  stream that misses a piece, or a turn retried after a crash, is caught up
  by the transcript.

## Options

**A. A draft in the object's state, polled.** The turn keeps the reply being
written in its object's state (`restate-agents.draft`: its text so far), and
flushes it at most every ~100 ms (an awaited Restate operation). `status`
returns it with the message being answered; the UI polls `status` while the
agent works (~250 ms) and renders the draft after the transcript. The
framework's commit of the message clears it.

**B. A draft, watched.** As A, and a shared handler `watch(version)` that
answers when the draft (or the transcript) is newer than `version`: it waits
on an awakeable, which the turn resolves with each flush (one call to the
agent's `signals` object, which is also the flush). A UI calls `watch` in a
loop instead of polling: each change arrives in tens of ms, and an idle
agent costs nothing.

**C. Your own channel.** An option `stream: (ctx, event) => void` on every
framework's `chat`, called in the turn's process with one event type for
all five (`{ type: 'text', delta }`, `{ type: 'tool', name, state }`). The
app sends it on: a WebSocket or SSE of its own, a pub/sub it runs. Restate
sees none of it.

|                       | A: polled draft                                                         | B: watched draft                                     | C: your own channel               |
| --------------------- | ----------------------------------------------------------------------- | ---------------------------------------------------- | --------------------------------- |
| Latency               | flush + poll: ~100–350 ms                                               | flush + a call: ~50–150 ms                           | as fast as the channel            |
| Infrastructure        | none                                                                    | none                                                 | the app's                         |
| Restate's load        | a state write and flush per 100 ms of reply, a read per poll per viewer | the same writes, a call per flush, a wait per viewer | none                              |
| Restate's client only | yes                                                                     | yes                                                  | no                                |
| After a crash         | the draft restarts with the retry                                       | the same                                             | the app's stream restarts         |
| Code in this repo     | the five streams → one draft                                            | A, and `watch`                                       | the five streams → one event type |

All three need the same first step: each framework's stream mapped to one
event type, in its turn's process. A and B are built on it; C exposes it.

## Proposal

1. **One event type, in the turn's process.** Each framework's live stream
   mapped to `{ type: 'text', delta } | { type: 'tool', name, state }`,
   tested per framework with its fake model streaming in pieces.
2. **A, by default.** The draft in state, flushed at most every 100 ms;
   `status` gives `answering.draft`; [`examples/ui`](../../../examples/ui/app.ts)
   polls `status` faster while the agent works, and renders the draft. No
   option needed; a turn without a viewer pays the writes, which are small.
3. **C, as the escape hatch.** `stream: (ctx, event) => void` on `chat` and
   `prompt`, for an app that already has a channel to its users, documented
   with a short SSE example.
4. **B, later, if polling costs too much**: it changes only how the UI
   learns of a change, so it can follow without changing A.

Also, apart from streaming: the same flush after each framework's commit
would make `messages` show a turn's tool calls as they are stored, not at
the turn's next Restate operation.

## Cost, roughly

A reply of 20 s, flushed every 100 ms: at most 200 state writes and 200
flushes, each a small journal entry; a viewer polling every 250 ms makes 80
reads of `status` (journaled, no database). A turn's tool calls and its
commits are unchanged. The flush interval is the knob: 250 ms halves it,
and still reads as live.

## Decisions

1. Is polling (A), at ~100–350 ms, good enough to start, with B later? Or
   should the first version watch (B)?
2. Text only, or also the model's thinking (`thinking_delta`, where the
   framework has it) and tool progress?
3. Is the flush's cost (above) acceptable by default, or should streaming be
   an option of `chat` (`stream: true`)?
