# SQL in Restate virtual objects, with the TypeScript SDK

The Node.js counterpart of [the Rust integration](../../restate/README.md): a
virtual object keeps a SQL database in its state, with journal replay turned
off. It uses the [sqlite-kv addon](../README.md) and a patched Restate
TypeScript SDK.

```js
import * as restate from '@restatedev/restate-sdk';
import { sql, sqlOptions } from './sql.mjs';

const ledger = restate.object({
  name: 'Ledger',
  handlers: {
    init: sql(async (ctx, db) => {
      await db.execute('CREATE TABLE IF NOT EXISTS accounts(id INTEGER PRIMARY KEY, balance INTEGER)');
    }),
    deposit: sql(async (ctx, db, { id, amount }) => {
      await db.execute('UPDATE accounts SET balance = balance + ? WHERE id = ?', [amount, id]);
    }), // one transaction: durable as a whole once the handler returns
  },
  options: sqlOptions, // journalReplay: false, lazy state, no journal retention
});
```

`sql(handler)` opens the object's database (namespace `service/key`), runs
the handler as one SQL transaction, ends the session with `save()` (one
`ctx.run`, if it wrote, so the page cache may trust the writes), and turns
errors into handler errors: an SQL error, or the handler's own, is terminal; a
storage error is retried. [`ledger.mjs`](ledger.mjs) is the Rust ledger,
ported.

## How the store maps to the SDK

| Store | SDK |
|---|---|
| `get` | `ctx.get(key, serde.binary)` |
| `set`, `clear` | `ctx.set`, `ctx.clear`: ordered, returning at once |
| `sync` | `ctx.run('sync', ...)`, acknowledged after the commands before it |

**One wait at a time.** The SDK waits on one thing at a time per invocation (a
second concurrent `await` of a context promise fails with "awaitNext already
pending"), while the engine reads pages concurrently. So the store issues each
request at once, which keeps the commands in the engine's order with respect
to `set` and `clear`, and awaits the requests issued together as one batch
(`RestatePromise.allSettled`), one batch after another.

## The SDK patches

`setup.sh` clones the SDK and its shared core at pinned revisions (the
TypeScript SDK 1.17.3, and the shared core revision it uses, with service
protocol V8), applies [`patches/`](patches), and builds:

- **shared core** (`sdk-shared-core.patch`): `JournalReplay::Disabled`, as in
  the Rust patch: a retried or resumed attempt skips the recorded commands,
  with fresh completion ids, and refuses awakeables.
- **TypeScript SDK** (`sdk-typescript.patch`): a `journalReplay` service and
  handler option, passed to the WebAssembly VM's new constructor argument; the
  bindings build against the patched core.
- The core is compiled to WebAssembly and inlined into the SDK with
  [`inline-wasm.mjs`](inline-wasm.mjs). The SDK's own inliner
  (wasm-pack-inline 0.1.4) slices wasm-bindgen's output at text positions that
  current wasm-bindgen no longer produces, and duplicates code.

```sh
./setup.sh   # needs cargo + wasm32-unknown-unknown, wasm-bindgen 0.2.121, node, pnpm
```

## Validation

The Rust crash campaign ([`restate/tests/crash.rs`](../../restate/tests/crash.rs))
runs this ledger with `LEDGER_COMMAND`: 120 transfers from six clients, a third
sent twice, the ledger aborting at random state requests, and the server killed
three times.

```sh
cd ../../restate
LEDGER_COMMAND="node $PWD/../node/restate/ledger.mjs" \
RESTATE_SERVER=/path/to/restate-server cargo test --test crash -- --nocapture
```

| Abort rate per state request | Ledger restarts | Result |
|---|---|---|
| 1.5% | 57 | All consistent |
| 4% | 394 | All consistent |

200 sequential transfers take 18.8 ms each on one local node (the Rust ledger
takes 14.5 ms).

## Not supported

As in Rust: shared handlers, awakeables, journal replay for the same service,
and request/response deployments (such as Lambda), which suspend at every
await, since a suspension is a restart.
