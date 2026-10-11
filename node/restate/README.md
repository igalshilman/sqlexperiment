# SQL in Restate virtual objects, with the TypeScript SDK

The Node.js counterpart of [the Rust integration](../../restate/README.md): a
virtual object keeps a SQL database in its state, with journal replay turned
off. It uses the [sqlite-kv addon](../README.md) and a patched Restate
TypeScript SDK.

```js
import * as restate from '@restatedev/restate-sdk';

const ledger = restate.object({
  name: 'Ledger',
  handlers: {
    init: (ctx) =>
      ctx.sql(async (db) => {
        await db.execute('CREATE TABLE IF NOT EXISTS accounts(id INTEGER PRIMARY KEY, balance INTEGER)');
      }),
    deposit: (ctx, { id, amount }) =>
      ctx.sql(async (db) => {
        await db.execute('UPDATE accounts SET balance = balance + ? WHERE id = ?', [amount, id]);
      }), // one transaction: durable as a whole once ctx.sql resolves
  },
  options: restate.sqlOptions, // journalReplay: false, lazy state, no journal retention
});
```

`ctx.sql(body)`, on the context of an exclusive object handler, opens the
object's database (namespace `service/key`) over the object's state, with the
process's page cache, runs `body` as one SQL transaction, and ends the session
with `save()` (one `ctx.run`, if it wrote, so the page cache may trust the
writes). An SQL error, or the body's own, is terminal; a storage error is
retried. Creating tables is up to the handlers, such as `init`. The store is
internal to the SDK; the SDK loads the sqlite-kv addon, an optional peer
dependency, on the first `ctx.sql`.

## The context without journal replay

A retried attempt runs the handler again from the start, so every context
operation it reaches happens again. What each one does then:

| Operation | On a retried attempt |
|---|---|
| `ctx.sql` | Runs its transaction again, against the state as stored. A handler makes itself idempotent with a stable id it is given, such as an order id or a transfer id in the request: it records the id with its writes, and finds it the second time. |
| Calls (`ctx.objectClient(...)`, `ctx.serviceClient(...)`, `genericCall`) | Idempotent: the call gets an idempotency key derived from the invocation id and the request (target, input, and how many identical requests came before in this attempt). The same call reaches the invocation the earlier attempt started, and gets its result. |
| Sends, delayed sends | Idempotent in the same way: a repeated send is dropped, and a delayed send keeps its first timer. |
| `ctx.get`, `ctx.set`, `ctx.clear` | Read the state as stored; writes are ordered, and a lost one takes the later commands with it. |
| `ctx.run` | Runs again: its result is not kept. An empty `ctx.run` is a barrier: every write before it is stored once it resolves. |
| `ctx.sleep` | Starts over. |
| `attach`, `cancel` | Idempotent already. |
| Awakeables | Refused. |

