# The volume, and a durable functions extension on it

A volume is transactional state with a write-ahead log, kept in one Restate
object. The state is paged files and small records. The WAL lives in the commit
record, and a commit appends one transaction to it in a single put; every few
commits a checkpoint applies the WAL to the pages. SQL runs on a volume, and the
durable functions runtime commits one transaction per turn, with each extension
keeping its own state there.

It holds an object's working state (a database, a VM's changed memory, an
extension's counters), small enough to load and commit quickly. Bulk bytes go
to the scope's blob store, outside Restate, and a record names each blob by its
hash. Against a Restate server, a warm call costs the same on 1 MB as on 100 MB,
and text stores at about half its size
([measurements](../../sql/benches/RESTATE_RESULTS.md)).

## What Restate stores

A volume is one Restate object's state, in four kinds of key. Restate's own
invocation journal is not used: these handlers run with `journalReplay: false`,
and the commit record plays that role.

| Key                   | What it holds                                                                                                                     |
| --------------------- | --------------------------------------------------------------------------------------------------------------------------------- |
| `v15/commit`          | The commit record: the directory of files, every record, and the WAL since the last checkpoint. The only key whose write commits. |
| `v15/log/<n>`         | WAL segments too large to keep inline in the record                                                                               |
| `v15/page/<file>/<n>` | 4 KiB pages of any file, overwritten in place by checkpoints                                                                      |
| `v15/hint`            | An advisory list of pages to prefetch on a cold open                                                                              |

Transactions change two kinds of state:

- **Files** are page-addressed: the engine above reads and writes 4 KiB pages at
  random, and a commit logs the pages that changed. SQL keeps its database in
  the file `db`. The functions runtime keeps a Wasm VM's memory in a file opened
  over the program's golden image, so only pages that differ from it are
  stored.
- **Records** are small named values inside the commit record, such as `run`,
  `memory` or an extension's `notary/count`.

The contract it needs from Restate is small ([`kv.rs`](../src/kv.rs)): writes
return at once and are kept in issue order, `sync()` waits until they are
stored, and a crash keeps a prefix of the unsynced writes. There are no
multi-key transactions, locks or compare-and-swap. One owner reads and writes a
volume at a time, which a Restate virtual object guarantees.

## How a commit works

Nothing is durable until `volume.commit()`: every record set and page written
since the last commit becomes durable at once, and dropping the volume discards
them. A commit queues its writes in this order, then syncs once:

1. **Garbage:** deletes of stored values the durable record no longer names.
2. **Stored values:** WAL segments too large for the record's inline budget.
3. **The commit record:** the WAL so far plus this transaction, and the
   records. If this put is stored, the transaction happened.
4. **Checkpoint (every few commits):** the latest image of each logged page to
   `page/<file>/<n>`, then a record with an empty WAL.

The order makes a crash at any point safe. Restate keeps a prefix of the queued
writes, and pages are overwritten only after a record whose WAL covers them, so
whatever survives, open finds a record whose WAL replays correctly over the
pages that are there. It is SQLite's WAL design on KV keys: no page map, no
allocation, no orphans to collect.

- **Cost:** usually one put and one sync. A logged page is compressed against
  its previous image, so it costs about what changed in it. An unchanged commit
  writes nothing.
- **The cache:** an optional process-wide `PageCache` keeps pages in memory and
  on local disk, and remembers which record this owner made durable. With it,
  open and commit in steady state are one round trip each. An owner without a
  cache, or whose last commit's outcome is unknown, syncs at open, so that
  everything it reads is durable.
- **Errors:** a failed or cancelled commit has an unknown outcome. The owner
  settles outstanding requests and reopens, then looks at what landed before
  retrying.

## Who uses a volume

The volume never commits on its own. A host opens it, lets participants change
it, and decides when to commit. A participant implements `volume::Participant`:
it moves its pending changes into the volume's files and records before the
commit.

| Host                                                                                           | Participants                                                 | When it commits                             |
| ---------------------------------------------------------------------------------------------- | ------------------------------------------------------------ | ------------------------------------------- |
| The Node.js addon `restate-sql` and Python extension `restate_sql` (`core/sql/src/binding.rs`) | `Sql`, on the file `db`                                      | After every SQL transaction, as SQLite does |
| The durable functions runtime (`rust/functions`)                                               | Run state, VM memory, the scope's tables, and each extension | Once per turn: the seal                     |

**SQL.** `Sql` runs Turso's SQLite-compatible engine, unmodified, in three
layers:

1. Statements change a private view: dirty pages and the engine's own WAL, in
   memory.
2. `Sql::flush` moves what SQL committed into the file `db`, still private to
   the volume.
3. `volume.commit()` makes it durable with the volume's other files and
   records. The volume refuses to commit while SQL holds changes it has not
   flushed.

An SQL error (syntax, a constraint) undoes only that statement. A storage
error, a cancelled call or a failed commit ends the session, which the owner
then reopens.

**Durable functions.** A Wasm program runs turn by turn, and each turn is one
transaction, the seal:

