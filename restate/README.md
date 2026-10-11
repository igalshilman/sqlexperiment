# SQL in Restate virtual objects, without a journal

An experiment: a Restate virtual object whose handlers keep a SQL database in
the object's state, with journal-based recovery turned off. The storage
protocol of this repository provides durability and recovery; Restate provides
the state, single-writer execution per key, and retries.

```rust,ignore
#[restate_sdk::object]
impl Ledger {
    #[handler]
    async fn transfer(&self, ctx: ObjectContext<'_>, t: Json<Transfer>) -> HandlerResult<bool> {
        ctx.with_sql(async |db| {
            // ... SELECT, INSERT, UPDATE ...
        })
        .await // one transaction: durable as a whole once this returns Ok
    }
}

// Once per process: the page cache and each service's schema.
Sql::new()
    .cache(PageCache::open(PageCacheConfig::new(file, 256 << 20, 8 << 30)).await?)
    .schema("Ledger", "CREATE TABLE accounts(...); CREATE TABLE transfers(...);")
    .install();
Endpoint::builder()
    .bind(Ledger.into_service_definition().options(sqlite_kv_restate::service_options()))
```

`with_sql` opens the object's database (namespace `service/key`), creates
the schema when the database has no tables, runs the body as one SQL
transaction, and turns errors into handler errors: an SQL error, or the body's
own, is terminal, since a retry would repeat it; a storage error is retryable.

[`src/bin/ledger.rs`](src/bin/ledger.rs) is a complete object: accounts and
transfers, each transfer recorded with its id in the same SQL transaction as
the balances it moves.

## How it works

**State access.** `ObjectState` is a `KvStore` over the state of one object
key. A get is a state read, from the runtime unless this attempt wrote the key
(lazy state, so an invocation does not start by loading every page). A put or
delete is a state write, and returns at once. Restate stores an invocation's
commands in order, and after a crash keeps a prefix of them, so writes are
FIFO: a lost write takes every later command with it, including later writes,
calls to other services and the response. The protocol's ordering rules
(commit after the transaction's other segments, `meta` after the checkpointed
pages, deletes after `meta`) hold by that order, without waiting for
acknowledgements.

**Barrier only when needed.** Something that outlives the attempt must not
trust writes that may be lost. Two things do:
- **The page cache.** It keeps what this process wrote (the last commit, a
  checkpoint not yet recorded in `meta`, checkpointed pages, the parked
  engine) for the object's next invocation. So after a session that wrote,
  `with_sql` waits for one barrier: an empty `ctx.run`, which the runtime
  acknowledges after the commands before it. If the attempt dies before that,
  the next session in this process resets the namespace's cache and starts
  cold.
- **Effects outside Restate**, such as a request from `ctx.run` to another
  system. `sync(&ctx)` waits for the same barrier first.

Without the cache's barrier, a lost checkpoint stays claimed: the next save
records `meta` past log segments whose pages were never stored, and deletes
them. A simulation of exactly that (`tests/integration/ordered.rs` in the main
crate) corrupts the database for every other reader.

