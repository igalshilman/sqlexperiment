# Node.js addon

SQL over a key/value store implemented in JavaScript: a native addon
([napi-rs](https://napi.rs)) around [SQL on the state](#inside-the-layers), for the Restate TypeScript
SDK, where the store is an object's state: [`restate-fns`](../fns/README.md)
builds `ctx.db` on it, and [`examples`](../examples) has the
examples.

```js
const { open } = require('restate-sql');

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

This is the shape of the store. With the Restate SDK, use `ctx.db` instead,
which builds the store inside the SDK.

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
puts them in the same tail entry, as the record `effects`, so they are stored exactly when the
transaction is, even if it changes nothing, and stay until a later
transaction commits others. `effects()` returns them. The SDK keeps a
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

Four layers, each knowing only the one below it. Every change climbs down the
same way: a layer keeps it private until the one above hands it on, and only
the state's flush makes it durable.

```
host     restate-fns (Node.js): flush after each SQL transaction
             │
             ▼
SQL      Sql: statements change a private view; Sql::flush(&mut mutation)
         moves what SQL committed into the mutation, as pages of the file `db`
             │
             ▼
state    records · files, read as last flushed; a Mutation collects
         changes, and its flush puts a tail entry, then the head: the commit
             │
             ▼
store    one Restate object's key/value state: get · put · delete · sync
```

| Layer     | Gives the layer above                                                                                                                           | Keeps to itself                                                                             | Knows nothing of                                      |
| --------- | ----------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------- | ----------------------------------------------------- |
| **Store** | `get`, `put`, `delete` and `sync` on one object's keys, writes in issue order; a crash keeps a prefix of the unsynced ones                      | Restate's own durability and fencing: one owner per object at a time                        | What the keys mean                                    |
| **State** | `State` to read, `Mutation` to change, `flush` to make a mutation durable at once; a `Receipt` says what became of one; `park` for a warm value | The format: a tail, the head naming it, checkpoints in packs, garbage collection, the cache | Who writes it, and when to flush                      |
| **SQL**   | `Sql`: `query`, `execute`, `transaction`; `flush(&mut mutation)` to hand over what SQL committed                                                | The engine, its private view of `db` and its WAL; a warm engine, parked                     | Other writers, and when to flush                      |
| **Host**  | The SDK API: `ctx.db` in an object handler                                                                                                      | When to flush, and what goes in the same mutation                                           | How the state lays out what it stores, or SQL's pages |

**When a change is durable.** At the state's flush, and only then. A
statement that succeeds is in SQL's private view; `Sql::flush` puts it in a
mutation, still in memory; the mutation's flush stores it with everything
else in that mutation, in one tail entry that one head names. A mutation
dropped, or flushed without `Sql::flush`, leaves SQL's changes private for a
later one: pairing the two is the host's.

**Failures.** An SQL error (syntax, a constraint) undoes only that statement.
A storage error, a cancelled call or a failed flush ends the `Sql` and the
`State` alike: the flush's outcome is unknown, so the host settles outstanding
requests and opens them again, reading whatever head last landed.

**The host.** `binding::Session`, the Node.js addon's under `restate-fns`,
pairs `Sql::flush` with the mutation's flush after each call that leaves no
SQL transaction open. In the same mutation it sets the record `effects`: the
sends committed with the rows, an outbox, opaque to the storage.

The crate is one Rust crate, the addon, with two private modules: `state`
([`src/state`](src/state/README.md): its [architecture](src/state/docs/ARCHITECTURE.md),
[design](src/state/docs/DESIGN.md) and [format](src/state/docs/FORMAT.md)) and
`sql` on it ([`src/sql`](src/sql/README.md)). `src/node.rs` is the napi
bindings, around `sql::binding::Session`.

## Build and test

```sh
npm run build      # release build, copied to sql.node (or `pnpm build`)
npm run build:debug
npm test           # node:test, against an in-memory store
cargo test --release   # the state and SQL: their API, crashes and fault campaigns
cargo bench --features bench   # the benchmarks (benches/README.md)
cargo run --release --features bench --example lookup
```

`npm test` covers SQL and value round trips, SQL errors and rollback, a write the
store rejects, and lost writes: for every position of a request that loses its
writes (its `sync` failing, or the session closed first), the owner and a
reader with no cached state read exactly the stored rows. `cargo test` runs
the state's and SQL's own tests, in the crate (`src/state/tests`,
`src/sql/tests`), with the simulated stores.
