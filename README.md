# SQL objects and agents on Restate

Two layers, the second on the first:

1. **[Objects with SQL](#objects-with-sql)**: Restate virtual objects whose
   handlers run without journal replay, so a retried attempt runs again
   against the state as stored, each key with its own SQLite-compatible
   database, stored in the object's state
   ([`restate-fns`](typescript/packages/fns/README.md)).
2. **[AI agents](#ai-agents)** in those objects, their conversation in the
   object's database: pi, OpenCode, LangGraph, Claude and more
   ([`restate-agents`](typescript/packages/integrations/README.md)).

## Objects with SQL

Each key of a virtual object has a SQLite database of its own, up to 10 GiB,
kept in the object's state: no database to run. `restate-fns` is the Restate SDK with
that in its objects ([`examples-typescript/basic/notes.ts`](examples-typescript/basic/notes.ts)):

```ts
import * as restate from 'restate-fns';

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
[`restate-fns`](typescript/packages/fns/README.md).

## AI agents

An agent is an object with SQL: its conversation is in its own database, so
a crash, or a manager who takes a day, changes nothing. This one's tools are
Restate itself: it looks an order up with a service, waits for a manager's
approval, and refunds with a workflow
([`examples-typescript/intermediate/support.ts`](examples-typescript/intermediate/support.ts)):

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
or the DeepSeek Harness: see [`restate-agents`](typescript/packages/integrations/README.md)
and [the examples](examples-typescript/README.md).

### Tools of its own

A tool is a function with the object's `ctx`. This assistant keeps what it
learns about you in a table of its own SQL database, next to its
conversation, and a handler of yours serves that table, for a profile page
say ([`examples-typescript/basic/memory.ts`](examples-typescript/basic/memory.ts)):

```ts
import { handlers } from 'restate-fns';
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

Each part is a directory with its README, a workspace of its own; the
[`Makefile`](Makefile) builds and tests them all.

- [`core/`](core): the storage every SDK builds on: the
  [`state`](core/state/README.md) (records, paged files and logs, changed
  through a mutation and flushed atomically, in one round trip) and
  [`sql`](core/sql/README.md) on it; [the layers](core/README.md#layers), from
  Restate's key/value store up to the SDKs.
- [`typescript/`](typescript/README.md): the Node.js addon,
  [`restate-fns`](typescript/packages/fns/README.md) (SQL in virtual objects, on the
  published SDK) and the [`restate-agents`](typescript/packages/integrations/README.md) integrations.
- [`examples-typescript/`](examples-typescript/README.md): the examples.

## Requirements

|                | Version                                                                       |
| -------------- | ----------------------------------------------------------------------------- |
| Restate server | 1.8 or later                                                                  |
| TypeScript SDK | 1.17.4, as published, with [`restate-fns`](typescript/packages/fns/README.md) |

```sh
docker run --rm -p 8080:8080 -p 9070:9070 --add-host=host.docker.internal:host-gateway \
  ghcr.io/restatedev/restate:1.8.0
```

## Build and test

With [Nix](https://nixos.org) (flakes), `nix develop` gives a shell with every
tool below, on macOS and Linux ([`flake.nix`](flake.nix)):

```sh
nix develop       # or `direnv allow`, with use flake in .envrc
make              # build everything: the crates, the addon and the pnpm workspace
make test         # every test that needs no Restate server
make test-ts T=langgraph   # the TypeScript tests (vitest in typescript/tests), optionally filtered
make crash-test         # the integrations and SQL objects through crashes, against a Restate server
make concurrency-test   # the context without journal replay, against a Restate server
make help
```

Without Nix: besides Rust, the Node.js parts need node 24 or later (the
examples and tests are TypeScript that node runs as it is, and OpenCode needs 24) and pnpm, which installs [their workspace](typescript/README.md) (`make js`).

The tests against a Restate server download its 1.8.0 release on first use
(see the [`Makefile`](Makefile): `RESTATE_VERSION`,
`RESTATE_FROM`, `RESTATE_IMAGE`, `RESTATE_SERVER`).