**No journal replay.** On a retry or a resume, the SDK normally replays the
journal: each call returns its recorded result and must match it. That cannot
work here, since a later attempt makes different reads (the page cache, the
log length and the other owners' checkpoints all differ). With
`journal_replay(false)` the SDK skips the recorded journal: the handler runs
again from the start, and every call executes anew against the state as it is.
Recovery is then the storage protocol's: open reads `meta` and the log, so a
half-written commit or checkpoint from the crashed attempt is ignored or
replayed over.

**Durability.** Sessions use `Durability::Commit`, and `with_sql` runs its
body as one transaction: it is durable as a whole when it returns.

**Semantics.** A handler runs at least once: a crash after a commit and before
the response reruns it. Effects on the database become exactly once by
recording a request id in the same transaction and checking it first, as the
ledger does. Calls to other services, `ctx.run` and other side effects are at
least once, with no recorded results.

**One owner.** The protocol needs one writer per namespace. Restate runs one
exclusive invocation per object key at a time, and runs one attempt of it at a
time, so the namespace is `service/key` and only exclusive handlers
(`ObjectContext`) may open it. Shared handlers would read while a writer
checkpoints, and are not supported.

## SDK changes

The SDK is vendored (`vendor/`, restate-sdk 0.12.1 and restate-sdk-shared-core
7.0.3) and patched; the first commit on the branch is the unmodified copy, so
`git diff` shows the patches. They build together through this crate's
`[patch.crates-io]`.

| Change | Where |
|---|---|
| `JournalReplay::Disabled`: after the input, skip the recorded commands; start completion ids at `1 + 2 × known entries` (a command takes at most two, so ids of earlier attempts are never reused); refuse awakeables, whose signal ids cannot be made fresh | shared-core `VMOptions`, `SysInput`, `Journal::skip_replayed` |
| `ServiceOptions::journal_replay(bool)`, recorded in the service metadata and applied to each invocation's VM | SDK `endpoint/builder.rs`, `endpoint/mod.rs` |
| `ObjectContext::internal()`, so a store can hold the context beyond a borrow | SDK `context/mod.rs` |
| Fix: futures awaited concurrently in one task lost wake-ups and hung | SDK `endpoint/futures/async_result_poll.rs` |
| Fix: set `TCP_NODELAY` on accepted connections | SDK `http_server.rs` |

The two fixes are bugs in the SDK as published, not specific to this
experiment.

**Concurrent awaits.** The input stream wakes only the last future to poll it. Once
another future had read the input that completed a handle, the future waiting
on that handle was never woken, and then went back to reading input rather
than checking for its result. Session I/O awaits several state reads at once
(prefetch and readahead), so invocations hung until Restate's
60-second inactivity timeout. Now every future waiting for input is woken
when input arrives, and a woken future checks for its result first.

**`TCP_NODELAY`.** `ctx.run` sends its command and then its result as two
small writes. With Nagle's algorithm the second waits for the server's delayed
acknowledgement: every `ctx.run` took about 42 ms longer. A cached transfer
took 62 ms with the barrier, and takes 15.7 ms with the fix.

## Validation

- **Unit tests** in the vendored shared-core: a retry reads current state with
  fresh completion ids, a first attempt is unchanged, and awakeables are
  refused (258 tests pass).
- **Lost writes and the page cache** (`tests/integration/ordered.rs` in the
  main crate): a store with FIFO, unconfirmed writes, where a request loses its
  tail while the cache lives on. Resetting the cache keeps every case
  consistent; without the reset, losing a checkpoint corrupts the database.
- **Crash campaign** ([`tests/crash.rs`](tests/crash.rs)) against a real
  `restate-server` 1.7.13 on one node. Two objects, six clients and 120
  transfers; one in three is sent twice, and every call is resent until
  answered. The ledger process aborts before or after random state requests
  and is restarted, and the server is killed and restarted three times; each
  kill loses the unstored writes of attempts in flight while the ledger and its
  page cache live on. The audit
  checks `PRAGMA integrity_check`, total money, exactly one recorded row per
  transfer id, and every balance against the recorded transfers.

  | Abort rate per state request | Runs | Ledger restarts | Result |
  |---|---|---|---|
  | 1.5% | 17 | 34 to 78 | All consistent |
  | 4% | 8 | 185 to 290 | All consistent |

  At 8% an invocation of about 100 requests almost never completes, so the
  campaign does not finish; that is a liveness bound of the test, not a
  correctness failure.
- **Resuming a paused invocation**: an invocation paused after 70 failed
  attempts (a journal of 1,121 entries) was resumed, skipped the journal and
  completed.

## Costs

200 sequential transfers on a 100-account ledger, release build, one local
server node, measured through the ingress:

| | Mean latency | Lazy reads per transfer | Writes per transfer |
|---|---|---|---|
| Page cache | 15.7 ms | 5.0, and one `ctx.run` | 2.0 sets, 1.9 clears |
| No cache | 15.5 ms | 11.9 | 1.8 sets, 1.0 clear |

A lazy read, and the `ctx.run`, are two journal entries each (the command,
and the notification carrying the value), so a cached transfer journals about
18 entries. On one local node the cache saves reads, not time; it pays off as
reads get more expensive and databases larger.

## Caveats

- **Every read is journaled with its value.** A state read appends the value
  to the invocation's journal, so reading pages costs log writes, and the
  page cache matters more than it does elsewhere. `service_options()` sets
  journal retention to zero so completed journals are not kept.
- **The journal grows across attempts.** Skipped entries stay; each attempt
  appends its own.
- **A suspension is a restart.** Resuming means running from the start. Long
  handlers that hit the inactivity timeout start over, and request/response
  deployments (such as Lambda), which suspend at every await, cannot make
  progress. Use HTTP/2 bidirectional streaming.
- **Outside the model.** The TLA+ specification and the protocol monitor
  assume independent, acknowledged puts. Writes that return before they are
  stored break their acknowledgement rules, and are safe only by the FIFO
  argument above, which the crash campaign tests but nothing proves.
- **Not supported:** shared handlers, awakeables, and journal replay for the
  same service.
- **Assumptions to confirm with the runtime:** an attempt's stored commands
  are a prefix of those it sent, in order (FIFO writes); an empty `ctx.run` is
  acknowledged only after the commands before it are stored (the barrier); an
  abandoned attempt's commands are never applied after a new attempt starts
  (fencing); state a lazy read returns is durable (`durable_when_visible`).
  The campaign kills processes, so it checks ordering and recovery, not
  storage durability across a machine crash.

## Run

```sh
npm install --prefix /tmp/restate @restatedev/restate-server@1.7.13   # or any restate-server binary
cd restate
RESTATE_SERVER=/tmp/restate/node_modules/@restatedev/restate-server-linux-x64/bin/restate-server \
    cargo test --test crash -- --nocapture    # ABORT_PER_MILLE=40 for more faults
```

Without `RESTATE_SERVER` the campaign is skipped. To run the ledger by hand,
start `restate-server`, run `target/release/ledger` (port 9080, or `PORT`), and
register `http://127.0.0.1:9080` with the admin API.
