# restate-sessions: SQL in Restate virtual objects

Each key of a session object, a Restate virtual object, gets a SQL database
of its own, `ctx.db`, kept in the object's state. Its handlers run with the
SDK's explicit recovery instead of journal replay: the storage protocol of this
repository provides durability and recovery, and Restate provides the state,
single-writer execution per key, and retries. Its storage is
a Rust addon in [`native/`](native/README.md), on the published Restate TypeScript
SDK (1.17.4, pinned: see [concurrent awaits](#concurrent-awaits-one-await-point)).

It runs on a released Restate server: see [the requirements](../../README.md#requirements).

```ts
import * as restate from 'restate-sessions';

const ledger = restate.sessionObject({
  name: 'Ledger',
  schema: ['CREATE TABLE accounts(id INTEGER PRIMARY KEY, balance INTEGER)'], // applied once per key
  handlers: {
    deposit: (ctx, { id, amount }: { id: number; amount: number }) =>
      ctx.db.transaction(async (tx) => {
        await tx.execute('UPDATE accounts SET balance = balance + ? WHERE id = ?', [amount, id]);
      }), // one transaction: durable as a whole once it resolves
  },
  views: {
    balance: async (ctx, id: number) =>
      ctx.db.get<{ balance: number }>('SELECT balance FROM accounts WHERE id = ?', [id]),
  },
});
```

`restate-sessions` is the Restate SDK (it re-exports `@restatedev/restate-sdk`),
with session objects beside its own objects, services and workflows.
`restate.sessionObject({ name, schema, handlers, views })` is a virtual object
whose state is a SQL database per key:

- **`handlers`** run one at a time per key, each with a `SessionContext`;
- **`views`** run beside them, at any time, each with a `ViewContext`: they
  read the database as committed at their first read, and write nothing
  (a write fails: the database is query-only).

A name is a handler or a view, not both. Each is a function, or
`{ handler, ...options }` with the SDK's handler options (`input`, `output`,
`description`, `ingressPrivate`...). `ctx` is inferred: only the input is
annotated. `description`, `options` and `metadata` are the SDK's object
fields; a session object adds lazy state, and hides the database's state
commands in Restate's UI. An SDK handler (`restate.handlers.*`) is refused:
journaled work goes in a plain `restate.object`, `restate.service` or
`restate.workflow`, which a session object calls.

**The contexts** are short, explicit lists, built for each attempt, not the
SDK's context:

```ts
interface SessionContext {
  readonly key: string;
  readonly request: restate.Request; // id (the same on every attempt), headers, target...
  readonly db: Database;
  readonly rpc: Rpc;
  readonly send: Send;
}

interface ViewContext {
  readonly key: string;
  readonly request: restate.Request;
  readonly db: DatabaseReader;
  readonly rpc: Rpc;
}
```

Neither has key/value state, `run`, `sleep`, awakeables, promises or
signals: the database is a session object's only state. Their operations can
be awaited together (`Promise.all`, `Promise.race`).

**Retries.** A handler runs in attempts: Restate starts a new one after a
crash, a retryable error or a suspension, and each runs the handler from its
first line (explicit recovery: no journal replay). A commit is durable when
its promise resolves, and the next attempt finds it; an error, terminal or
not, keeps what the handler committed. So a handler's writes must be safe to
run again: key them by an id that is the same on every attempt, such as
`ctx.request.id`, or an id in the input. Calls and sends are made again by
every attempt, unless they have an idempotency key.

**The database.** `ctx.db` is the key's database (namespace `service/key`)
over the object's state, with the process's page cache.

- `query<T>(sql, params)` returns the rows of one statement as objects by
  column name, and `get<T>` the first, or `undefined`. `T` declares a row's
  shape without checking it; integers past 2^53 come back as bigints, and
  booleans as 0 or 1. An expression needs `AS` to be named, and two columns
  of one name are an error. `values(sql, params)` returns the rows as
  arrays, in column order: for adapters, and for columns that share a name.
  `execute` runs one or more statements, without rows.
- Each statement outside a transaction is its own commit; one that changes
  nothing writes nothing. `ctx.db.transaction(tx => ...)` commits several
  as one: when its body resolves, or rolls back when it rejects. Each commit
  is one round trip, so writes grouped in a transaction cost one (one more
  if it sends: see the outbox, below).
- Calls run one at a time, in call order; a transaction's statements too,
  even awaited together. A transaction holds `ctx.db` until it ends: a call
  to `ctx.db` from inside its body is refused (use its `tx`).

A crash between two commits keeps the first: statements that must stand or
fall together go in one transaction. A library that wants a database for
the handler's work gets `ctx.db`: the [pi integration](../agents/src/pi/README.md)
runs pi-durable's storage this way.

An SQL error rejects as a `TerminalError` (its `cause` the SQL error), since
a retry would repeat it: a statement the engine rejects, a parameter it
cannot bind, or a call the session refuses, even one that ends the session
(a `COMMIT` inside a transaction's body, a transaction left open when the
handler ends). A storage error, which ends the session, is retried. The
handler's own errors are as in any handler.

**The schema.** `schema` is the steps that make each key's database, in
order: statements, or async functions of the transaction (to move data).
They are applied once per key, in one transaction, by the first attempt
of a handler of the key, which opens its database as it starts. How far each database got is a record beside
its tables, committed with the steps, so a step added later (a column, an
index) runs on each key's database once. A step never changes once it ran:
a change is a step of its own. A view of a key never written reads empty
tables. The database holds your tables, and those of the libraries you run
in it: restate-agents names its agents' tables `restate_…`, so name yours
otherwise.

**Calls and sends.** `ctx.rpc(target, key?, options?)` is a client of a
service, object or workflow definition, typed by it: its methods take their
input, if any. An object or a workflow needs its key, a service has none.
`ctx.rpc({ service, key, handler, input })` calls a target known only at run
time (JSON by default). `ctx.send` is the same, for sends, with a `delay`.
The options go with the target, and apply to every call of the client:

```ts
const owner = await ctx.rpc(nickname, name, { idempotencyKey: `${ctx.request.id}:claim` }).claim(ctx.key);
ctx.send(audit).record(entry);
```

The SDK makes up no keys: a call has an idempotency key only when the code
gives it one, and then a new attempt (including one that resumes a
suspension) reaches the call the first one made, and gets its result. Build
it from the request id and what the call is for. Restate scopes a key to the
target handler, and keeps it for the target's idempotency retention (a day
by default).

**Sends that commit with the SQL.** A send made with a transaction's
`tx.send` is made right after it commits, and never if it rolls back:

```ts
ctx.db.transaction(async (tx) => {
  await tx.execute('UPDATE accounts SET balance = balance - ? WHERE id = ?', [amount, from]);
  tx.send(store, 'a').reserve(item);
  tx.send(store, 'b', { delay: { hours: 1 } }).expire();
});
```

The sends commit with the transaction, in the same tail entry: the
database's record `outbox` holds them (beside its tables, opaque to the
storage). Once the commit is done the session makes them, each with an
idempotency key of the transaction, `transaction/<id>/<index>` (or the
send's own key), with a random id of the transaction. Restate keeps an
attempt's commands in the order they were made, so a send is stored
whenever a commit made after it is: right after making them, the
transaction empties the outbox with a commit of its own, before
`transaction()` resolves. So an attempt that suspends later (a wait for an
approval, say) leaves nothing to make again when it resumes, days later and
past the retention of their keys. An attempt that ends between the two
commits leaves them in the outbox: the next exclusive attempt opens its
database as it starts, and makes them again, with the same keys and what
remains of their delays, counted from the commit; Restate drops a send whose key it has seen. So each
send of a committed transaction is made once, even when a new attempt never
uses the database, and a send of a transaction rolled back or lost is not
made. A transaction without sends costs nothing more; one with sends costs
one more commit. If making its sends fails after the commit (the attempt was
replaced), `transaction()` rejects though it committed: the attempt is
over, and the next one makes them. A send has
no invocation id. Make calls before a transaction, with an idempotency key
of their own, and record their outcome in it, as the shop's `Orders` does;
its `expire` is a send of `tx`.

The store is the object's state: `get` is `ctx.get`, `set` and `clear` are
`ctx.set` and `ctx.clear` of the SDK's context (each named after its key),
and `sync` is an empty `ctx.run`.

**The database is the object's state.** Its keys (`v22/head`, `v22/tail/…`,
`v22/manifest/…` and `v22/pack/…`) sit next to the object's
other state keys, which no handler or view of a session object reaches: it
has no key/value state.

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
recovery (`restate.internal.explicitRecovery()`, which every handler and
view of a session object has) the SDK skips the recorded journal: the
handler runs again from the start against the state as it is. Recovery is then the storage
protocol's: open reads the head, so a lost commit from the crashed attempt is
as if it never started.

