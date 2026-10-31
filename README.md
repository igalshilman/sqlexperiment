# Session objects and agents on Restate

This repository adds two libraries to the [Restate](https://restate.dev)
TypeScript SDK:

- **[`restate-sessions`](packages/sessions/README.md)** gives every key of a
  virtual object its own SQL database. We call these _session objects_, or
  _sessions_ for short. The database lives in Restate's state, so there is no
  separate database to run.
- **[`restate-agents`](packages/agents/README.md)** runs AI agents inside
  sessions, with each conversation stored in its session's database. It
  supports pi, OpenCode, LangGraph, Claude and the DeepSeek Harness.

If you are new to this, start with [the tutorial](packages/template/README.md).
It builds a wallet one idea per file, from a plain virtual object up to an
assistant that pays your friends and asks before large transfers.

## Session objects

A session object is a virtual object with a `schema` and a database,
`ctx.db`. Each key (`alice`, `bob`, …) gets its own SQLite-compatible
database of up to 10 GiB. This one keeps notes per user
([`examples/basic/notes.ts`](packages/examples/basic/notes.ts)):

```ts
import * as restate from 'restate-sessions';

const notes = restate.sessionObject({
  name: 'notes',
  // Creates each key's database. Each statement runs once per key; append new ones later as needed.
  schema: ['CREATE TABLE notes (text TEXT NOT NULL)'],
  handlers: {
    add: async (ctx, note: string) => {
      await ctx.db.execute('INSERT INTO notes (text) VALUES (?)', [note]);
    },
  },
  views: {
    /** A view only reads, so it can run while `add` is writing. */
    list: async (ctx) => {
      const rows = await ctx.db.query<{ text: string }>('SELECT text FROM notes ORDER BY rowid');
      return rows.map(({ text }) => text);
    },
  },
});

await restate.serve({ services: [notes], port: 9080 });
```

```sh
curl localhost:8080/notes/alice/add --json '"Buy milk"'
curl localhost:8080/notes/alice/list
```

Handlers run one at a time per key, and they are the only ones that write.
Views can run at any time, even while a handler is running, and only read
what has been committed.

Each statement is durable once it returns. To commit several statements at
once, put them in `ctx.db.transaction(tx => …)`. Messages you send with
`tx.send` inside a transaction go out only if it commits.

Session handlers don't replay a journal after a failure. Restate simply runs
the handler again from the start, against whatever the database holds by
then. That means a handler should be safe to run twice, for example by
recording a request or transfer id with its writes. The
[`restate-sessions` README](packages/sessions/README.md) covers this, and
how the storage works.

## Agents

An agent is a session whose conversation is stored in its database. If the
process crashes, or a tool waits a day for a person to answer, the agent
picks up where it left off.

Tools can be Restate itself. This support agent looks up orders with a
service, waits for a manager to approve a refund, and runs the refund as a
workflow ([`examples/intermediate/support.ts`](packages/examples/intermediate/support.ts)):

```ts
const support = pi.agent({
  name: 'support',
  tools: [
    call(orders, 'status', {
      description: 'The status of an order.',
      input: z.object({ order: z.string() }),
    }),
    signal('approval', {
      description: "Wait for a manager's decision on a refund.",
    }),
    workflow(refund, {
      description: 'Refund an order.',
      input: z.object({ order: z.string(), amount: z.number() }),
    }),
  ],
});

await serve(orders, refund, support);
```

```sh
curl localhost:8080/support/alice/chat -d 'Order 103 arrived broken, can I get my $30 back?'
curl localhost:8080/support/alice/signal --json '{"name": "approval", "value": "approved"}'
```

The same agent runs on any of the supported frameworks. See
[`restate-agents`](packages/agents/README.md) and
[the examples](packages/examples/README.md).

### Your own tools and tables

A tool is a function that gets the session's `ctx`, so it can use the same
database as the agent. This assistant stores what it learns about each user
in a `facts` table next to the conversation, and exposes the table through a
view that a profile page could call
([`examples/basic/memory.ts`](packages/examples/basic/memory.ts)). It is
defined with `object` from `restate-agents`, which is `restate.sessionObject`
plus the handlers every agent has (`status`, `stop`, `signal` and a few
more):

```ts
import { object, serve, tool } from 'restate-agents';
import * as pi from 'restate-agents/pi';
import { z } from 'zod';

const remember = tool({
  name: 'remember',
  description: 'Remember a fact about the user, for this conversation and later ones.',
  input: z.object({ fact: z.string() }),
  run: async ({ fact }, { ctx }) => {
    await ctx.db.execute('INSERT OR IGNORE INTO facts (fact) VALUES (?)', [fact]);
    return `Remembered: ${fact}`;
  },
});

const recall = tool({
  name: 'recall',
  description: 'Everything remembered about the user.',
  input: z.object({}),
  run: async (_input, { ctx }) => {
    const rows = await ctx.db.query<{ fact: string }>('SELECT fact FROM facts ORDER BY rowid');
    return rows.map(({ fact }) => fact);
  },
});

const assistant = object({
  name: 'assistant',
  // Your own table, in each user's database next to the agent's tables.
  schema: ['CREATE TABLE facts (fact TEXT PRIMARY KEY)'],
  handlers: {
    chat: pi.chat({
      system: 'You are a helpful assistant. Remember what you learn about the user, and recall it when it helps.',
      tools: [remember, recall],
    }),
  },
  views: {
    messages: pi.messages(),

    /** What the assistant knows about the user. A view, so it answers even while the agent is busy. */
    facts: async (ctx) => {
      const rows = await ctx.db.query<{ fact: string }>('SELECT fact FROM facts ORDER BY rowid');
      return rows.map(({ fact }) => fact);
    },
  },
});

await serve(assistant);
```

```sh
curl localhost:8080/assistant/alice/chat -d 'I am vegetarian, and I live in Lisbon.'
curl localhost:8080/assistant/alice/chat -d 'Suggest a restaurant for tonight.'
curl localhost:8080/assistant/alice/facts
```

## Repository layout

This is a pnpm workspace ([`pnpm-workspace.yaml`](pnpm-workspace.yaml)) with
five packages. Each has its own README, and its unit tests in `test/`.

- [`packages/sessions`](packages/sessions/README.md): `restate-sessions`.
  The storage is a Rust addon in
  [`native/`](packages/sessions/native/README.md), whose README walks through
  [the layers](packages/sessions/native/README.md#inside-the-layers) from
  Restate's key/value state up to `ctx.db`.
- [`packages/agents`](packages/agents/README.md): `restate-agents`, with an
  integration per framework.
- [`packages/template`](packages/template/README.md): the tutorial.
- [`packages/examples`](packages/examples/README.md): small examples, each a
  single file you run with Node.
- [`packages/e2e`](packages/e2e/README.md): tests against a real Restate
  server (crashes, concurrency, Restate's own agent tools, a benchmark), and
  a check that the packages install and work as a user would get them.

## Requirements

- Restate server 1.8 or later.
- The published TypeScript SDK, version 1.17.4. `restate-sessions` pins it,
  because it relies on some of the SDK's internals
  ([why](packages/sessions/README.md#concurrent-awaits)).

To run Restate in Docker:

```sh
docker run --rm -p 8080:8080 -p 9070:9070 --add-host=host.docker.internal:host-gateway \
  ghcr.io/restatedev/restate:1.8.0
```

## Build and test

You need Node 24 or later (the code is TypeScript that Node runs directly,
and OpenCode requires 24), pnpm, and Rust for the addon.

```sh
pnpm install
pnpm build        # builds the Rust addon
pnpm test         # unit tests of every package; no Restate server needed
pnpm lint         # tsc, oxlint, oxfmt --check, rustfmt and clippy
pnpm format
```

`pnpm --filter <package> test` runs a single package's tests, and you can
add a filter such as `langgraph`. The `e2e` scripts start a Restate server
themselves: the 1.8.0 release from `@restatedev/restate-server`, or the binary
in `RESTATE_SERVER` if you set it.

```sh
pnpm --filter restate-agents test langgraph   # one framework's agent tests
pnpm --filter e2e crash                       # agents and sessions through crashes
pnpm --filter e2e concurrency                 # concurrent awaits without journal replay
pnpm --filter e2e tools                       # Restate's own agent tools
pnpm --filter e2e bench                       # latency, throughput and storage size
```

To run an example, see [Run an example](packages/examples/README.md#run-an-example).

### Packaging

Inside the workspace, Node runs each package's TypeScript sources directly.
The published packages are compiled to ESM JavaScript with type declarations
in `dist/` (`pnpm --filter <package> build:ts`).

The addon ships as one npm package per platform
(`restate-sessions-<platform>`, in `packages/sessions/npm`), and
`restate-sessions` lists them as optional dependencies, so a package manager
installs only the one for its machine. CI builds them for Linux (x64 and
arm64, glibc 2.35 or later) and macOS on Apple silicon, and installs the
packed packages on each
([`packages.yml`](.github/workflows/packages.yml)). `restate-agents` lists
each framework as an optional peer dependency, so a project installs only
the frameworks it uses ([which ones](packages/agents/README.md#choose-a-framework)).

```sh
pnpm --filter restate-sessions build:dist   # the addon as published: smaller and fully optimized (takes minutes)
pnpm --filter e2e packaging                 # pack, lint and install both packages into a fresh project
```
