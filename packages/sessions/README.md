# restate-sessions

`restate-sessions` is the Restate TypeScript SDK plus _session objects_:
virtual objects where each key has its own SQL database, `ctx.db`. We call
them _sessions_ for short. The database is stored in the object's Restate
state, so there is no database server to run, and you still get what
Restate gives any virtual object: one writer per key, retries, and durable
calls to other services.

Session handlers don't use Restate's journal replay. When an attempt fails,
Restate runs the handler again from the start (the SDK calls this _explicit
recovery_), and the handler continues from whatever it has committed to its
database. The storage that makes this safe is a Rust addon in
[`native/`](native/README.md), built on Turso's SQLite-compatible engine.

It runs on a released Restate server and the published SDK, version 1.18.1,
which it pins ([why](#concurrent-awaits)). See
[the requirements](../../README.md#requirements).

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

## Defining a session object

`restate-sessions` re-exports `@restatedev/restate-sdk`, so session objects
sit next to your regular services, virtual objects and workflows.
`restate.sessionObject({ name, schema, handlers, views })` takes:

- **`handlers`**, which run one at a time per key and are the only ones that
  write. Each gets a `SessionContext`.
- **`views`**, which can run at any time, alongside the handlers, and only
  read. Each gets a `ViewContext`. See [views](#views).

A name is either a handler or a view, not both. Each one is a function, or
`{ handler, ...options }` with the SDK's usual handler options (`input`,
`output`, `description`, `ingressPrivate`, …). You only annotate the input;
`ctx` is inferred. `description`, `options` and `metadata` work as on any
virtual object. A session object also turns on lazy state, and hides the
database's state operations in Restate's UI.

SDK handlers (`restate.handlers.*`) aren't accepted. Journaled work belongs
in a regular `restate.object`, `restate.service` or `restate.workflow`, which
your session object can call.

### The contexts

The contexts are small and explicit, and a new one is built for each
attempt:

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
signals: the database is a session's only state. You can await their
operations concurrently, with `Promise.all` or `Promise.race`.

### Views

A view reads the session's database as committed, while handlers may be
committing at the same time. A chat UI can read a conversation while the
agent is still working on it, for example. A view takes a snapshot at its
first read and keeps reading that snapshot, whatever the handlers commit
afterwards. A view that is still running after 16 of their commits fails,
and Restate retries it ([how readers work](native/src/state/docs/DESIGN.md#readers)).

A view's `db` has no `execute` or `transaction`, and its context has no
`send`. A write through `query` fails too, because a view's database is
read-only.

```ts
views: {
  count: async (ctx) => ctx.db.get<{ accounts: number }>('SELECT count(*) AS accounts FROM accounts'),
},
```

## Retries

A handler runs in attempts. Restate starts a new attempt after a crash, a
retryable error or a suspension, and every attempt runs the handler from its
first line. A commit is durable once its promise resolves, and every later
attempt sees it. An error, terminal or not, doesn't undo what the handler
already committed.

So a handler's writes must be safe to run twice. The usual way is to record
an id that is the same on every attempt along with the writes, such as
`ctx.request.id` or an order id from the input, and check for it first.
Calls and sends are repeated on every attempt too, unless you give them an
idempotency key ([calls and sends](#calls-and-sends)).

What each operation does when a new attempt reaches it again:

| Operation  | On a new attempt                                                                                                        |
| ---------- | ----------------------------------------------------------------------------------------------------------------------- |
| `ctx.db`   | Runs again against the database as committed. Record a stable id with your writes to recognize work already done.       |
| `ctx.rpc`  | Makes a new call, unless it has an idempotency key; then it reaches the invocation the earlier attempt started.         |
| `ctx.send` | Sends again, unless it has an idempotency key; then Restate drops the repeat, and a delayed send keeps its first timer. |
| `tx.send`  | Sent once its transaction commits, and only once ([sends in a transaction](#sends-in-a-transaction)).                   |

## The database

`ctx.db` is the key's database. Its namespace is `service/key`, it is
stored in the object's state, and the process keeps a page cache of it.

- `query<T>(sql, params)` returns the rows of a statement as objects keyed
  by column name, and `get<T>` returns the first row or `undefined`. `T`
  describes a row but isn't checked at runtime. Integers above 2^53 come
  back as bigints, and booleans as 0 or 1. An expression needs `AS` to get a
  column name, and two columns with the same name are an error.
- `values(sql, params)` returns rows as arrays in column order, for adapters
  or for columns that share a name.
- `execute` runs one or more statements and returns no rows.

Outside a transaction, each statement is a commit of its own, and a
statement that changes nothing writes nothing. `ctx.db.transaction(tx => …)`
commits its body's statements together when the body resolves, and rolls
them back if it rejects. Every commit is one round trip to Restate, so a
transaction also makes a group of writes cheaper (one with sends costs one
more round trip, see [sends in a transaction](#sends-in-a-transaction)).

Calls to `ctx.db` run one at a time in the order you make them, even when
you await them together, and so do a transaction's statements. A
transaction holds `ctx.db` until it ends: inside its body, use `tx`, since a
call to `ctx.db` is refused.

If a crash happens between two commits, the first one stays. Statements that
must succeed or fail together belong in one transaction.

Libraries can keep their data in `ctx.db` too: the
[pi integration](../agents/src/pi/README.md) stores pi-durable's state there.

### Errors

An SQL error rejects with a `TerminalError` whose `cause` is the SQL error,
since a retry would fail the same way. That covers a statement the engine
rejects, a parameter it can't bind, and a call `ctx.db` refuses, including
the ones that close it: a `COMMIT` inside a transaction's body, or a
transaction still open when the handler returns. A storage error closes
`ctx.db` too, but it is retried. Errors thrown by your own code behave as in
any handler.

## The schema

`schema` lists the steps that create each key's database, in order. A step
is an SQL statement, or an async function that gets the transaction (to
move data, for example). The first attempt of a handler for a key applies
them in one transaction, when it opens the database.

The database records how far it got in a record next to its tables, which
commits together with the steps. So when you add a step later, such as a new
column or index, it runs once on each key's database. Never change a step
that has already run; add a new one instead. A view of a key that has never
been written sees empty tables.

Libraries that run in the database add their own tables. `restate-agents`
names its tables `restate_…`, so give yours other names.

## Calls and sends

`ctx.rpc(target, key?, options?)` returns a client for a service, object or
workflow, typed by its definition. Objects and workflows need a key and
services don't; getting that wrong throws a `TerminalError`. For a target
known only at runtime, use `ctx.rpc({ service, key, handler, input })`,
which encodes JSON by default. `ctx.send` works the same way for one-way
messages, and also takes a `delay`. The options apply to every call made
through the client:

```ts
const owner = await ctx.rpc(nickname, name, { idempotencyKey: `${ctx.request.id}:claim` }).claim(ctx.key);
ctx.send(audit).record(entry);
```

The SDK never makes up idempotency keys: a call has one only if you pass it.
With a key, a new attempt, including one resuming from a suspension, reaches
the invocation the first attempt started and gets its result. Build the key
from the request id and what the call is for. Restate scopes keys to the
target handler and remembers them for the target's idempotency retention, a
day by default.

A key names one call. If an invocation makes the same call several times,
once per tool call for example, add something to the key that tells them
apart. Restate attaches a call with a key it has already seen to the earlier
invocation, whatever its input, so a key only fits a call whose input is the
same on every attempt. If the input depends on what the attempt read, put
that in the key, or use no key.

## Sends in a transaction

A send made with `tx.send` goes out right after its transaction commits, and
never if it rolls back:

```ts
ctx.db.transaction(async (tx) => {
  await tx.execute('UPDATE accounts SET balance = balance - ? WHERE id = ?', [amount, from]);
  tx.send(store, 'a').reserve(item);
  tx.send(store, 'b', { delay: { hours: 1 } }).expire();
});
```

The sends are stored in the transaction's commit, in a record called
`outbox` next to the tables. After the commit, the attempt makes them, each
with the idempotency key `transaction/<id>/<index>` (or the send's own key),
where `<id>` is a random id of the transaction. Then, before `transaction()`
resolves, a second commit empties the outbox. Restate stores an attempt's
commands in order, so the sends are stored by the time that commit is. An
attempt that suspends later, to wait days for an approval for example, has
nothing left to send again when it resumes, long after the keys expired.

If an attempt ends between the two commits, the sends stay in the outbox.
The next exclusive attempt opens the database when it starts, and sends them
again with the same keys and whatever remains of their delays, which count
from the commit. Restate drops a send whose key it has already seen. So
every send of a committed transaction is made once, even if no later attempt
uses the database, and the sends of a transaction that rolled back or was
lost are never made.

A transaction without sends costs nothing extra; one with sends costs one
more commit. If the sends fail after the commit because the attempt was
replaced, `transaction()` rejects even though it committed: that attempt is
over, and the next one makes them.

A send doesn't return an invocation id. When you need a call's result, make
the call before the transaction, with an idempotency key of its own, and
record its outcome in the transaction. The shop's `Orders` does this, and its
`expire` is a `tx.send`.

## How it works

**Where the data lives.** The database is stored in the object's state,
under the keys `v22/head`, `v22/tail/…`, `v22/manifest/…` and `v22/pack/…`.
Reads are `ctx.get`, writes are `ctx.set` and `ctx.clear`, and a sync is an
empty `ctx.run`. Handlers and views can't reach any other state key, since
a session has no key/value state of its own.

**Reads and writes.** A read is a state read. It is journaled for now (once
protocol v8 ships it won't be, see [FUTURE.md](../../FUTURE.md)), and it goes
to the runtime unless this attempt wrote the key or the invocation started
with it. Sessions use lazy state, so an invocation doesn't start by loading
every page. A write or a clear is a state write, and returns at once.
Restate keeps an attempt's writes and sends in the order they were made, so
a crash can lose the last ones, but never a write without those before it.

**Commits.** A transaction writes the pages it changed, and its records, as
one tail entry under a key of its own. Then it writes the head, which points
to that entry; this is the commit point. One empty `ctx.run` then waits until
all of it is stored, so a commit is one round trip. Whatever a crash keeps,
the stored head only points to tail entries and checkpoints that are stored
too. Now and then, a commit first folds the tail into a checkpoint (see the
[state's architecture](native/src/state/docs/ARCHITECTURE.md)).

**The page cache.** The process caches the pages it read and wrote, and the
database's state as of its last head, for the key's next invocation.
Checkpoint packs never change, so the cache never invalidates them. An
attempt reads the stored head, and if another process wrote since, fetches
only the ranges the cache lacks. The head carries the database's random id,
which is part of the cache keys, so one process can serve several Restate
clusters, or a cluster that was wiped and recreated, without mixing up their
databases.

**No journal replay.** On a retry or a resume, the SDK normally replays the
journal: each operation returns its recorded result, and must match it.
That can't work here, because a later attempt makes different reads,
depending on what the page cache holds. With explicit recovery
(`restate.internal.explicitRecovery()`, which every session handler and view
has), the SDK skips the recorded journal, and the handler runs again from
the start against the state as it is. Recovery is then up to the storage:
opening the database reads the head, so a commit the crashed attempt didn't
finish is as if it never started.

**One writer.** The storage needs one writer per namespace. Restate runs one
exclusive invocation per key at a time, and one attempt of it at a time, so
the namespace is `service/key`, and only handlers write to it. Views read
alongside them.

## Concurrent awaits

You can await a context's calls (the SDK's `RestatePromise`s) and the
database's calls concurrently: with `Promise.all`, `Promise.race`, your own
combinators, several async functions at once, or a library's. The database's
own reads run concurrently too.

On its own, the published SDK drives each awaited promise with a loop of its
own, and lets only one of them wait for the server at a time
(`ExternalProgressChannel`). A second waiter fails the attempt with
`awaitNext already pending`, and it is retried forever. So session handlers
drive every promise they await with a single loop,
[the selector](src/selector.ts). Before the handler runs, it takes over the
invocation's `promisesExecutor.doProgress`, and gives it back when the
attempt ends. These are SDK internals as of 1.18.1, so the package pins that
version, and refuses to run a handler on an SDK without them.
[`test/selector.test.ts`](test/selector.test.ts) tests the selector against
a fake VM and the SDK's single-waiter channel.

What you get:

- **One await point.** The loop asks the server about all awaited promises
  at once. After a suspension, the handler resumes on the first of them to
  complete, and a promise awaited while the loop waits joins its next check,
  so a call awaited late isn't held up by a slow call made earlier.
- **An operation starts when you make it.** A call goes out without an
  `await`, and a failure nobody awaits is not an unhandled rejection.
- **Cancellation reaches every awaited promise**, and each one rejects with
  `CancelledError`.

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

[`e2e/concurrency.ts`](../e2e/concurrency.ts) tests all of this against a
real server.

## Examples

The smallest is [`examples/basic/notes.ts`](../examples/basic/notes.ts): a
handler and a view with `ctx.db`. Two more live in [`test/`](test), where
the tests use them:

- [`ledger.ts`](test/ledger.ts): accounts and transfers between them,
  deduplicated by transfer id.
- [`shop.ts`](test/shop.ts): two session objects that call each other, a
  `Warehouse` per SKU and `Orders` per order. Placing an order reserves stock
  (a call) and schedules the order's expiry (a delayed send). Paying commits
  the stock, and an unpaid order releases it when it expires.

Each one runs on its own (`node test/shop.ts`, from here) or together with
the others (`pnpm --filter e2e serve`, or `node serve.ts ledger shop` in
[`packages/e2e/`](../e2e) for a few), on port 9080 or `PORT`:

```sh
pnpm install && pnpm build   # from the repository root
pnpm --filter e2e serve                                            # the ledger and the shop, on port 9080
npx @restatedev/restate-server                                       # Restate, in another terminal
npx @restatedev/restate deployments register http://localhost:9080
curl localhost:8080/Ledger/bank/init --json '[8, 1000]'
curl localhost:8080/Warehouse/apple/restock --json '{"id": "first", "qty": 10}'   # orders need stock
curl localhost:8080/Orders/o1/place --json '{"sku": "apple", "qty": 2}'
```

Both run through crashes against a real server in
[the crash test](../e2e/crash.ts) (`pnpm --filter e2e crash`). On one local
node, 200 ledger transfers in a row take 18.8 ms each.

For an agent that keeps its state in `ctx.db`, see
[`restate-agents/pi`](../agents/src/pi/README.md), which runs pi-durable's
storage in the session's database.

## Caveats

- **Every read is journaled with its value.** A state read adds the value to
  the invocation's journal, so reading pages costs log writes, and the page
  cache matters more than usual. A completed invocation's journal, reads
  included, is kept for Restate's default retention.
- **The journal grows across attempts.** Entries an attempt skips stay, and
  each attempt adds its own.
- **A suspension is a restart.** Resuming means running from the start, so a
  long handler that hits the inactivity timeout starts over.
- **One sync per commit.** Every commit waits for an empty `ctx.run` after
  its head, a round trip to the runtime. Statements that commit one by one
  pay it each time, so group them in a transaction.
- **10 GiB per database.** A statement that would grow a key's database past
  that fails with SQLite's "database or disk is full", as an SQL error (a
  `TerminalError`), and `ctx.db` keeps working.
- **Assumptions still to confirm with the runtime.** An empty `ctx.run` is
  acknowledged only after the commands before it are stored (the sync). An
  abandoned attempt's commands are never applied after a new attempt starts
  (fencing). State that a lazy read returns is durable. The crash tests kill
  processes, so they check recovery, not durability across a machine crash.

## Not supported

- Key/value state, durable steps, timers and awakeables in a session object.
  Put them in a regular `restate.object`, `service` or `workflow`, and call
  it.
- Request/response deployments, such as Lambda. They suspend at every
  await, and for a session a suspension is a restart.

## Tests

- [`test/`](test): the library's own tests, without a server
  (`pnpm --filter restate-sessions test`): session objects, transactions and
  their sends, views, SQL errors and lost writes.
- [`native/`](native/README.md#build-and-test): the addon's tests
  (`pnpm --filter restate-sessions test:rust`).
- [`e2e/concurrency.ts`](../e2e/concurrency.ts) and
  [`e2e/crash.ts`](../e2e/crash.ts): against a real server.

[`test/support.ts`](test/support.ts) runs a handler's attempts over an
in-memory copy of the object's state, through crashes and fencing.
`restate-agents` uses it in its tests too, as `restate-sessions/test/support`.

`restate-sessions/internal` exposes what the tests take apart:

- `invoke`: run a handler for one attempt, over a context of your own;
- `contextOf`: a handler's or view's context over such a context;
- `Attempt` and `withAttempt`: an attempt's database;
- `open` and `asDatabase`: a database over a store of your own;
- `setStoreWrapper`: fault injection in the store.

It also has what this repository's other libraries need: `Schema` (tables a
library owns, applied where it uses them), `database` (another database of
the same key), `reset` (empty the key's database in a transaction), and
`cancelled` and `cancelCalls` (explicit cancellation, for a handler with
`explicitCancellation: true`).
