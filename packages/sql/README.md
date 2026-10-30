# Node.js addon

SQL over a key/value store implemented in JavaScript: a native addon
([napi-rs](https://napi.rs)) around the [`sql`](../../core/sql/README.md) crate, for the Restate TypeScript
SDK, where the store is an object's state: [`restate-fns`](../fns/README.md)
builds `ctx.db` on it, and [`examples`](../../examples) has the
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
entry and the head that names it (see the crate's
[format](../../core/state/docs/FORMAT.md)), written after it, so a commit
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

## Build and test

```sh
npm run build      # release build, copied to sql.node (or `make addon`)
npm run build:debug
npm test           # node:test, against an in-memory store
```

The tests cover SQL and value round trips, SQL errors and rollback, a write the
store rejects, and lost writes: for every position of a request that loses its
writes (its `sync` failing, or the session closed first), the owner and a
reader with no cached state read exactly the stored rows.
