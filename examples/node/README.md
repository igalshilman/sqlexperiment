# SQL in Restate virtual objects, without a journal

An experiment: a Restate virtual object whose handlers keep a SQL database in
the object's state, with journal-based recovery turned off. The storage
protocol of this repository provides durability and recovery; Restate provides
the state, single-writer execution per key, and retries. It uses the
[sqlite-kv addon](../../node/README.md) and a patched Restate TypeScript SDK
([`vendor/typescript`](../../vendor/README.md#typescript)).

```js
import * as restate from '@restatedev/restate-sdk';

const ledger = restate.object({
  name: 'Ledger',
  handlers: {
    init: (ctx) =>
      ctx.sql(async (db) => {
        await db.execute('CREATE TABLE IF NOT EXISTS accounts(id INTEGER PRIMARY KEY, balance INTEGER)');
      }),
    deposit: (ctx, { id, amount }) =>
      ctx.sql(async (db) => {
        await db.execute('UPDATE accounts SET balance = balance + ? WHERE id = ?', [amount, id]);
      }), // one transaction: durable as a whole once ctx.sql resolves
  },
  options: restate.sqlOptions, // journalReplay: false, lazy state, no journal retention
});
```

`ctx.sql(body)`, on the context of an exclusive object handler, opens the
object's database (namespace `service/key`) over the object's state, with the
process's page cache, runs `body` as one SQL transaction, and ends the session
with `save()` (one `ctx.run`, if it wrote, so the page cache may trust the
writes). An SQL error, or the body's own, is terminal; a storage error is
retried. Creating tables is up to the handlers, such as `init`.

`ctx.sqlSession(body)` keeps the database for longer work, such as a whole
handler: each statement is its own transaction, and `db.transaction(fn)`
groups several. A commit is durable once its writes are stored, which Restate
does in order, so it needs no barrier of its own: a lost commit takes
everything after it, including calls and the response. The session is saved
once, at the end. The agent example runs pi-durable's storage this way.

The store is internal to the SDK: `get` is `ctx.get`, `set` and `clear` are
`ctx.set` and `ctx.clear`, and the barrier is an empty `ctx.run`. The SDK loads
the sqlite-kv addon, an optional peer dependency, on first use.

## How it works

**State access.** A get is a state read, from the runtime unless this attempt
wrote the key (lazy state, so an invocation does not start by loading every
page). A set or clear is a state write, and returns at once. Restate stores an
invocation's commands in order, and after a crash keeps a prefix of them, so
writes are FIFO: a lost write takes every later command with it, including
later writes, calls to other services and the response. The protocol's
ordering rules (commit after the transaction's other segments, `meta` after
the checkpointed pages, deletes after `meta`) hold by that order, without
waiting for acknowledgements.

**A barrier only for the page cache.** The process's page cache keeps what it
wrote (the last commit, checkpointed pages, the parked engine) for the
object's next invocation, so it must not trust writes that may be lost. The
store is ordered: a session that wrote confirms its writes when it is saved,
with one empty `ctx.run`, which the runtime acknowledges after the commands
before it. If the attempt dies first, the next session in this process starts
cold. Without that barrier a lost checkpoint would stay claimed, and the next
save would record `meta` past log segments whose pages were never stored, and
delete them (`tests/integration/ordered.rs` in the main crate simulates
exactly that).

**No journal replay.** On a retry or a resume, the SDK normally replays the
journal: each operation returns its recorded result and must match it. That
cannot work here, since a later attempt makes different reads (the page cache,
the log length and the other owners' checkpoints all differ). With
`journalReplay: false` the SDK skips the recorded journal: the handler runs
again from the start against the state as it is. Recovery is then the storage
protocol's: open reads `meta` and the log, so a half-written commit or
checkpoint from the crashed attempt is ignored or replayed over.

**One owner.** The protocol needs one writer per namespace. Restate runs one
exclusive invocation per object key at a time, and one attempt of it at a
time, so the namespace is `service/key` and only exclusive handlers may open
it.

## The context without journal replay

A retried attempt runs the handler again from the start, so every context
operation it reaches happens again. What each one does then:

| Operation | On a retried attempt |
|---|---|
| `ctx.sql`, `ctx.sqlSession` | Run again, against the state as stored. A handler makes itself idempotent with a stable id it is given, such as an order id or a transfer id in the request: it records the id with its writes, and finds it the second time. |
| Calls (`ctx.objectClient(...)`, `ctx.serviceClient(...)`, `genericCall`) | Idempotent: the call gets an idempotency key derived from the invocation id and the request (target, input, and how many identical requests came before in this attempt). The same call reaches the invocation the earlier attempt started, and gets its result. |
| Sends, delayed sends | Idempotent in the same way: a repeated send is dropped, and a delayed send keeps its first timer. |
| `ctx.get`, `ctx.set`, `ctx.clear` | Read the state as stored; writes are ordered, and a lost one takes the later commands with it. |
| `ctx.run` | Runs again: its result is not kept. An empty `ctx.run` is a barrier: every write before it is stored once it resolves. |
| `ctx.sleep` | Starts over. |
| `attach`, `cancel` | Idempotent already. |
| Awakeables | Refused. |

An explicit idempotency key (`rpc.opts({ idempotencyKey })`) is used as
given. A derived key holds while Restate keeps the invocation: a day by
default (the target's idempotency retention). A retried attempt that makes a
different request (other input, because it read other state) starts a new
invocation, as it should.

Promises may be awaited concurrently: the patched SDK lifts the published
SDK's limit of one pending await per invocation.

## The examples

Each runs alone (`node shop.mjs`), or together (`npm run serve`, or
`node serve.mjs ledger shop` for some, on port 9080, or `PORT`):

- [`ledger.mjs`](ledger.mjs): accounts and transfers, deduped by transfer id.
- [`shop.mjs`](shop.mjs): a Warehouse object per SKU and an Orders object per
  order, each with its own database, over RPC. Placing an order reserves stock
  (a call) and schedules its expiry (a delayed send); paying commits the
  stock, and an unpaid order releases it when it expires.
- [`agent.mjs`](agent.mjs): a durable agent on
  [pi-durable](https://www.npmjs.com/package/@earendil-works/pi-durable)
  (`@earendil-works/pi-durable`), a durable agent harness that commits
  conversations, model turns, tool calls and their state to storage before
  anything is shown, and resumes unfinished work when the storage is opened
  again. An Agent object per session runs a pi-durable Harness whose storage
  is the object's database: the handler runs in one `ctx.sqlSession`, and
  [`pi-storage.mjs`](pi-storage.mjs) gives pi-durable's SQLite storage its
  `SqliteDatabase` facade over it (pi-durable's storage conformance suite
  passes on it). A retried attempt reopens the Harness and carries on:
  - a prompt is submitted once, by its id (pi-durable's `requestId`);
  - an answered prompt returns its answer;
  - unfinished work resumes: a model call is made again, and a tool call
    declared replay-safe runs again with the same call id.

  Its tools move money in the ledger, with the call id as the transfer id.
  `PI_MODEL=anthropic/claude-sonnet-4-5` (with `ANTHROPIC_API_KEY`) picks a
  model; by default a scripted teller answers "move 5 from 1 to 2" and
  "audit", with no key.

```sh
make examples-node   # from the repository root: the addon, the patched SDK, npm install
npm run serve
restate deployments register http://localhost:9080
curl localhost:8080/Ledger/bank/init --json '[8, 1000]'
curl localhost:8080/Agent/me/prompt --json '{"id": "p1", "text": "move 5 from 1 to 2"}'
curl localhost:8080/Orders/o1/place --json '{"sku": "apple", "qty": 2}'
```

## Validation

[`test.mjs`](test.mjs) runs every example against a real server through
crashes. The ledger and the shop run in one process, aborting at 1.5% of
state requests; the agents run in another, aborting at 0.3% (a prompt makes
some 200 state requests, so one attempt in three crashes). The server is
killed twice. Meanwhile:
- 60 orders are placed (a quarter twice), half of them paid, the rest
  expiring;
- four agent sessions take eight transfer prompts each (a third sent twice)
  and an audit.

It checks that each warehouse's stock balances and agrees with every order,
that each prompt moved money exactly once, and, from the server's
`sys_invocation` table, that there is exactly one `reserve` and one `transfer`
invocation per order and prompt, though their callers ran many attempts. It
passed twice, with 109 and 108 restarts of the shop's process and 85 and 92 of
the agents'.

```sh
RESTATE_SERVER=/path/to/restate-server make crash-test
```

200 sequential ledger transfers take 18.8 ms each on one local node.

## Caveats

- **Every read is journaled with its value.** A state read appends the value
  to the invocation's journal, so reading pages costs log writes, and the
  page cache matters more than it does elsewhere. `sqlOptions` sets journal
  retention to zero so completed journals are not kept.
- **The journal grows across attempts.** Skipped entries stay; each attempt
  appends its own.
- **A suspension is a restart.** Resuming means running from the start, so
  long handlers that hit the inactivity timeout start over.
- **Outside the model.** The TLA+ specification and the protocol monitor
  assume independent, acknowledged puts. Writes that return before they are
  stored are safe only by the FIFO argument above, which the crash tests test
  but nothing proves.
- **Assumptions to confirm with the runtime:** an attempt's stored commands
  are a prefix of those it sent, in order (FIFO writes); an empty `ctx.run` is
  acknowledged only after the commands before it are stored (the barrier); an
  abandoned attempt's commands are never applied after a new attempt starts
  (fencing); state a lazy read returns is durable. The crash tests kill
  processes, so they check ordering and recovery, not storage durability
  across a machine crash.

## Not supported

Shared handlers, awakeables, journal replay for the same service, and
request/response deployments (such as Lambda), which suspend at every await,
since a suspension is a restart.
