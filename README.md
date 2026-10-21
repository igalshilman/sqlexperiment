# Durable functions on Restate

An experiment in three layers, each on the one before:

1. **[Objects with SQL](#objects-with-sql)**: Restate virtual objects whose
   handlers run without journal replay, so a retried attempt runs again
   against the state as stored, each key with its own SQLite-compatible
   database, stored in the object's state
   ([`restate-fns`](typescript/packages/fns/README.md)).
2. **[AI agents](#ai-agents)** in those objects, their conversation in the
   object's database: pi, OpenCode, LangGraph, Claude and more
   ([`restate-agents`](typescript/packages/integrations/README.md)).
3. **[Durable functions](#durable-functions)**: plain JavaScript or Python,
   with no SDK, whose memory is saved while they wait
   ([`rust/functions`](rust/functions/README.md)).

## Objects with SQL

Each key of a virtual object has a SQLite database of its own, kept in the
object's state: no database to run. `restate-fns` is the Restate SDK with
that in its objects ([`examples-typescript/notes.ts`](examples-typescript/notes.ts)):

```ts
import * as restate from 'restate-fns';

const SCHEMA = 'CREATE TABLE IF NOT EXISTS notes (text TEXT NOT NULL)';

const notes = restate.object({
  name: 'notes',
  handlers: {
    add: async (ctx: restate.ObjectContext, note: string) => {
      await ctx.db.execute(SCHEMA);
      await ctx.db.execute('INSERT INTO notes (text) VALUES (?)', [note]);
    },

    list: async (ctx: restate.ObjectContext) => {
      await ctx.db.execute(SCHEMA);
      const rows = await ctx.db.query('SELECT text FROM notes ORDER BY rowid');
      return rows.map(([text]) => text);
    },
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
[`restate-fns`](typescript/packages/fns/README.md), and
[`examples-python/notes.py`](examples-python/notes.py) for the same in Python
([`restate_fns`](python/fns/README.md)).

## AI agents

An agent is an object with SQL: its conversation is in its own database, so
a crash, or a manager who takes a day, changes nothing. This one's tools are
Restate itself: it looks an order up with a service, waits for a manager's
approval, and refunds with a workflow
([`examples-typescript/support.ts`](examples-typescript/support.ts)):

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
and [the examples](examples-typescript/README.md). In Python, it runs on
Claude, LangGraph or Pydantic AI ([`restate_agents`](python/agents/README.md),
[the examples](examples-python/README.md)).

### Tools of its own

A tool is a function with the object's `ctx`. This assistant keeps what it
learns about you in a table of its own SQL database, next to its
conversation, and a handler of yours serves that table, for a profile page
say ([`examples-typescript/memory.ts`](examples-typescript/memory.ts)):

```ts
import { object, serve, tool } from 'restate-agents';
import * as pi from 'restate-agents/pi';
import { z } from 'zod';

const SCHEMA = 'CREATE TABLE IF NOT EXISTS facts (fact TEXT PRIMARY KEY)';

const remember = tool({
  name: 'remember',
  description: 'Remember a fact about the user, for this conversation and later ones.',
  input: z.object({ fact: z.string() }),
  run: async ({ fact }, { ctx }) => {
    await ctx.db.execute(SCHEMA);
    await ctx.db.execute('INSERT OR IGNORE INTO facts (fact) VALUES (?)', [fact]);
    return `Remembered: ${fact}`;
  },
});

const recall = tool({
  name: 'recall',
  description: 'Everything remembered about the user.',
  input: z.object({}),
  run: async (_input, { ctx }) => {
    await ctx.db.execute(SCHEMA);
    const rows = await ctx.db.query('SELECT fact FROM facts ORDER BY rowid');
    return rows.map(([fact]) => fact);
  },
});

const assistant = object({
  name: 'assistant',
  handlers: {
    chat: pi.chat({
      system: 'You are a helpful assistant. Remember what you learn about the user, and recall it when it helps.',
      tools: [remember, recall],
    }),
    messages: pi.messages(),

    /** What the assistant knows about the user. */
    facts: async (ctx) => {
      await ctx.db.execute(SCHEMA);
      const rows = await ctx.db.query('SELECT fact FROM facts ORDER BY rowid');
      return rows.map(([fact]) => fact);
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

## Durable functions

Plain JavaScript or Python, with no SDK: each file is a virtual object, and
each function it exports is a handler. While a call waits, on a timer,
another service or a model, its memory is saved: variables, closures, loops
in progress. After a crash it carries on where it stopped; nothing is
replayed. In this saga, the undo steps are closures in an array, and a crash
between two steps loses none of them
([`examples-functions/checkout.js`](examples-functions/checkout.js)):

```js
export async function pay({ customer, items }, ctx) {
  const done = await ctx.kv.get('outcome');
  if (done) return done;
  let outcome;
  const inventory = (sku, handler, qty) => ctx.rpc.call({ service: 'inventory', handler, key: sku }, qty);
  const undo = [];
  try {
    for (const { sku, qty } of items) {
      await inventory(sku, 'reserve', qty);
      undo.push(() => inventory(sku, 'release', qty));
    }
    const total = items.reduce((sum, { qty, price }) => sum + qty * price, 0);
    await ctx.rpc.call({ service: 'payments', handler: 'charge', key: customer }, total);
    outcome = { status: 'paid', total };
  } catch (error) {
    for (const step of undo.reverse()) await step();
    outcome = { status: 'cancelled', reason: error.message };
  }
  await ctx.kv.set('outcome', outcome);
  return outcome;
}
```

```sh
make functions && target/release/functions   # every function in examples-functions/, on port 9080
curl localhost:9070/deployments --json '{"uri": "http://localhost:9080"}'
curl localhost:8080/inventory/widget/restock --json 5
curl localhost:8080/payments/alice/deposit --json 100
curl localhost:8080/checkout/order-1/pay --json '{"customer": "alice", "items": [{"sku": "widget", "qty": 2, "price": 10}]}'
# {"status":"paid","total":20}
curl localhost:8080/checkout/order-2/pay --json '{"customer": "alice", "items": [{"sku": "widget", "qty": 3, "price": 30}]}'
# {"status":"cancelled","reason":"Error: Declined: balance 80, charge 90"}, and the 3 widgets back in stock
```

The [functions](examples-functions/README.md) include timers, retries,
deadlines, fan-out and AI agents; the
[same in Python](examples-functions-python/README.md) runs on CPython 3.15
in WebAssembly. How it works: [`rust/functions`](rust/functions/README.md).

## Where things are

Each part is a directory with its README, a workspace of its own; the
[`Makefile`](Makefile) builds and tests them all.

- [`core/`](core): the storage every SDK builds on: the
  [`journal`](core/journal/README.md) (records and paged files, committed in
  one put) and [`sql`](core/sql/README.md) on it.
- [`typescript/`](typescript/README.md): the Node.js addon,
  [`restate-fns`](typescript/packages/fns/README.md) (SQL in virtual objects, on the
  published SDK) and the [`restate-agents`](typescript/packages/integrations/README.md) integrations.
- [`python/`](python/README.md): the same for Python, with
  [`restate-fns`](python/fns/README.md) on the published SDK.
- [`rust/`](rust/README.md): the patched Rust SDK, and
  [`functions`](rust/functions/README.md), the runtime for durable functions.
- `examples-<part>/`: the examples, one directory per part:
  [TypeScript](examples-typescript/README.md), [Python](examples-python/README.md),
  [functions](examples-functions/README.md),
  [functions in Python](examples-functions-python/README.md).

## Requirements

|                | Version                                                                                                   |
| -------------- | --------------------------------------------------------------------------------------------------------- |
| Restate server | 1.8 or later                                                                                              |
| TypeScript SDK | 1.17.4, as published, with [`restate-fns`](typescript/packages/fns/README.md)                             |
| Python SDK     | 1.1.0, as published, with [`restate-fns`](python/fns/README.md)                                           |
| Rust SDK       | 0.12.1 with [the patches](rust/sdk/README.md), on the released shared core 8.1.0; `make sdk-rs` builds it |

```sh
docker run --rm -p 8080:8080 -p 9070:9070 --add-host=host.docker.internal:host-gateway \
  ghcr.io/restatedev/restate:1.8.0
```

## Build and test

With [Nix](https://nixos.org) (flakes), `nix develop` gives a shell with every
tool below, on macOS and Linux ([`flake.nix`](flake.nix)):

```sh
nix develop       # or `direnv allow`, with use flake in .envrc
make              # build everything: the crate, the addon, the pnpm and uv workspaces
make test         # every test that needs no Restate server
make test-ts T=langgraph   # the TypeScript tests (vitest in typescript/tests), optionally filtered
make test-py      # the Python tests: the extension, restate-fns and the integrations
make crash-test         # the integrations and SQL objects through crashes, against a Restate server
make concurrency-test   # the context without journal replay, against a Restate server
make crash-test-py concurrency-test-py   # the same, in Python
make test-functions           # durable JavaScript in Restate objects, with a Restate server
make help
```

Without Nix: besides Rust, the Node.js parts need node 24 or later (the
examples and tests are TypeScript that node runs as it is, and OpenCode needs 24) and pnpm, which installs [their workspace](typescript/README.md) (`make js`).
The Python parts need Python 3.13 and [uv](https://docs.astral.sh/uv/), which
installs [their workspace](python/README.md) into `python/.venv` (`make py`),
building the extension with maturin.

The tests against a Restate server download its 1.8.0 release on first use
(see the [`Makefile`](Makefile): `RESTATE_VERSION`,
`RESTATE_FROM`, `RESTATE_IMAGE`, `RESTATE_SERVER`).
