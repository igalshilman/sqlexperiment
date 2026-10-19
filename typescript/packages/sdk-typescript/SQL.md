# SQL in Restate virtual objects, without a journal

An experiment: a Restate virtual object whose handlers keep a SQL database in
the object's state, with journal-based recovery turned off. The storage
protocol of this repository provides durability and recovery; Restate provides
the state, single-writer execution per key, and retries. It uses the
[sqlite-kv addon](../sql-node/README.md) and a patched Restate TypeScript SDK
([`packages/sdk-typescript`](README.md#typescript)).

It runs on a released Restate server: see [the requirements](../../../README.md#requirements).

```js
import * as restate from '@restatedev/restate-sdk';

const ledger = restate.object({
  name: 'Ledger',
  handlers: {
    init: (ctx) =>
      ctx.transaction(async ({ db }) => {
        await db.execute('CREATE TABLE IF NOT EXISTS accounts(id INTEGER PRIMARY KEY, balance INTEGER)');
      }),
    deposit: (ctx, { id, amount }) =>
      ctx.transaction(async ({ db }) => {
        await db.execute('UPDATE accounts SET balance = balance + ? WHERE id = ?', [amount, id]);
      }), // one transaction: durable as a whole once ctx.transaction resolves
  },
  options: restate.sqlOptions, // journalReplay: false, lazy state, no journal retention
});
```

`journalReplay` is a handler option, as well as a service option that its
handlers inherit: the examples set it for the whole object, but a single
handler may take it (`restate.handlers.object.exclusive({ ...restate.sqlOptions },
handler)`), and the object's other handlers keep replaying their journals.
`ctx.db` needs it off in the handler that uses it.

`ctx.db`, on the context of an exclusive object handler, is the object's
database (namespace `service/key`) over the object's state, with the
process's page cache. A shared handler's `ctx.db` reads it, while an exclusive
handler runs or not: see [Shared handlers](#shared-handlers). Each statement is its own transaction, and
`db.transaction(fn)` groups several; calls run one at a time, in call order.
A crash between two commits keeps the first: statements that must stand or
fall together go in one transaction.

`ctx.transaction(body)` is the handler's unit of work: `body` gets
`{ db, tx }`, one transaction's SQL and its sends, which commit together (see
below). It commits when `body` resolves and rolls back when it rejects. A
library that brings its own transactions uses `ctx.db.transaction`.

An attempt has one session. It opens on the first call to `ctx.db` or
`ctx.transaction` (a read of the commit record, with the log inline), stays
open while the handler runs, however many calls it makes, and is saved when
the handler ends. A transaction commits with one state write, of the commit
record, and is durable when the statement or `ctx.transaction` that commits
it resolves: the SDK waits for an empty `ctx.run` after the write. A library
that wants a database for the handler's work gets `ctx.db`: the
[pi integration](../integrations/src/pi/README.md) runs pi-durable's storage
this way.

An SQL error rejects as a `TerminalError` (its `cause` the SQL error), from
`ctx.db` and `ctx.transaction` alike, since a retry would repeat it; a storage
error, which ends the session, is retried. The handler's own errors are as in any handler: a `TerminalError` is
terminal, and any other error, such as a failed request to a model or another
service, is retried. Creating tables is up to the handlers, such as `init`.

**Sends that commit with the SQL.** A send made with `tx`, the transaction's
send clients, is made once the transaction commits, and only then:

```js
ctx.transaction(async ({ db, tx }) => {
  await db.execute('UPDATE accounts SET balance = balance - ? WHERE id = ?', [amount, from]);
  tx.objectSendClient(store, 'a').reserve(item);
  tx.objectSendClient(store, 'b').deposit(amount);
});
```

The sends commit with the transaction, in the same write: the commit record
holds them (its _effects_, opaque to the storage), as the database's outbox.
Once the commit is done the SDK makes them, each with an idempotency key of
the transaction, `transaction/<id>/<index>` (or the send's own key), with a
random id of the transaction. Restate keeps an attempt's commands in the
order they were made, so a send is stored whenever a commit made after it
is: the next transaction's commit leaves this attempt's sends out of the
outbox, and the handler's end empties it with a commit of its own. The
session's next open makes again the sends the outbox still holds, with the
same keys; Restate drops a send whose key it has seen. So each send of a committed transaction is made once, even when a
retried attempt never runs the transaction's body again (it finds its work
done, by its id), and a send of a transaction rolled back or lost is not
made, since the commit that would hold it is not stored. A transaction
without sends costs nothing more; a handler whose transactions sent costs
one more commit at its end. Two limits: Restate keeps an
idempotency key for the target's retention (a day by default), and a send
made again after that, by an invocation that died between the commit and its
end and whose object is not used again for that long, is made twice; and a
send has no invocation id, which only exists once it is made. Only `db` and
`tx` are part of the transaction: a call, whose result the body would wait
for, cannot be, and one made through `ctx` inside the body is made whether or
not the transaction commits, while holding it open. Make calls before the
transaction, with an idempotency key of their own, and record their outcome
in it, as the shop's `Orders` does; its `expire` is a send of `tx`.

The store is internal to the SDK: `get` is `ctx.get`, `set` and `clear` are
`ctx.set` and `ctx.clear`, and `sync` is an empty `ctx.run`. The SDK loads
the sqlite-kv addon, an optional peer dependency, on first use.

**The database is the object's state.** Its keys (`v14/commit`, `v14/log/…`,
`v14/page/…`, `v14/hint`) sit next to the object's other state keys, so
`ctx.clearAll()` in any handler of the object deletes the database, a handler's
own key starting with `v14/` corrupts it, and `ctx.stateKeys()` lists its
pages. Keep other state out of `v14/`, and do not clear all of an object that
uses `ctx.db`. (Reserving these keys in the SDK is future work.)

**One cluster per process.** The page cache knows a database by its namespace,
`service/key`, and trusts what it cached while the database's commit record
is the one it left; nothing identifies the database itself. A process
registered with two Restate clusters (staging and production, or a dev
cluster wiped and recreated) could take one cluster's `Ledger/bank` for the
other's and serve its cached pages. Run one process per cluster, or give each
its own `SQLITE_KV_CACHE_FILE` and restart it after recreating a cluster.
(A database id in the commit record is future work.)

## How it works

**State access.** A get is a state read (journaled for now; left out of the
journal once protocol v8 ships, see FUTURE.md), from the runtime unless this attempt wrote the key or
the invocation started with it (lazy state, so an invocation does not start
by loading every page). A set or clear is a state write, and returns at once.
Restate keeps an attempt's writes, and its sends, in the order they were
made: a crash loses a suffix of them, never a write without the ones before
it. Each transaction is one write, of the commit record, which holds the log
since the last checkpoint (a large transaction's log also goes to segments,
written before the record). A checkpoint writes the pages after that record,
then the record that no longer needs the log. Then one empty `ctx.run` waits
until all of it is stored: a commit is one round trip. Whatever prefix a crash
keeps, the stored record names only what is stored, and its log covers the
pages a checkpoint rewrote.

**The page cache.** The process's page cache keeps the pages it read and
wrote, with the commit record they belong to, for the object's next
invocation. A session opens on them only when the stored record is the one
the cache saw last; any other record, such as one a crashed attempt in
another process wrote, makes it read the pages again.

**No journal replay.** On a retry or a resume, the SDK normally replays the
journal: each operation returns its recorded result and must match it. That
cannot work here, since a later attempt makes different reads (the page cache,
the log length and the other owners' checkpoints all differ). With
`journalReplay: false` the SDK skips the recorded journal: the handler runs
again from the start against the state as it is. Recovery is then the storage
protocol's: open reads the commit record, so a lost commit or checkpoint from
the crashed attempt is as if it never started.

**One owner.** The protocol needs one writer per namespace. Restate runs one
exclusive invocation per object key at a time, and one attempt of it at a
time, so the namespace is `service/key` and only exclusive handlers may open
it.

## The context without journal replay

A retried attempt runs the handler again from the start, so every context
operation it reaches happens again. What each one does then:

| Operation                                                                | On a retried attempt                                                                                                                                                                                                          |
| ------------------------------------------------------------------------ | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `ctx.db`, `ctx.transaction`                                              | Run again, against the state as stored. A handler makes itself idempotent with a stable id it is given, such as an order id or a transfer id in the request: it records the id with its writes, and finds it the second time. |
| Calls (`ctx.objectClient(...)`, `ctx.serviceClient(...)`, `genericCall`) | A new call, unless it has an idempotency key: with one that is the same on every attempt, the call reaches the invocation the earlier attempt started, and gets its result.                                                   |
| Sends, delayed sends                                                     | The same: with an idempotency key, a repeated send is dropped, and a delayed send keeps its first timer.                                                                                                                      |
| `ctx.get`, `ctx.set`, `ctx.clear`                                        | Read the state as stored; a write is stored once an empty `ctx.run` after it resolves, and a crash before that may lose it.                                                                                                   |
| `ctx.run`                                                                | Not available: its result would not be kept, and a retried attempt runs the handler again anyway, so call the function directly. A call to it is a terminal error. (The SDK uses an empty run itself, to sync the store.)     |
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

In TypeScript this context has its own type, `SqlContext`, with
`SqlObjectContext`, `SqlObjectSharedContext` and the workflow
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
    add: async (ctx: restate.SqlObjectContext, n: number) => {
      const [, total] = await Promise.all([ctx.sleep(10), ctx.get<number>('total')]);
      ctx.set('total', (total ?? 0) + n);
      return (total ?? 0) + n;
    },
  },
  options: { ...restate.sqlOptions },
});
```

A handler that replays its journal keeps the published SDK's context and
`RestatePromise`s. [`tests/concurrency.ts`](../../tests/concurrency.ts) tests all of
this against a real server.

## The examples

The smallest is [`examples-typescript/notes.ts`](../../../examples-typescript/notes.ts): plain SQL
with `ctx.db`. The ledger and the shop are in [`tests/sql/`](../../tests/sql),
where the tests use them. Each runs alone (`node sql/shop.ts` from
[`tests/`](../../tests)), or together with the others (`pnpm serve` there, or
`node serve.ts ledger shop` for some, on port 9080, or `PORT`):

- [`ledger.ts`](../../tests/sql/ledger.ts): accounts and transfers, deduped by transfer id.
- [`shop.ts`](../../tests/sql/shop.ts): a Warehouse object per SKU and an Orders object per
  order, each with its own database, over RPC. Placing an order reserves stock
  (a call) and schedules its expiry (a delayed send); paying commits the
  stock, and an unpaid order releases it when it expires.

[`restate-agents/pi`](../integrations/src/pi/README.md) has a durable agent on pi-durable, with its
storage in the object's database.

```sh
make js   # from the repository root: the addon, the patched SDK, pnpm install
cd typescript/tests && pnpm serve
docker run --rm -p 8080:8080 -p 9070:9070 --add-host=host.docker.internal:host-gateway ghcr.io/restatedev/restate:1.8.0   # Restate (see the repository's README, Requirements), in another terminal
restate deployments register http://host.docker.internal:9080   # as the server's container sees this host
curl localhost:8080/Ledger/bank/init --json '[8, 1000]'
curl localhost:8080/Warehouse/apple/restock --json '{"id": "first", "qty": 10}'   # without stock, an order is rejected
curl localhost:8080/Orders/o1/place --json '{"sku": "apple", "qty": 2}'
```

Both run through crashes against a real server: see
[the crash test](../../tests/crash.ts) (`make crash-test`). 200 sequential ledger
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
- **A sync per commit.** Every transaction waits for an empty `ctx.run` after
  its record: a round trip to the runtime, which statements that commit one
  by one pay each time. Group them in a transaction.
- **Assumptions to confirm with the runtime:** an empty `ctx.run` is
  acknowledged only after the commands before it are stored (writes and
  sends: the sync); an abandoned attempt's commands are never applied after a
  new attempt starts (fencing); state a lazy read returns is durable. The
  crash tests kill processes, so they check recovery, not storage durability
  across a machine crash.

## Shared handlers

A shared handler's `ctx.db` is the object's database as stored, read beside
the exclusive handler, which may commit meanwhile (a chat UI's read of a
conversation while the agent works, say). It opens as of its first call, and
writes nothing: what the handler writes stays in its session, and is never
stored. If the exclusive handler commits while it reads, the attempt fails
when the handler ends, and Restate retries it, so what a shared handler
returns was the stored state ([the reader](../../../core/sqlite-kv/docs/DESIGN.md#readers)).
`ctx.transaction` is the exclusive handlers'.

```ts
count: restate.handlers.object.shared({ journalReplay: false }, async (ctx: restate.SqlObjectSharedContext) =>
  ctx.db.query('SELECT count(*) FROM accounts'),
),
```

## Not supported

Writes from shared handlers, awakeables, `ctx.db` in a handler that replays its journal,
and request/response deployments (such as Lambda), which suspend at every await,
since a suspension is a restart.