**One owner.** The protocol needs one writer per namespace. Restate runs one
exclusive invocation per object key at a time, and one attempt of it at a
time, so the namespace is `service/key` and only handlers write it; views
read it beside them.

## The context with explicit recovery

A new attempt runs the handler again from the start, so every operation it
reaches happens again. What each one does then:

| Operation  | On a new attempt                                                                                                                                                                                                                     |
| ---------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `ctx.db`   | Runs again, against the database as committed. A handler makes itself idempotent with a stable id it is given, such as an order id or a transfer id in the request: it records the id with its writes, and finds it the second time. |
| `ctx.rpc`  | A new call, unless it has an idempotency key: with one that is the same on every attempt, the call reaches the invocation the earlier attempt started, and gets its result.                                                          |
| `ctx.send` | The same: with an idempotency key, a repeated send is dropped, and a delayed send keeps its first timer.                                                                                                                             |
| `tx.send`  | Made once its transaction commits, and once only: see above.                                                                                                                                                                         |

A key names one call: one that an invocation makes several times (a tool
call) adds what tells them apart. Restate attaches a call with a key it has
seen to the earlier invocation, whatever its input, so the key fits a call
whose input is the same on every attempt; one whose input depends on what
the attempt read needs that in its key, or none.

### Concurrent awaits, one await point

