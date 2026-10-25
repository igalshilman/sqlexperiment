# restate-fns: SQL in Restate virtual objects

Each key of a Restate virtual object gets a SQL database of its own, `ctx.db`,
kept in the object's state. The handlers that use it run with the SDK's
explicit recovery instead of journal replay: the storage protocol of this
repository provides durability and recovery, and Restate provides the state,
single-writer execution per key, and retries. It uses the
[SQL addon](../sql/README.md), on the published Restate TypeScript
SDK (1.17.4, pinned: see [concurrent awaits](#concurrent-awaits-one-await-point)).

It runs on a released Restate server: see [the requirements](../../../README.md#requirements).

```ts
import * as restate from 'restate-fns';

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

`restate-fns` is the Restate SDK (it re-exports `@restatedev/restate-sdk`),
with the virtual objects of SQL: in `restate.object`, each function in
`handlers` is an exclusive handler with `ctx.db`;
`restate.handlers.object.shared(fn)` makes a shared one, and
`restate.handlers.object.exclusive(options, fn)` one with handler options
(such as `input`). With `journalReplay: true` in its options, a handler is
the SDK's own: it replays its journal and has no `ctx.db` (its context is
`restate.JournaledObjectContext`), such as one that waits on an awakeable.
The object's options are `restate.sqlOptions` (lazy state, no journal kept),
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
`ctx.transaction` (a read of the database's root), stays open while the
handler runs, however many calls it makes, and is saved when the handler
ends. A transaction commits with one write of the root, after the blobs it
names, and is durable when the statement or `ctx.transaction` that commits
it resolves: the SDK waits for an empty `ctx.run` after the write. A library
that wants a database for the handler's work gets `ctx.db`: the
[pi integration](../integrations/src/pi/README.md) runs pi-durable's storage
this way.

An SQL error rejects as a `TerminalError` (its `cause` the SQL error), from
`ctx.db` and `ctx.transaction` alike, since a retry would repeat it; a storage
error, which ends the session, is retried. The handler's own errors are as in any handler: a `TerminalError` is
terminal, and any other error, such as a failed request to a model or another
service, is retried.

**The schema.** `schema` in `restate.object` is the steps that make each
key's database, in order: statements, or async functions of the transaction
(to move data). They are applied once per key, in one transaction, when a
handler of the key first opens its database; the table `restate_schema`
keeps how far each database got, so a step added later (a column, an index)
runs on each key's database once, as that key is next opened. A step never
changes once it ran: a change is a step of its own. A shared handler applies
the steps its key lacks in its own session, which stores nothing: a key never
written reads as empty tables. Tables whose names start with `restate_` are
Restate's (`restate_schema`, and those of the agents of restate-agents): name
yours otherwise.

**Sends that commit with the SQL.** A send made with `tx`, the transaction's
send clients, is made once the transaction commits, and only then:

```js
ctx.transaction(async ({ db, tx }) => {
  await db.execute('UPDATE accounts SET balance = balance - ? WHERE id = ?', [amount, from]);
  tx.objectSendClient(store, 'a').reserve(item);
  tx.objectSendClient(store, 'b').deposit(amount);
});
```

The sends commit with the transaction, in the same write: the root holds
them (its _effects_, opaque to the storage), as the database's outbox.
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

**Steps: what the invocation keeps across its attempts.** A retried attempt
runs the handler again from the start, so a request to a model, say, would be
made again. `ctx.step(name, body)` runs `body` once for the invocation:

```ts
const answer = await ctx.step('ask', () => model.ask(question));
```

A step is a transaction like `ctx.transaction`'s: `body` gets `{ db, tx }`,
and its result (as JSON) commits with its SQL and its sends, in one write. A
later attempt of the invocation that reaches the step gets the stored result,
without running `body`; one that crashed before the commit runs it again. So a
step's SQL and its result are stored together or not at all:

```ts
await ctx.step('charge', async ({ db }) => {
  const receipt = await payments.charge(order);
  await db.execute('UPDATE orders SET paid = ? WHERE id = ?', [receipt.id, order.id]);
  return receipt;
});
```

The transaction begins at the body's first statement, not before it runs: steps
that only make requests run concurrently (`Promise.all`), and commit one after
the other. A name is one step of the invocation, so a step in a loop adds what
tells its runs apart (`` `ask ${turn}` ``); a name used twice in one attempt is
a terminal error.

The steps live in the table `restate_steps` of the object's database, by the
invocation's id (`ctx.request().id`, the same on every attempt) and name. They
are kept until the invocation ends, with its result or a terminal error, and
its end drops the table: the object runs one exclusive invocation at a time,
so every row then belongs to one that ended (an invocation killed without
ending leaves its rows to the end of the next one that uses the database). A
step is for exclusive handlers: a shared one stores nothing.

The store is the object's state: `get` is `ctx.get`, `set` and `clear` are
`ctx.set` and `ctx.clear` (each named after its key), and `sync` is an empty
`ctx.run`.

**The database is the object's state.** Its keys (`v20/head`, `v20/tail/…`,
`v20/manifest/…` and, without S3, `v20/pack/…`) sit next to the object's
other state keys, so `ctx.clearAll()` in any handler of the object deletes
the database, a handler's own key starting with `v20/` corrupts it, and
`ctx.stateKeys()` lists them. Keep other state out of `v20/`, and do not
clear all of an object that uses `ctx.db`. (Reserving
these keys in the SDK is future work.)

## How it works

**State access.** A get is a state read (journaled for now; left out of the
journal once protocol v8 ships, see FUTURE.md), from the runtime unless this attempt wrote the key or
the invocation started with it (lazy state, so an invocation does not start
by loading every page). A set or clear is a state write, and returns at once.
Restate keeps an attempt's writes, and its sends, in the order they were
made: a crash loses a suffix of them, never a write without the ones before
it. A transaction writes the chunks of pages it changed as new blobs, which
are never overwritten, then the root, which names them: the commit point.
Then one empty `ctx.run` waits until all of it is stored: a commit is one
round trip. Whatever prefix a crash keeps, the stored root names only blobs
that are stored.

**The page cache.** The process's page cache keeps the pages it read and
wrote, as the blobs that hold them, for the object's next invocation. A blob
never changes once a root names it, so the cache keeps it under its number
and never invalidates it: a session reads the stored root, then the blobs it
names that the cache lacks. The root carries the database's random id, which
the cache's keys include, so one process may serve several Restate clusters,
or a cluster wiped and made again, without mixing their databases.

**No journal replay.** On a retry or a resume, the SDK normally replays the
journal: each operation returns its recorded result and must match it. That
cannot work here: a later attempt makes different reads, since what it
fetches depends on what the page cache holds. With explicit
recovery (`restate.internal.explicitRecovery()`, which the handlers here
have) the SDK skips the recorded journal: the handler runs again from the
start against the state as it is. Recovery is then the storage
protocol's: open reads the root, so a lost commit from the crashed attempt is
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
| `ctx.step`                                                               | Returns the result a previous attempt committed, without running its body; runs it if none did.                                                                                                                               |
| Calls (`ctx.objectClient(...)`, `ctx.serviceClient(...)`, `genericCall`) | A new call, unless it has an idempotency key: with one that is the same on every attempt, the call reaches the invocation the earlier attempt started, and gets its result.                                                   |
| Sends, delayed sends                                                     | The same: with an idempotency key, a repeated send is dropped, and a delayed send keeps its first timer.                                                                                                                      |
| `ctx.get`, `ctx.set`, `ctx.clear`                                        | Read the state as stored; a write is stored once an empty `ctx.run` after it resolves, and a crash before that may lose it.                                                                                                   |
| `ctx.run`                                                                | Runs its action again: its result is not kept from one attempt to the next (use `ctx.step` for that). (The store uses an empty run, to sync.)                                                                                 |
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
[`tests/sql/selector.test.ts`](../../tests/sql/selector.test.ts) tests the
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

[`tests/concurrency.ts`](../../tests/concurrency.ts) tests all of this against
a real server.

## The examples

The smallest is [`examples-typescript/basic/notes.ts`](../../../examples-typescript/basic/notes.ts): plain SQL
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
make js   # from the repository root: the addon, pnpm install
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
  page cache matters more than it does elsewhere. `restate.sqlOptions` sets journal
  retention to zero so completed journals are not kept.
- **The journal grows across attempts.** Skipped entries stay; each attempt
  appends its own.
- **A suspension is a restart.** Resuming means running from the start, so
  long handlers that hit the inactivity timeout start over.
- **A sync per commit.** Every transaction waits for an empty `ctx.run` after
  its root: a round trip to the runtime, which statements that commit one
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
([the reader](../../../core/state/docs/DESIGN.md#readers)).
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

The library's own: [`tests/sql`](../../tests/sql) (sessions, transactions
and their sends, readers, SQL errors) without a server, and
[`tests/concurrency.ts`](../../tests/concurrency.ts) and
[`tests/crash.ts`](../../tests/crash.ts) against one. `restate-fns/internal`
has what the tests take apart: an attempt's session over a context of their
own (`SqlAttempt`, `withSql`), and the store's fault injection
(`setStoreWrapper`).
