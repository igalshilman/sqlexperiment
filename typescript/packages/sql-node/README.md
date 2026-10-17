# Node.js addon

SQL over a key/value store implemented in JavaScript: a native addon
([napi-rs](https://napi.rs)) around `sqlite-kv`, for the Restate TypeScript
SDK, where the store is an object's state: the patched SDK
([`packages/sdk-typescript`](../sdk-typescript/README.md#typescript)) builds `ctx.db` on it
([how it works](../sdk-typescript/SQL.md)), and [`examples`](../../examples) has the
examples.

```js
const { open } = require('sqlite-kv');

const db = await open('Ledger/a', {
  get: (key) => ctx.get(key, serde.binary), // stored value, or this request's own write
  set: (key, value) => ctx.set(key, value, serde.binary),
  clear: (key) => ctx.clear(key),
  sync: () => ctx.run('sync', () => {}), // resolves once every write so far is stored
});
await db.transaction(async (tx) => {
  await tx.execute('INSERT INTO transfers VALUES(?, ?, ?, ?)', [id, from, to, amount]);
});
await db.save(); // confirms the session's writes for the page cache
```

This is the shape of the store. With the Restate SDK, use `ctx.db` instead,
which builds the store inside the SDK.

## The store

The store is _ordered_, as Restate state writes are: `set` and `clear` return
at once, the store keeps writes in the order they were issued, and a failure
may lose a tail of unconfirmed writes, never one without the writes after it.
`sync` resolves once every write so far is stored.

Every call to the store goes through one queue (a single thread-safe
function), so the store sees them in the order the engine issued them: a write
before a later read of its key, and every write before a `sync`. A `set` or
`clear` that throws fails the next `sync`.

## Sessions and the page cache

`open` returns a session of the namespace's database. Each SQL commit is
durable once the store has stored its writes (a statement resolves once they
reached the store, and `save()` waits until they are stored); `transaction` commits a body as a whole. Calls
run one at a time, in call order, and a transaction holds the session until it
commits or rolls back: calls made meanwhile wait, so none runs inside it. The
body uses the handle it gets, which is invalid once the body settles; a call
to the session from inside the body is refused, as it would wait for the body.
A COMMIT that fails rolls back. An SQL error leaves the session usable
(`isUsable()`); a storage error ends it. `version()` is a number that grows
with each commit that changes the database, and only then; it does not wait
for queued calls, so a transaction's body can record it before its COMMIT, and
a later session can tell from its own whether that commit landed. A body that
may change nothing calls `tx.recordCommit()` for that: its commit is then
logged, and grows the version, all the same.

A page cache shared by the process's databases is built in: 256 MiB of memory
and 1 GiB of disk in a temporary file, unlinked once open so that it goes away
with the process, or the file `SQLITE_KV_CACHE_FILE` names. `save()` ends a
session and confirms its writes (one `sync`, if it wrote), so the cache may
trust them. A session that ends otherwise (`close()`,
a failed `save`, or garbage collection) makes the next session of that
namespace in the process start cold, since its writes may be lost. The logic
is the main crate's ordered-store mode. The cache knows a database only by its
namespace: two stores holding different databases under one namespace (two
Restate clusters, say) must not share a process.

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
npm test           # node:test, against an in-memory ordered store
```

The tests cover SQL and value round trips, SQL errors and rollback, a write the
store rejects, and lost writes: for every position of a request that loses its
writes (its `sync` failing, or the session closed first), the owner and a
reader with no cached state read exactly the stored rows.
