# Node.js addon

SQL over a key/value store implemented in JavaScript: a native addon
([napi-rs](https://napi.rs)) around `sqlite-kv`, for the Restate TypeScript
SDK, where the store is an object's state: the patched SDK
([`packages/sdk-typescript`](../sdk-typescript/README.md#typescript)) builds `ctx.db` on it
([how it works](../sql/README.md)), and [`examples-typescript`](../../../examples-typescript) has the
examples.

```js
const { open } = require('sqlite-kv');

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
it is stored. The store keeps no order: until a sync resolves, a failure may
lose any of the writes before it, and keep any others. A `set` or `clear` that
throws fails the next `sync`. A database's writes are its commit record and
pages (see the crate's [format](../../../core/sqlite-kv/docs/FORMAT.md)): a
transaction commits with one put of the record, so a commit is durable once a
sync after it resolves.

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
puts them in the same commit record, so they are stored exactly when the
transaction is, even if it changes nothing, and stay until a later
transaction commits others. `effects()` returns them. The SDK keeps a
transaction's sends there, its outbox, and makes them once it commits.

A page cache shared by the process's databases is built in: 256 MiB of memory
and 1 GiB of disk in a temporary file, unlinked once open so that it goes away
with the process, or the file `SQLITE_KV_CACHE_FILE` names. It knows a
database by its commit record: a session opens on the cached pages when the
record is the one the cache saw last. `save()` (or `close()`) ends a session,
leaving its engine to the next one when it is idle. The cache knows a
database only by its namespace: two stores holding different databases under
one namespace (two Restate clusters, say) must not share a process.

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
npm run build      # release build, copied to sqlite_kv.node (or `make addon`)
npm run build:debug
npm test           # node:test, against an in-memory store
```

The tests cover SQL and value round trips, SQL errors and rollback, a write the
store rejects, and lost writes: for every position of a request that loses its
writes (its `sync` failing, or the session closed first), the owner and a
reader with no cached state read exactly the stored rows.