| Participant       | Where it lives in the volume                                            |
| ----------------- | ----------------------------------------------------------------------- |
| Run state         | Record `run`                                                            |
| VM memory         | Record `memory`, plus the changed pages of a file over the golden image |
| Tables (`ctx.db`) | `Sql` on the file `db`                                                  |
| Each extension    | Its own records and files, named under `<name>/`                        |
| Large bytes       | The scope's blob store, outside Restate; a record names each blob       |

```
turn n                      seal n                  after the seal
───────────────────────     ───────────────────     ───────────────────────────
program runs                prepare() each          operations issued in turn n
extensions: call(),         participant, then       are made (outbox); their
issued(), delivered()       volume.commit()         outcomes arrive in turn n+1
```

A crash before the seal loses the whole turn, which runs again from the last
seal. Whether a turn happened is whether its seal was stored: `committed()` and
`aborted()` are only notices a crash may skip, and every attempt recovers each
participant from the last seal, in `attach()`.

## A toy extension: notary

`notary` notarizes a document through an outside service and counts receipts in
its own record. It uses two of the three things an extension can offer
([`extension.rs`](../../../rust/functions/src/extension.rs)): an operation made
after the seal, and state kept in the seal. `rust/functions/tests/transactions.rs`
has a similar `Counter` toy you can run.

```rust
use functions::{Attached, Extension, GuestError, HostCall, Making, Operation, Outcome,
                Participant, Record, ScopeBlobs, Tables, Volume};

/// The extension, as a deployment offers it.
#[derive(Clone)]
struct Notary;

/// Its part of one attempt at a run.
struct Notarized {
    record: Record, // stored as "notary/count" in the commit record
    count: u64,     // as this turn sees it
    sealed: u64,    // as the last seal holds it
}

#[async_trait]
impl Extension for Notary {
    /// Every attempt, on any machine, starts here: from the last seal.
    async fn attach(&self, _blobs: ScopeBlobs, volume: Volume) -> Result<Box<dyn Attached>> {
        let record = volume.record("count");
        let count = std::str::from_utf8(&record.get())?.parse().unwrap_or(0);
        Ok(Box::new(Notarized { record, count, sealed: count }))
    }
}

#[async_trait]
impl Participant for Notarized {
    /// Before the seal: put what changed in the record. The volume commits it
    /// with the run's memory, its SQL and every other participant, in one put.
    async fn prepare(&mut self) -> Result<()> {
        if self.count != self.sealed {
            self.record.set(self.count.to_string().into());
        }
        Ok(())
    }
    fn committed(&mut self) { self.sealed = self.count; }
    fn aborted(&mut self) { self.count = self.sealed; } // back to the seal
}

#[async_trait]
impl Attached for Notarized {
    /// Made again under the same key, the service answers as the first time.
    fn deduplicates(&self) -> bool { true }

    /// After the seal that issued it; again after a crash, under the same key.
    async fn operation(&mut self, call: HostCall, _tables: &mut Tables<'_>)
        -> Result<Making, GuestError> {
        Ok(Box::pin(async move {
            let receipt = notarize(&call.idempotency_key, &call.input).await?;
            Ok(serde_json::json!({ "receipt": receipt }))
        }))
    }

    /// In the turn the outcome arrives, before the program sees it.
    async fn delivered(&mut self, _tables: &mut Tables<'_>, _op: &Operation,
                       _outcome: &mut Outcome) -> Result<()> {
        self.count += 1; // sealed by this turn's prepare
        Ok(())
    }
}
```

Offer it beside the built-in extensions (`rust/functions/src/restate.rs`):

```rust
Extensions::new().with("fetch", fetch).with("notary", Notary)
```

A program calls it through the generic operation entry point:

```js
export async function main(input, ctx) {
  const { receipt } = await ctx.call('notary', { doc: input.doc });
  return receipt;
}
```

What each turn seals:

| Turn                       | What runs                                                                    | What the seal holds                                                    |
| -------------------------- | ---------------------------------------------------------------------------- | ---------------------------------------------------------------------- |
| 1                          | `ctx.call('notary', …)` issues the operation; the program waits              | Memory, run state with the pending operation; `notary/count` unchanged |
| Between turns              | The host makes `operation()`; a crash here repeats it under the same key     | Nothing new                                                            |
| 2                          | `delivered()` increments the count; the program gets the receipt and returns | New memory pages, run state, `notary/count = 1`                        |
| Crash before turn 2's seal | A new attempt calls `attach()`, reads `count = 0`, and runs turn 2 again     | Unchanged                                                              |

The count can never be off by one: the increment and the program seeing the
receipt are sealed together in turn 2.

Not covered by the toy:

- **Calls inside the turn** (`async fn call`, such as `ctx.notary.status()`)
  need a wrapper in the guest code (`rust/functions/assets/guest.js`). Only the
  built-ins have one today (`ctx.fs`, `ctx.llm`); a third-party extension is
  reachable only through `ctx.call`, as an operation.
- **SQL in an extension:** `Tables` lets an extension read and write the
  scope's SQL inside the turn's transaction, sealed in the same put. `llm` keeps
  its conversations this way.
- **Large state:** an extension writes big bytes to the scope's blobs before the
  seal and names them in a record, as `fs` does with its manifest.
