# SQL objects and agents on Restate

Two layers, the second on the first:

1. **[Objects with SQL](#objects-with-sql)**: Restate virtual objects whose
   handlers run without journal replay, so a retried attempt runs again
   against the state as stored, each key with its own SQLite-compatible
   database, stored in the object's state
   ([`restate-objects`](packages/objects/README.md)).
2. **[AI agents](#ai-agents)** in those objects, their conversation in the
   object's database: pi, OpenCode, LangGraph, Claude and more
   ([`restate-agents`](packages/agents/README.md)).

New to them? [The tutorial](packages/template/README.md) goes one concept per
file, from a virtual object to agents that delegate.

## Objects with SQL

Each key of a virtual object has a SQLite database of its own, up to 10 GiB,
kept in the object's state: no database to run. `restate-objects` is the
Restate SDK with that in its objects
([`examples/basic/notes.ts`](packages/examples/basic/notes.ts)):

```ts
import * as restate from 'restate-objects';

const notes = restate.object({
  name: 'notes',
  // The schema of each key's database: its steps, applied once per key (a step added later runs then).
  schema: ['CREATE TABLE notes (text TEXT NOT NULL)'],
  handlers: {
    add: async (ctx: restate.ObjectContext, note: string) => {
      await ctx.db.execute('INSERT INTO notes (text) VALUES (?)', [note]);
    },

    /** Shared: it reads while `add` writes; a key with no notes yet has none. */
    list: restate.handlers.object.shared(async (ctx: restate.ObjectSharedContext) => {
      const rows = await ctx.db.query('SELECT text FROM notes ORDER BY rowid');
      return rows.map(([text]) => text);
    }),
  },
});

await restate.serve({ services: [notes], port: 9080 });
```

```sh
curl localhost:8080/notes/alice/add --json '"Buy milk"'
curl localhost:8080/notes/alice/list
```

A statement is durable when it returns. `ctx.transaction` groups
statements, and the sends made in it, into one commit. See
[`restate-objects`](packages/objects/README.md).

## AI agents

An agent is an object with SQL: its conversation is in its own database, so
a crash, or a manager who takes a day, changes nothing. This one's tools are
Restate itself: it looks an order up with a service, waits for a manager's
approval, and refunds with a workflow
([`examples/intermediate/support.ts`](packages/examples/intermediate/support.ts)):

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

The same agent runs on pi, OpenCode, LangGraph, Claude (the Anthropic SDK)
or the DeepSeek Harness: see [`restate-agents`](packages/agents/README.md)
and [the examples](packages/examples/README.md).

### Tools of its own

A tool is a function with the object's `ctx`. This assistant keeps what it
learns about you in a table of its own SQL database, next to its
conversation, and a handler of yours serves that table, for a profile page
say ([`examples/basic/memory.ts`](packages/examples/basic/memory.ts)):

```ts
import { handlers } from 'restate-objects';
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
    const rows = await ctx.db.query('SELECT fact FROM facts ORDER BY rowid');
    return rows.map(([fact]) => fact);
  },
});

const assistant = object({
  name: 'assistant',
  // A table of your own, beside the agent's: the facts, in each user's database.
  schema: ['CREATE TABLE facts (fact TEXT PRIMARY KEY)'],
  handlers: {
    chat: pi.chat({
      system: 'You are a helpful assistant. Remember what you learn about the user, and recall it when it helps.',
      tools: [remember, recall],
    }),
    messages: pi.messages(),

    /** What the assistant knows about the user: shared, so it answers while a message is answered. */
    facts: handlers.object.shared(async (ctx) => {
      const rows = await ctx.db.query('SELECT fact FROM facts ORDER BY rowid');
      return rows.map(([fact]) => fact);
    }),
  },
});

await serve(assistant);
```

```sh
curl localhost:8080/assistant/alice/chat -d 'I am vegetarian, and I live in Lisbon.'
curl localhost:8080/assistant/alice/chat -d 'Suggest a restaurant for tonight.'
curl localhost:8080/assistant/alice/facts
```

## Where things are

A pnpm workspace ([`pnpm-workspace.yaml`](pnpm-workspace.yaml)) of five
packages, each with its README, and its tests in its `test/` directory:

- [`packages/objects`](packages/objects/README.md): `restate-objects`, SQL in
  virtual objects (`ctx.db`, `ctx.transaction`) on the published SDK, with
  its storage in a Rust addon, [`native/`](packages/objects/native/README.md):
  [the layers](packages/objects/native/README.md#inside-the-layers), from
  Restate's key/value store up to `ctx.db`.
- [`packages/agents`](packages/agents/README.md): `restate-agents`,
  pi, OpenCode, LangGraph, Claude and the DeepSeek Harness in virtual objects.
- [`packages/template`](packages/template/README.md): a tutorial, one concept
  per file: a virtual object, then SQL in it, then agents.
- [`packages/examples`](packages/examples/README.md): the examples, each a
  small file you run with node.
- [`packages/e2e`](packages/e2e/README.md): the tests against a real Restate
  server: the agents and SQL objects through crashes, the context without
  journal replay, and the SQL benchmark.

## Requirements

|                | Version                                                                    |
| -------------- | -------------------------------------------------------------------------- |
| Restate server | 1.8 or later                                                               |
| TypeScript SDK | 1.17.4, as published, with [`restate-objects`](packages/objects/README.md) |

```sh
docker run --rm -p 8080:8080 -p 9070:9070 --add-host=host.docker.internal:host-gateway \
  ghcr.io/restatedev/restate:1.8.0
```

## Build and test

Node 24 or later (the examples and tests are TypeScript that node runs as it
is, and OpenCode needs 24), pnpm, and Rust for the addon:

```sh
pnpm install
pnpm build        # the addon, with the state and SQL inside
pnpm test         # every package's tests, which need no Restate server
pnpm lint         # tsc, oxlint, oxfmt --check, rustfmt and clippy
pnpm format
```

`pnpm --filter <package> test` runs one package's tests (add a filter, such
as `langgraph`). The `e2e` package's scripts run against a Restate server:
the 1.8.0 release that `@restatedev/restate-server` installs, or the binary
`RESTATE_SERVER` names.

```sh
pnpm --filter restate-agents test langgraph   # one framework's agent tests
pnpm --filter e2e crash                       # the agents and SQL objects through crashes
pnpm --filter e2e concurrency                 # the context without journal replay
pnpm --filter e2e tools                       # Restate's own agent tools
pnpm --filter e2e bench                       # SQL in objects: latency, throughput and stored bytes
```

To run an example, see [Run an example](packages/examples/README.md#run-an-example).