A context's calls are the SDK's `RestatePromise`s, and they and the
database's calls can be awaited concurrently: `Promise.all`, `Promise.race`, your own combinators,
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
  joins the next check: a call awaited late is not held up by a long call
  made earlier.
- **An operation starts when it is made**: a call is made without an `await`,
  and a failure nobody awaits is not an unhandled rejection.
- **A cancellation reaches every awaited promise** (each rejects with
  `CancelledError`).

```ts
const quote = restate.sessionObject({
  name: 'quote',
  handlers: {
    price: async (ctx, sku: string) => {
      const [price, stock] = await Promise.all([
        ctx.rpc(pricing).quote(sku),
        ctx.db.get<{ count: number }>('SELECT count FROM stock WHERE sku = ?', [sku]),
      ]);
      return { price, inStock: (stock?.count ?? 0) > 0 };
    },
  },
});
```

[`e2e/concurrency.ts`](../e2e/concurrency.ts) tests all of this against
a real server.

## The examples

The smallest is [`examples/basic/notes.ts`](../examples/basic/notes.ts):
a handler and a view with `ctx.db`. The ledger and the shop are in [`test/`](test), where
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
- **A sync per commit.** Every commit waits for an empty `ctx.run` after
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

## Views

A view's `ctx.db` is the object's database as committed, read beside the
handlers, which may commit meanwhile (a chat UI's read of a conversation
while the agent works, say). It opens as of its first read, and reads that
snapshot throughout, whatever the handlers commit meanwhile; only a view
that outlives 16 of their commits fails, and Restate retries it
([the reader](native/src/state/docs/DESIGN.md#readers)). Its `db` has no
`execute` or `transaction`, and its context no `send`; a write through
`query` fails, as its session is query-only.

```ts
views: {
  count: async (ctx) => ctx.db.get<{ accounts: number }>('SELECT count(*) AS accounts FROM accounts'),
},
```

## Not supported

Key/value state, durable steps, timers and awakeables in a session object
(put them in a plain `restate.object`, `service` or `workflow`, and call it),
and request/response deployments (such as Lambda), which suspend at every
await, since a suspension is a restart.

## Tests

The library's own: [`test/`](test) (sessions, transactions and their sends,
readers, SQL errors, lost writes) without a server
(`pnpm --filter restate-sessions test`), the addon's in
[`native/`](native/README.md#build-and-test)
(`pnpm --filter restate-sessions test:rust`), and
[`e2e/concurrency.ts`](../e2e/concurrency.ts) and
[`e2e/crash.ts`](../e2e/crash.ts) against one.
[`test/support.ts`](test/support.ts) runs a handler's attempts over an
object's state in memory, through crashes and fencing; `restate-agents`'s
tests use it too (`restate-sessions/test/support`). `restate-sessions/internal`
has what the tests take apart: a handler run for one attempt over a context
of their own (`invoke`), an attempt's session (`Attempt`, `withAttempt`), a
database over a store of their own (`open`, `asDatabase`), and the store's
fault injection (`setStoreWrapper`). For this repository's libraries, it has
`Schema` (tables a library owns, applied where it uses them), `database`
(another database of the key), `reset` (empty the key's database in a
transaction) and `cancelled`/`cancelCalls` (explicit cancellation).
