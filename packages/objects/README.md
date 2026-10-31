# restate-objects: SQL in Restate virtual objects

Each key of a Restate virtual object gets a SQL database of its own, `ctx.db`,
kept in the object's state. The handlers that use it run with the SDK's
explicit recovery instead of journal replay: the storage protocol of this
repository provides durability and recovery, and Restate provides the state,
single-writer execution per key, and retries. Its storage is
a Rust addon in [`native/`](native/README.md), on the published Restate TypeScript
SDK (1.17.4, pinned: see [concurrent awaits](#concurrent-awaits-one-await-point)).

It runs on a released Restate server: see [the requirements](../../README.md#requirements).

```ts
import * as restate from 'restate-objects';

const ledger = restate.object({
  name: 'Ledger',
  schema: ['CREATE TABLE accounts(id INTEGER PRIMARY KEY, balance INTEGER)'], // applied once per key
  handlers: {
    deposit: (ctx: restate.ObjectContext, { id, amount }: { id: number; amount: number }) =>
      ctx.transaction(async ({ db }) => {
        await db.execute('UPDATE accounts SET balance = balance + ? WHERE id = ?', [amount, id]);
      }), // one transaction: durable as a whole once ctx.transaction resolves
    balance: restate.handlers.object.shared(async (ctx: restate.ObjectSharedContext, id: number) =>
      ctx.db.query('SELECT balance FROM accounts WHERE id = ?', [id]),
    ),
  },
});
```

`restate-objects` is the Restate SDK (it re-exports `@restatedev/restate-sdk`),
with the virtual objects of SQL: in `restate.object`, each function in
`handlers` is an exclusive handler with `ctx.db`;
`restate.handlers.object.shared(fn)` makes a shared one, and
`restate.handlers.object.exclusive(options, fn)` one with handler options
(such as `input`). With `journalReplay: true` in its options, a handler is
the SDK's own: it replays its journal and has no `ctx.db` (its context is
`restate.JournaledObjectContext`), such as one that waits on an awakeable.
The object's options are `restate.sqlOptions` (lazy state),
and its metadata hides the database's state commands in Restate's UI.

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
`ctx.transaction` (a read of the database's head), stays open while the
handler runs, however many calls it makes, and is saved when the handler
ends. A transaction commits as one tail entry, then the head that names it,
and is durable when the statement or `ctx.transaction` that commits
it resolves: the SDK waits for an empty `ctx.run` after the write. A library
that wants a database for the handler's work gets `ctx.db`: the
[pi integration](../agents/src/pi/README.md) runs pi-durable's storage
this way.

An SQL error rejects as a `TerminalError` (its `cause` the SQL error), from
`ctx.db` and `ctx.transaction` alike, since a retry would repeat it: a
statement the engine rejects, a parameter it cannot bind, or a call the
session refuses, even one that ends the session (a `COMMIT` inside a
transaction's body, a transaction left open when the handler ends). A storage
error, which ends the session, is retried. The handler's own errors are as in
any handler: a `TerminalError` is terminal, and any other error, such as a
failed request to a model or another service, is retried.

**The schema.** `schema` in `restate.object` is the steps that make each
key's database, in order: statements, or async functions of the transaction
(to move data). They are applied once per key, in one transaction, when a
handler of the key first opens its database. How far each database got is
a record beside its tables, in none of them, committed with the steps, so a
step added later (a column, an index) runs on each key's database once, as
that key is next opened. A step never changes once it ran: a change is a
step of its own. A shared handler applies the steps its key lacks in its own
session, which stores nothing: a key never written reads as empty tables.
The database holds your tables, and those of the libraries you run in it:
restate-agents names its agents' tables `restate_…`, so name yours otherwise.

**Sends that commit with the SQL.** A send made with `tx`, the transaction's
send clients, is made once the transaction commits, and only then:

```js
ctx.transaction(async ({ db, tx }) => {
  await db.execute('UPDATE accounts SET balance = balance - ? WHERE id = ?', [amount, from]);
  tx.objectSendClient(store, 'a').reserve(item);
  tx.objectSendClient(store, 'b').deposit(amount);
});
```

The sends commit with the transaction, in the same tail entry: the
database's record `outbox` holds them (beside its tables, opaque to the
storage).
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

The store is the object's state: `get` is `ctx.get`, `set` and `clear` are
`ctx.set` and `ctx.clear` (each named after its key), and `sync` is an empty
`ctx.run`.

**The database is the object's state.** Its keys (`v22/head`, `v22/tail/…`,
`v22/manifest/…` and `v22/pack/…`) sit next to the object's
other state keys, so `ctx.clearAll()` in any handler of the object deletes
the database, a handler's own key starting with `v22/` corrupts it, and
`ctx.stateKeys()` lists them. Keep other state out of `v22/`, and do not
clear all of an object that uses `ctx.db`. (Reserving
these keys in the SDK is future work.)

## How it works

**State access.** A get is a state read (journaled for now; left out of the
journal once protocol v8 ships, see FUTURE.md), from the runtime unless this attempt wrote the key or
the invocation started with it (lazy state, so an invocation does not start
by loading every page). A set or clear is a state write, and returns at once.
Restate keeps an attempt's writes, and its sends, in the order they were
made: a crash loses a suffix of them, never a write without the ones before
it. A transaction writes the pages it changed and its records as one tail
entry under a key of its own, then the head, which names it: the commit
point. Then one empty `ctx.run` waits until all of it is stored: a commit is
one round trip. Whatever prefix a crash keeps, the stored head names only a
tail and checkpoints that are stored. Now and then a commit first folds the
tail into a checkpoint (see the [state's architecture](native/src/state/docs/ARCHITECTURE.md)).

**The page cache.** The process's page cache keeps the pages it read and
wrote, and the database's state as of its last head, for the object's next
invocation. Checkpoints' packs never change, so the cache keeps their ranges
by location and never invalidates them: a session reads the stored head and,
if another process wrote since, the ranges it names that the cache lacks.
The head carries the database's random id, which
the cache's keys include, so one process may serve several Restate clusters,
or a cluster wiped and made again, without mixing their databases.

**No journal replay.** On a retry or a resume, the SDK normally replays the
journal: each operation returns its recorded result and must match it. That
cannot work here: a later attempt makes different reads, since what it
fetches depends on what the page cache holds. With explicit
recovery (`restate.internal.explicitRecovery()`, which the handlers here
have) the SDK skips the recorded journal: the handler runs again from the
start against the state as it is. Recovery is then the storage
protocol's: open reads the head, so a lost commit from the crashed attempt is
as if it never started.

**One owner.** The protocol needs one writer per namespace. Restate runs one
exclusive invocation per object key at a time, and one attempt of it at a
time, so the namespace is `service/key` and only exclusive handlers may open
it.

## The context with explicit recovery

A retried attempt runs the handler again from the start, so every context
operation it reaches happens again. What each one does then:

| Operation                                                                | On a retried attempt                                                                                                                                                                                                          |
| ------------------------------------------------------------------------ | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `ctx.db`, `ctx.transaction`                                              | Run again, against the state as stored. A handler makes itself idempotent with a stable id it is given, such as an order id or a transfer id in the request: it records the id with its writes, and finds it the second time. |
| Calls (`ctx.objectClient(...)`, `ctx.serviceClient(...)`, `genericCall`) | A new call, unless it has an idempotency key: with one that is the same on every attempt, the call reaches the invocation the earlier attempt started, and gets its result.                                                   |
| Sends, delayed sends                                                     | The same: with an idempotency key, a repeated send is dropped, and a delayed send keeps its first timer.                                                                                                                      |
| `ctx.get`, `ctx.set`, `ctx.clear`                                        | Read the state as stored; a write is stored once an empty `ctx.run` after it resolves, and a crash before that may lose it.                                                                                                   |
| `ctx.run`                                                                | Runs its action again: its result is not kept from one attempt to the next, so record in the database what must not be repeated. (The store uses an empty run, to sync.)                                                      |
| `ctx.sleep`                                                              | Starts over.                                                                                                                                                                                                                  |
| `attach`, `cancel`                                                       | Idempotent already.                                                                                                                                                                                                           |
| Awakeables                                                               | Refused, a terminal error (their signal ids would collide): wait in a handler with `journalReplay: true`.                                                                                                                     |

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

### Concurrent awaits, one await point

The context's operations are the SDK's `RestatePromise`s, and they can be
awaited concurrently: `Promise.all`, `Promise.race`, your own combinators,
several async functions at once, or a library's (the database's own reads
are concurrent). The published SDK drives each awaited promise with a loop
of its own, and lets one of them wait for the server at a time
(`ExternalProgressChannel`: a second waiter fails the attempt with
`awaitNext already pending`, and it is retried forever). So a handler with
SQL drives every promise it awaits with one loop of its own,
[the selector](src/selector.ts): before the handler runs, it takes over the
invocation's `promisesExecutor.doProgress`, and at the attempt's end gives it
back. That is the SDK's internals, as of 1.17.4: the package pins that
version, and refuses to run a handler on an SDK without them.
[`test/selector.test.ts`](test/selector.test.ts) tests the
selector against a fake VM and the SDK's single-waiter channel.

- **One await point.** The loop asks the server about every awaited promise
  at once. So a suspension (after the server's inactivity timeout) resumes on
  the first of them to complete, and a promise awaited while the loop waits
  joins the next check: a call awaited late is not held up by a long sleep
  made earlier.
- **An operation starts when it is made**: a call is made without an `await`,
  and a failure nobody awaits is not an unhandled rejection.
- **A cancellation reaches every awaited promise** (each rejects with
  `CancelledError`).

```ts
const counter = restate.object({
  name: 'counter',
  handlers: {
    add: async (ctx: restate.ObjectContext, n: number) => {
      const [, total] = await Promise.all([ctx.sleep(10), ctx.get<number>('total')]);
      ctx.set('total', (total ?? 0) + n);
      return (total ?? 0) + n;
    },
  },
});
```

[`e2e/concurrency.ts`](../e2e/concurrency.ts) tests all of this against
a real server.

## The examples

The smallest is [`examples/basic/notes.ts`](../examples/basic/notes.ts):
plain SQL with `ctx.db`. The ledger and the shop are in [`test/`](test), where
the tests use them. Each runs alone (`node test/shop.ts`, from here), or
together with the others (`pnpm --filter e2e serve`, or
`node serve.ts ledger shop` in [`packages/e2e/`](../e2e) for some), on port
9080, or `PORT`:

- [`ledger.ts`](test/ledger.ts): accounts and transfers, deduped by transfer id.
- [`shop.ts`](test/shop.ts): a Warehouse object per SKU and an Orders object per
  order, each with its own database, over RPC. Placing an order reserves stock
  (a call) and schedules its expiry (a delayed send); paying commits the
  stock, and an unpaid order releases it when it expires.

[`restate-agents/pi`](../agents/src/pi/README.md) has a durable agent on pi-durable, with its
storage in the object's database.

```sh
pnpm install && pnpm build   # from the repository root: the workspace and the addon
pnpm --filter e2e serve                                            # the objects, on port 9080
npx @restatedev/restate-server                                       # Restate, in another terminal
npx @restatedev/restate deployments register http://localhost:9080
curl localhost:8080/Ledger/bank/init --json '[8, 1000]'
curl localhost:8080/Warehouse/apple/restock --json '{"id": "first", "qty": 10}'   # without stock, an order is rejected
curl localhost:8080/Orders/o1/place --json '{"sku": "apple", "qty": 2}'
```

Both run through crashes against a real server: see
[the crash test](../e2e/crash.ts) (`pnpm --filter e2e crash`). 200 sequential ledger
transfers take 18.8 ms each on one local node.

## Caveats

- **Every read is journaled with its value.** A state read appends the value
  to the invocation's journal, so reading pages costs log writes, and the
  page cache matters more than it does elsewhere. A completed invocation's
  journal, those reads included, is kept for Restate's default retention.
- **The journal grows across attempts.** Skipped entries stay; each attempt
  appends its own.
- **A suspension is a restart.** Resuming means running from the start, so
  long handlers that hit the inactivity timeout start over.
- **A sync per commit.** Every transaction waits for an empty `ctx.run` after
  its head: a round trip to the runtime, which statements that commit one
  by one pay each time. Group them in a transaction.
- **10 GiB a database.** A statement that would grow a key's database past it
  fails with SQLite's "database or disk is full", as an SQL error (a
  `TerminalError`), and the session goes on.
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
stored. It reads the state as of that call, a snapshot, whatever the
exclusive handler commits meanwhile; only a shared handler that outlives 16
of the exclusive handler's commits fails, and Restate retries it
([the reader](native/src/state/docs/DESIGN.md#readers)).
`ctx.transaction` is the exclusive handlers'.

```ts
count: restate.handlers.object.shared(async (ctx: restate.ObjectSharedContext) =>
  ctx.db.query('SELECT count(*) FROM accounts'),
),
```

## Not supported

Writes from shared handlers, awakeables in a handler with `ctx.db`,
and request/response deployments (such as Lambda), which suspend at every await,
since a suspension is a restart.

## Tests

The library's own: [`test/`](test) (sessions, transactions and their sends,
readers, SQL errors, lost writes) without a server
(`pnpm --filter restate-objects test`), the addon's in
[`native/`](native/README.md#build-and-test)
(`pnpm --filter restate-objects test:rust`), and
[`e2e/concurrency.ts`](../e2e/concurrency.ts) and
[`e2e/crash.ts`](../e2e/crash.ts) against one.
[`test/support.ts`](test/support.ts) runs a handler's attempts over an
object's state in memory, through crashes and fencing; `restate-agents`'s
tests use it too (`restate-objects/test/support`). `restate-objects/internal`
has what the tests take apart: an attempt's session over a context of their
own (`SqlAttempt`, `withSql`), a database over a store of their own
(`open`), and the store's fault injection (`setStoreWrapper`).