An explicit idempotency key (`rpc.opts({ idempotencyKey })`) is used as
given. A derived key holds while Restate keeps the invocation: a day by
default (the target's idempotency retention). A retried attempt that makes a
different request (other input, because it read other state) starts a new
invocation, as it should.

## Examples

[`examples/`](examples), each runnable alone (`node examples/shop.mjs`) or
together (`npm run serve`, port 9080):

- [`ledger.mjs`](examples/ledger.mjs): accounts and transfers, deduped by
  transfer id. The Rust ledger, ported.
- [`shop.mjs`](examples/shop.mjs): a Warehouse object per SKU and an Orders
  object per order, each with its own database, over RPC. Placing an order
  reserves stock (a call) and schedules its expiry (a delayed send); paying
  commits the stock, and an unpaid order releases it when it expires.
- [`agent.mjs`](examples/agent.mjs): a durable
  [pi](https://github.com/badlogic/pi-mono) agent. An Agent object per
  session keeps its transcript in SQL, storing each message as the loop
  produces it and before any tool runs. A retried attempt carries on from the
  transcript:
  - a prompt is added once, by its id;
  - an answered prompt returns its answer without the model;
  - tool calls left without results run again with the same tool call id;
  - otherwise the loop continues.

  Its tools move money in the ledger, with the tool call id as transfer id.
  `PI_MODEL=anthropic/claude-sonnet-4-5` (with `ANTHROPIC_API_KEY`) picks a
  model; by default a scripted teller answers "move 5 from 1 to 2" and
  "audit", with no key.

```sh
npm run serve
restate deployments register http://localhost:9080
curl localhost:8080/Ledger/bank/init --json '[8, 1000]'
curl localhost:8080/Agent/me/prompt --json '{"id": "p1", "text": "move 5 from 1 to 2"}'
curl localhost:8080/Orders/o1/place --json '{"sku": "apple", "qty": 2}'
```

## How the store maps to the SDK

Inside the SDK (`src/sql.ts` in the patch):

| Store | SDK |
|---|---|
| `get` | `ctx.get(key, serde.binary)` |
| `set`, `clear` | `ctx.set`, `ctx.clear`: ordered, returning at once |
| `sync` | `ctx.run('sync', ...)`, acknowledged after the commands before it |

**One wait at a time.** The SDK waits on one thing at a time per invocation (a
second concurrent `await` of a context promise fails with "awaitNext already
pending"), while the engine reads pages concurrently. So the store issues each
request at once, which keeps the commands in the engine's order with respect
to `set` and `clear`, and awaits the requests issued together as one batch
(`RestatePromise.allSettled`), one batch after another.

## The SDK patches

`setup.sh` clones the SDK and its shared core at pinned revisions (the
TypeScript SDK 1.17.3, and the shared core revision it uses, with service
protocol V8), applies [`patches/`](patches), and builds:

- **shared core** (`sdk-shared-core.patch`): `JournalReplay::Disabled`, as in
  the Rust patch: a retried or resumed attempt skips the recorded commands,
  with fresh completion ids, and refuses awakeables.
- **TypeScript SDK** (`sdk-typescript.patch`): a `journalReplay` service and
  handler option, passed to the WebAssembly VM's new constructor argument; the
  bindings build against the patched core. `ObjectContext.sql`, with
  `restate.sqlOptions` and the `SqlDatabase` type; it refuses a handler that
  replays its journal, and a shared one. Derived idempotency keys for calls
  and sends without journal replay (`src/idempotency.ts`).
- The core is compiled to WebAssembly and inlined into the SDK with
  [`inline-wasm.mjs`](inline-wasm.mjs). The SDK's own inliner
  (wasm-pack-inline 0.1.4) slices wasm-bindgen's output at text positions that
  current wasm-bindgen no longer produces, and duplicates code.

```sh
./setup.sh   # needs cargo + wasm32-unknown-unknown, wasm-bindgen 0.2.121, node, pnpm
```

## Validation

[`examples/test.mjs`](examples/test.mjs) runs every example against a real
server through crashes:
- the service aborts at random state requests (1.5%), and the server is killed
  twice;
- 60 orders are placed (a quarter twice), half of them paid, the rest
  expiring;
- four agent sessions take eight transfer prompts each (a third sent twice)
  and an audit.

It checks that each warehouse's stock balances and agrees with every order,
that each prompt moved money exactly once, and, from the server's
`sys_invocation` table, that there is exactly one `reserve` and one `transfer`
invocation per order and prompt, though their callers ran many attempts. It
passed with 176 and 142 service restarts.

```sh
RESTATE_SERVER=/path/to/restate-server npm test
```

The Rust crash campaign ([`restate/tests/crash.rs`](../../restate/tests/crash.rs))
runs the ledger with `LEDGER_COMMAND`: 120 transfers from six clients, a third
sent twice, the ledger aborting at random state requests, and the server killed
three times.

```sh
cd ../../restate
LEDGER_COMMAND="node $PWD/../node/restate/examples/ledger.mjs" \
RESTATE_SERVER=/path/to/restate-server cargo test --test crash -- --nocapture
```

| Abort rate per state request | Ledger restarts | Result |
|---|---|---|
| 1.5% | 57, 39, 40 | All consistent |
| 4% | 394, 271 | All consistent |

200 sequential transfers take 18.8 ms each on one local node (the Rust ledger
takes 14.5 ms).

## Not supported

As in Rust: shared handlers, awakeables, journal replay for the same service,
and request/response deployments (such as Lambda), which suspend at every
await, since a suspension is a restart.
