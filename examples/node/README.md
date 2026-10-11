# SQL in Restate virtual objects, with the TypeScript SDK

The Node.js counterpart of [the Rust integration](../../restate/README.md): a
virtual object keeps a SQL database in its state, with journal replay turned
off. It uses the [sqlite-kv addon](../../node/README.md) and a patched Restate
TypeScript SDK ([`vendor/typescript`](../../vendor/README.md#typescript)).

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
retried. Creating tables is up to the handlers, such as `init`.

`ctx.sqlSession(body)` keeps the database for longer work, such as a whole
handler: each statement is its own transaction, and `db.transaction(fn)`
groups several. A commit is durable once its writes are stored, which Restate
does in order, so it needs no barrier of its own: a lost commit takes
everything after it, including calls and the response. The session is saved
once, at the end. The agent example runs pi-durable's storage this way.

The store is internal to the SDK: `get` is `ctx.get`, `set` and `clear` are
`ctx.set` and `ctx.clear`, and the barrier is an empty `ctx.run`. The SDK loads
the sqlite-kv addon, an optional peer dependency, on first use.

## The context without journal replay

A retried attempt runs the handler again from the start, so every context
operation it reaches happens again. What each one does then:

| Operation | On a retried attempt |
|---|---|
| `ctx.sql`, `ctx.sqlSession` | Run again, against the state as stored. A handler makes itself idempotent with a stable id it is given, such as an order id or a transfer id in the request: it records the id with its writes, and finds it the second time. |
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

Promises may be awaited concurrently: the patched SDK lifts the published
SDK's limit of one pending await per invocation.

## The examples

Each runs alone (`node shop.mjs`), or together (`npm run serve`, or
`node serve.mjs ledger shop` for some, on port 9080, or `PORT`):

- [`ledger.mjs`](ledger.mjs): accounts and transfers, deduped by transfer id.
  The Rust ledger, ported.
- [`shop.mjs`](shop.mjs): a Warehouse object per SKU and an Orders object per
  order, each with its own database, over RPC. Placing an order reserves stock
  (a call) and schedules its expiry (a delayed send); paying commits the
  stock, and an unpaid order releases it when it expires.
- [`agent.mjs`](agent.mjs): a durable agent on
  [pi-durable](https://www.npmjs.com/package/@earendil-works/pi-durable)
  (`@earendil-works/pi-durable`), a durable agent harness that commits
  conversations, model turns, tool calls and their state to storage before
  anything is shown, and resumes unfinished work when the storage is opened
  again. An Agent object per session runs a pi-durable Harness whose storage
  is the object's database: the handler runs in one `ctx.sqlSession`, and
  [`pi-storage.mjs`](pi-storage.mjs) gives pi-durable's SQLite storage its
  `SqliteDatabase` facade over it (pi-durable's storage conformance suite
  passes on it). A retried attempt reopens the Harness and carries on:
  - a prompt is submitted once, by its id (pi-durable's `requestId`);
  - an answered prompt returns its answer;
  - unfinished work resumes: a model call is made again, and a tool call
    declared replay-safe runs again with the same call id.

  Its tools move money in the ledger, with the call id as the transfer id.
  `PI_MODEL=anthropic/claude-sonnet-4-5` (with `ANTHROPIC_API_KEY`) picks a
  model; by default a scripted teller answers "move 5 from 1 to 2" and
  "audit", with no key.

```sh
make examples-node   # from the repository root: the addon, the patched SDK, npm install
npm run serve
restate deployments register http://localhost:9080
curl localhost:8080/Ledger/bank/init --json '[8, 1000]'
curl localhost:8080/Agent/me/prompt --json '{"id": "p1", "text": "move 5 from 1 to 2"}'
curl localhost:8080/Orders/o1/place --json '{"sku": "apple", "qty": 2}'
```

## Validation

[`test.mjs`](test.mjs) runs every example against a real server through
crashes. The ledger and the shop run in one process, aborting at 1.5% of
state requests; the agents run in another, aborting at 0.3% (a prompt makes
some 200 state requests, so one attempt in three crashes). The server is
killed twice. Meanwhile:
- 60 orders are placed (a quarter twice), half of them paid, the rest
  expiring;
- four agent sessions take eight transfer prompts each (a third sent twice)
  and an audit.

It checks that each warehouse's stock balances and agrees with every order,
that each prompt moved money exactly once, and, from the server's
`sys_invocation` table, that there is exactly one `reserve` and one `transfer`
invocation per order and prompt, though their callers ran many attempts. It
passed twice, with 109 and 108 restarts of the shop's process and 85 and 92 of
the agents'.

```sh
RESTATE_SERVER=/path/to/restate-server make crash-test-node
```

The Rust crash campaign ([`examples/rust/ledger`](../rust/ledger)) also runs
this ledger, with `LEDGER_COMMAND`: 120 transfers from six clients, a third
sent twice, the ledger aborting at random state requests, and the server
killed three times (`make crash-test-rust` runs it for both ledgers).

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
