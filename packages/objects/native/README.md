# The addon: an object's database in its state

The storage of [`restate-objects`](../README.md): SQLite-compatible SQL over
a key/value store implemented in JavaScript, the object's state. A native
addon ([napi-rs](https://napi.rs)), one Rust crate, with the [state and SQL
on it](#inside-the-layers) inside. [`src/database.ts`](../src/database.ts)
opens it over a store, and `ctx.db` builds that store on the object's state
([`src/session.ts`](../src/session.ts)):

```ts
const db = await open('Ledger/a', {
  get: (key) => ctx.get(key, serde.binary), // stored value, or this request's own write
  set: (key, value) => ctx.set(key, value, serde.binary),
  clear: (key) => ctx.clear(key),
  sync: () => ctx.run('sync', () => {}), // resolves once every write before it is stored
});
await db.transaction(async (tx) => {
  await tx.execute('INSERT INTO transfers VALUES(?, ?, ?, ?)', [id, from, to, amount]);
});
await db.save(); // ends the session, leaving its engine to the next one
```

## The store

`set` and `clear` return at once, and `sync` resolves once every write before
it is stored. Writes are stored in the order they were made: a crash keeps a
prefix of the writes since the last sync and loses the rest, never a write
without the ones before it (as a Restate object's state does). A `set` or
`clear` that throws fails the next `sync`. A transaction commits as a tail
entry and the head that names it (see the
[format](src/state/docs/FORMAT.md)), written after it, so a commit
is durable once a sync after it resolves. Now and then a commit first
checkpoints the database into a pack, which never changes, and drops the
tail.

## Sessions and the page cache

`open` returns a session of the namespace's database. A statement that commits
resolves once its writes are stored; `transaction` commits a body as a whole.
Calls run one at a time, in call order, and a transaction holds the session
until it commits or rolls back: calls made meanwhile wait, so none runs inside
it. The body uses the handle it gets, which is invalid once the body settles;
a call to the session from inside the body is refused, as it would wait for
the body. A COMMIT that fails rolls back. An SQL error leaves the session
usable (`isUsable()`); a storage error ends it.

A body may commit _effects_ with its transaction: `tx.commitEffects(bytes)`
puts them in the same tail entry, as the record `effects`, so they are
stored exactly when the transaction is, even if it changes nothing, and stay
until a later transaction commits others. `effects()` returns them. The SDK keeps a
transaction's sends there, its outbox, and makes them once it commits.

A page cache shared by the process's databases is built in: 256 MiB of memory
and 1 GiB of disk in a temporary file, unlinked once open so that it goes away
with the process, or the file `RESTATE_SQL_CACHE_FILE` names. It keeps a
database's packs, which never change, and its last head: a session reads the
ranges of packs it has from the cache, and reuses the idle engine when the
head is the one the cache saw last. `save()` (or `close()`) ends a session,
leaving its engine to the next one when it is idle. The cache keys packs by
their random names as well as the namespace: two stores
holding different databases under one namespace (two Restate clusters, say)
may share a process.

## Values

| SQL     | JavaScript                                                       |
| ------- | ---------------------------------------------------------------- |
| NULL    | `null` (`undefined` as a parameter)                              |
| INTEGER | `number`, or `bigint` beyond 2^53; a boolean parameter is 0 or 1 |
| REAL    | `number` (a non-integral parameter)                              |
| TEXT    | `string`                                                         |
| BLOB    | `Buffer` (any `Uint8Array` as a parameter)                       |

## Inside: the layers

Three layers, each knowing only the one below it. A change stays private to
the database until a call commits it, and the state's flush makes it durable.

```
host       restate-objects (TypeScript): ctx.db, transaction(), the effects
               │
               ▼
database   Database: SQL on a private view of one file; each call that
           leaves no SQL transaction open commits: the pages it changed and
           the effects, in one flush of its state
               │
               ▼
store      one Restate object's key/value state: get · put · delete · sync
```

| Layer        | Gives the layer above                                                                                                      | Keeps to itself                                                                                                   | Knows nothing of                    |
| ------------ | -------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------- | ----------------------------------- |
| **Store**    | `get`, `put`, `delete` and `sync` on one object's keys, writes in issue order; a crash keeps a prefix of the unsynced ones | Restate's own durability and fencing: one owner per object at a time                                              | What the keys mean                  |
| **Database** | `query`, `execute`, `begin`, `commit(effects)`, `rollback`, `save`, `close`: each commit durable when its call returns     | The engine, its private view of the file and its WAL; the state's format (a tail, the head, packs); a warm engine | Restate, and what the effects say   |
| **Host**     | The SDK API: `ctx.db` in an object handler                                                                                 | What goes in a transaction, and the effects to commit with it                                                     | How the database lays out its pages |

**When a change is durable.** When the call that commits it returns. A
statement outside a transaction commits on its own; one in a transaction,
with the transaction's `COMMIT` (or `commit(effects)`): the pages it changed
and the effects go into one mutation of the state, whose flush puts one tail
entry, then the head naming it.

**Failures.** An SQL error (syntax, a constraint) undoes only that statement.
A storage error, a cancelled call or a failed commit ends the session: the
flush's outcome is unknown, so the host settles outstanding requests and
opens a new one, reading whatever head last landed.

**Effects.** `commit(effects)` stores the effects with the rows, in the same
flush: the sends committed with them, an outbox, opaque to the storage.

All of it is one Rust crate, the addon, of two private modules: `state`
([`src/state`](src/state/README.md): its
[architecture](src/state/docs/ARCHITECTURE.md), [design](src/state/docs/DESIGN.md)
and [format](src/state/docs/FORMAT.md)), the storage of one file and the
effects, and `sql` ([`src/sql`](src/sql/README.md)), the `Database` on it.
[`src/node.rs`](src/node.rs) is the napi bindings, around `sql::Database`.

## Build and test

From `packages/objects`; at the repository's root, `pnpm build` builds the
addon, and `pnpm test` runs its tests with the rest:

```sh
pnpm build                          # release build, copied to native/objects.node
pnpm build:debug
pnpm test:rust                      # cargo test: the state and SQL, their API, crashes and fault campaigns
pnpm lint:rust                      # rustfmt, and clippy with and without the crate's features
cd native && cargo bench --features bench   # the benchmarks (benches/README.md)
```

`pnpm test` runs [`test/database.test.ts`](../test/database.test.ts) with
the package's other tests: SQL and value round trips, SQL errors and
rollback, a write the store rejects, and lost writes: for every position of
a request that loses its writes (its `sync` failing, or the session closed
first), the owner and a reader with no cached state read exactly the stored
rows. `pnpm test:rust` runs the state's and SQL's own tests, in the crate
(`src/state/tests`, `src/sql/tests`), with the simulated stores.
