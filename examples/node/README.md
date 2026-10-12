# SQL in Restate virtual objects, without a journal

An experiment: a Restate virtual object whose handlers keep a SQL database in
the object's state, with journal-based recovery turned off. The storage
protocol of this repository provides durability and recovery; Restate provides
the state, single-writer execution per key, and retries. It uses the
[sqlite-kv addon](../../sqlite-kv/node/README.md) and a patched Restate TypeScript SDK
([`vendor/typescript`](../../vendor/README.md#typescript)).

It runs on a released Restate server: see [the requirements](../../README.md#requirements).

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

`journalReplay` is a handler option, as well as a service option that its
handlers inherit: the examples set it for the whole object, but a single
handler may take it (`restate.handlers.object.exclusive({ ...restate.sqlOptions },
handler)`), and the object's other handlers keep replaying their journals.
`ctx.sql` needs it off in the handler that calls it.

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
once, at the end. The [pi example](../pi/README.md) runs pi-durable's storage this way.

The store is internal to the SDK: `get` is `ctx.get`, `set` and `clear` are
`ctx.set` and `ctx.clear`, and the barrier is an empty `ctx.run`. The SDK loads
the sqlite-kv addon, an optional peer dependency, on first use.

## How it works

**State access.** A get is a state read, from the runtime unless this attempt
wrote the key (lazy state, so an invocation does not start by loading every
page). A set or clear is a state write, and returns at once. Restate stores an
invocation's commands in order, and after a crash keeps a prefix of them, so
writes are FIFO: a lost write takes every later command with it, including
later writes, calls to other services and the response. The protocol's
ordering rules (commit after the transaction's other segments, `meta` after
the checkpointed pages, deletes after `meta`) hold by that order, without
waiting for acknowledgements.

**A barrier only for the page cache.** The process's page cache keeps what it
wrote (the last commit, checkpointed pages, the parked engine) for the
object's next invocation, so it must not trust writes that may be lost. The
store is ordered: a session that wrote confirms its writes when it is saved,
with one empty `ctx.run`, which the runtime acknowledges after the commands
before it. If the attempt dies first, the next session in this process starts
cold. Without that barrier a lost checkpoint would stay claimed, and the next
save would record `meta` past log segments whose pages were never stored, and
delete them (`tests/integration/ordered.rs` in the main crate simulates
exactly that).

**No journal replay.** On a retry or a resume, the SDK normally replays the
journal: each operation returns its recorded result and must match it. That
cannot work here, since a later attempt makes different reads (the page cache,
the log length and the other owners' checkpoints all differ). With
`journalReplay: false` the SDK skips the recorded journal: the handler runs
again from the start against the state as it is. Recovery is then the storage
protocol's: open reads `meta` and the log, so a half-written commit or
checkpoint from the crashed attempt is ignored or replayed over.

**One owner.** The protocol needs one writer per namespace. Restate runs one
exclusive invocation per object key at a time, and one attempt of it at a
time, so the namespace is `service/key` and only exclusive handlers may open
it.

## The context without journal replay

A retried attempt runs the handler again from the start, so every context
operation it reaches happens again. What each one does then:

| Operation                                                                | On a retried attempt                                                                                                                                                                                                          |
| ------------------------------------------------------------------------ | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `ctx.sql`, `ctx.sqlSession`                                              | Run again, against the state as stored. A handler makes itself idempotent with a stable id it is given, such as an order id or a transfer id in the request: it records the id with its writes, and finds it the second time. |
| Calls (`ctx.objectClient(...)`, `ctx.serviceClient(...)`, `genericCall`) | A new call, unless it has an idempotency key: with one that is the same on every attempt, the call reaches the invocation the earlier attempt started, and gets its result.                                                   |
| Sends, delayed sends                                                     | The same: with an idempotency key, a repeated send is dropped, and a delayed send keeps its first timer.                                                                                                                      |
| `ctx.get`, `ctx.set`, `ctx.clear`                                        | Read the state as stored; writes are ordered, and a lost one takes the later commands with it.                                                                                                                                |
| `ctx.run`                                                                | Not available: its result would not be kept, and a retried attempt runs the handler again anyway, so call the function directly. A call to it is a terminal error. (The SDK uses an empty run itself, as the barrier above.)  |
| `ctx.sleep`                                                              | Starts over.                                                                                                                                                                                                                  |
| `attach`, `cancel`                                                       | Idempotent already.                                                                                                                                                                                                           |
| Awakeables                                                               | Refused.                                                                                                                                                                                                                      |

The SDK does not make up keys: a call is idempotent when the code says so,
with `rpc.opts({ idempotencyKey })` (`rpc.sendOpts` for a send). The examples
build theirs from the request id, `ctx.request().id`, which is the same on
every attempt of an invocation, and what the call is for:

```js
await ctx
  .objectClient(warehouse, sku)
  .reserve({ order: ctx.key, qty }, restate.rpc.opts({ idempotencyKey: `${ctx.request().id}:reserve` }));
```

A key names one call: one that an invocation makes several times (a tool
call) adds what tells them apart. Restate attaches a call with a key it has
seen to the earlier invocation, whatever its input, so the key fits a call
whose input is the same on every attempt; one whose input depends on what
the attempt read needs that in its key, or none. A key holds while Restate
keeps the invocation: a day by default (the target's idempotency
retention).

### Plain promises, one await point

Without journal replay the handler's `ctx` returns plain promises: `ctx.get`,
`ctx.sleep`, calls, `invocationId`, and the rest. Await them,
combine them with `Promise.all`, `Promise.race` or your own combinators, or
leave them; a library that takes promises takes these. There is no
`RestatePromise` (no `map`, `orTimeout` or `RestatePromise.all`): with nothing
replayed, the order things happen in needs no recording.

- **An operation starts when it is made.** Its promise is registered then, and
  driven to completion whether or not anything awaits it: a call is made
  without an `await`. A failure nobody awaits is dropped, not an unhandled
  rejection.
- **One await point.** One loop per invocation drives every pending promise,
  and asks the server about all of them at once. So a suspension (after the
  server's inactivity timeout) resumes on the first of them to complete, and an
  operation made while the loop waits joins the next check: a call awaited
  late is not held up by a long sleep made earlier.
- **A cancellation reaches every pending promise** (each rejects with
  `CancelledError`); what is made after it runs normally.

In TypeScript this context has its own type, `UnjournaledContext`, with
`UnjournaledObjectContext`, `UnjournaledObjectSharedContext` and the workflow
ones: their operations return `Promise`s, there is no `awakeable` and no
`run`, and `ctx.journalReplay` is `false`. `restate.service`, `restate.object` and
`restate.workflow` pick it from the options: an unannotated `ctx` gets it
when they set `journalReplay: false` (`restate.sqlOptions` does), and a
handler annotated with the other context is a type error, in either
direction. A handler wrapper (`restate.handlers.object.exclusive` and the
others) takes it when its own options say `journalReplay: false`.

```ts
const counter = restate.object({
  name: 'counter',
  handlers: {
    add: async (ctx: restate.UnjournaledObjectContext, n: number) => {
      const [, total] = await Promise.all([ctx.sleep(10), ctx.get<number>('total')]);
      ctx.set('total', (total ?? 0) + n);
      return (total ?? 0) + n;
    },
  },
  options: { ...restate.sqlOptions },
});
```

A handler that replays its journal keeps the published SDK's context and
`RestatePromise`s. [`../concurrency.ts`](../concurrency.ts) tests all of
this against a real server.

## The examples

Each runs alone (`node node/shop.ts` from [`examples/`](..)), or together
with the others (`npm run serve` there, or `node serve.ts ledger shop` for
some, on port 9080, or `PORT`):

- [`ledger.ts`](ledger.ts): accounts and transfers, deduped by transfer id.
- [`shop.ts`](shop.ts): a Warehouse object per SKU and an Orders object per
  order, each with its own database, over RPC. Placing an order reserves stock
  (a call) and schedules its expiry (a delayed send); paying commits the
  stock, and an unpaid order releases it when it expires.

[`../pi`](../pi/README.md) has a durable agent on pi-durable, with its
storage in the object's database.

```sh
make examples-js   # from the repository root: the addon, the patched SDK, npm install
cd examples && npm run serve
docker run --rm -p 8080:8080 -p 9070:9070 --add-host=host.docker.internal:host-gateway ghcr.io/restatedev/restate:1.7.13   # Restate (see the repository's README, Requirements), in another terminal
restate deployments register http://host.docker.internal:9080   # as the server's container sees this host
curl localhost:8080/Ledger/bank/init --json '[8, 1000]'
curl localhost:8080/Orders/o1/place --json '{"sku": "apple", "qty": 2}'
```

All the examples run through crashes against a real server: see
[the examples' validation](../README.md#validation). 200 sequential ledger
transfers take 18.8 ms each on one local node.

## Caveats

- **Every read is journaled with its value.** A state read appends the value
  to the invocation's journal, so reading pages costs log writes, and the
  page cache matters more than it does elsewhere. `sqlOptions` sets journal
  retention to zero so completed journals are not kept.
- **The journal grows across attempts.** Skipped entries stay; each attempt
  appends its own.
- **A suspension is a restart.** Resuming means running from the start, so
  long handlers that hit the inactivity timeout start over.
- **Outside the model.** The TLA+ specification and the protocol monitor
  assume independent, acknowledged puts. Writes that return before they are
  stored are safe only by the FIFO argument above, which the crash tests test
  but nothing proves.
- **Assumptions to confirm with the runtime:** an attempt's stored commands
  are a prefix of those it sent, in order (FIFO writes); an empty `ctx.run` is
  acknowledged only after the commands before it are stored (the barrier); an
  abandoned attempt's commands are never applied after a new attempt starts
  (fencing); state a lazy read returns is durable. The crash tests kill
  processes, so they check ordering and recovery, not storage durability
  across a machine crash.

## Not supported

Shared handlers, awakeables, `ctx.sql` in a handler that replays its journal,
and request/response deployments (such as Lambda), which suspend at every await,
since a suspension is a restart.
