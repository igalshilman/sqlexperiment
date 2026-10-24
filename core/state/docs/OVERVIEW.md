# The state, and a durable functions extension on it

A state is one Restate object's transactional state: paged files and small
records. It is read; it changes only through a mutation, an owned value that
collects records set and pages written, in memory. A flush consumes the mutation,
writes the chunks of pages it changed as new blobs, and puts the root that
names them, the commit point; a mutation dropped instead discards them. SQL runs on
the state, and the durable functions
runtime flushes one mutation per turn, with each extension keeping its own state
there.

It holds an object's working state (a database, a VM's changed memory, an
extension's counters), small enough to load and flush quickly. Bulk bytes go
to the scope's blob store, outside Restate, and a record names each blob by its
hash.

## What Restate stores

A state is one Restate object's state, in two kinds of key. Restate's own
invocation journal is not used: these handlers run with `journalReplay: false`,
and the root plays that role.

| Key            | What it holds                                                                                                                                             |
| -------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `v16/root`     | The root: the directory of files, the small records, and the top of each file's chunk map. The only key whose write commits.                              |
| `v16/blob/<n>` | One zstd frame, written once and never changed: a chunk of 4 pages (a base, or a delta against it), a chunk map's node, or a large record, or a log entry |

Transactions change two kinds of state:

- **Files** are page-addressed: the engine above reads and writes 4 KiB pages at
  random, and a flush writes the chunks that changed, as new blobs. A
  copy-on-write radix tree, the file's chunk map, names them: up to 1 MiB it
  fits in the root, and a file holds at most 10 GiB. SQL keeps its database in
  the file `db`. The functions runtime keeps a Wasm VM's memory in a file opened
  over the program's golden image, so only pages that differ from it are
  stored.
- **Records** are named values, such as `run`, `memory` or an extension's
  `notary/count`: inside the root up to 1 KiB, a blob of their own past it.
- **Logs** are named sequences of entries, each a blob of its own:
  a mutation appends entries and truncates the log from its start, so a
  flush writes what it appends, not what came before.

The contract it needs from Restate is small ([`kv.rs`](../src/kv.rs)): writes
return at once and are kept in issue order, `sync()` waits until they are
stored, and a crash keeps a prefix of the unsynced writes. There are no
multi-key transactions, locks or compare-and-swap. One owner reads and writes a
state at a time, which a Restate virtual object guarantees.

## How a flush works

Nothing is durable until `mutation.flush()`: every record set and page written in
the mutation (and every entry appended) becomes durable at once, and dropping the mutation discards them. The state
itself shows only what was flushed. A flush queues its writes in this order,
then syncs once:

1. **Garbage:** deletes of blobs that no root names for the last 16 flushes.
2. **Blobs:** each changed chunk's pages, compressed against the chunk's base
   (a delta), or, once the delta grows, a new base; a new node on each path
   of a chunk map to a changed chunk, below its top; and large records and
   log entries.
3. **The root:** naming them, with the small records. If this put is stored,
   the transaction happened.

The order makes a crash at any point safe. Restate keeps a prefix of the
queued writes, and a blob is put before any root names it and never
overwritten, so whatever survives, open finds a root that names only blobs
that are there. There is nothing to replay, checkpoint or repair.

- **Cost:** one sync, and a few puts: the root, a blob per changed chunk,
  and, in a file past 1 MiB, a map node per level on each changed path.
  A delta is compressed against its chunk's base, so it costs about what
  changed. A mutation that changes nothing writes nothing.
- **The cache:** an optional process-wide `PageCache` keeps blobs in memory and
  on local disk, by number: a blob never changes, so nothing is invalidated.
  With it, open and flush in steady state are one round trip each. An owner
  without a cache, or whose last flush's outcome is unknown, syncs at open, so
  that everything it reads is durable.
- **Readers:** a shared handler reads the root it opened, a consistent
  snapshot, beside the owner's flushes.
- **Errors:** a failed or cancelled flush has an unknown outcome, and ends the
  state. The owner settles outstanding requests and reopens, then looks at
  what landed before retrying.

## Who writes the state

The state never flushes on its own, nor knows what writes it. A host opens it,
makes a mutation, has each writer move its pending changes into it, as
records, pages and log entries, and decides when to flush.

| Host                                                                                           | Writers                                                      | When it flushes                             |
| ---------------------------------------------------------------------------------------------- | ------------------------------------------------------------ | ------------------------------------------- |
| The Node.js addon `restate-sql` and Python extension `restate_sql` (`core/sql/src/binding.rs`) | `Sql`, on the file `db`                                      | After every SQL transaction, as SQLite does |
| The durable functions runtime (`rust/functions`)                                               | Run state, VM memory, the scope's tables, and each extension | Once per turn: the seal                     |

**SQL.** `Sql` runs Turso's SQLite-compatible engine, unmodified, in three
layers:

1. Statements change a private view: dirty pages and the engine's own WAL
   file, in memory.
2. `Sql::flush(&mut mutation)` copies what SQL committed into the mutation, and keeps it
   in the private view until the mutation's receipt says the state flushed it; a
   mutation dropped unflushed leaves it for the next flush.
3. `mutation.flush()` makes it durable with the mutation's other files and records.
   A mutation flushed without `Sql::flush` leaves SQL's changes private; the
   host pairs the two.

An SQL error (syntax, a constraint) undoes only that statement. A storage
error, a cancelled call or a failed flush ends the session, which the owner
then reopens.

**Durable functions.** A Wasm program runs turn by turn, and each turn is one
transaction, the seal: one mutation that every participant prepares into, then
flushed.

| Participant       | Where it lives in the state                                                                                  |
| ----------------- | ------------------------------------------------------------------------------------------------------------ |
| Run state         | Record `run`                                                                                                 |
| VM memory         | Record `memory`, a file over the golden image, and a log of the turns since it was last folded into the file |
| Tables (`ctx.db`) | `Sql` on the file `db`                                                                                       |
| Each extension    | Its own records and files, named under `<name>/`                                                             |
| Large bytes       | The scope's blob store, outside Restate; a record names each blob                                            |

```
turn n                      seal n                  after the seal
───────────────────────     ───────────────────     ───────────────────────────
program runs                prepare(&mut mutation) on    operations issued in turn n
extensions: call(),         each participant,       are made (outbox); their
issued(), delivered()       then mutation.flush()        outcomes arrive in turn n+1
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
                Mutation, Participant, Record, ScopeBlobs, State, Tables};

/// The extension, as a deployment offers it.
#[derive(Clone)]
struct Notary;

/// Its part of one attempt at a run.
struct Notarized {
    record: Record, // stored as "notary/count" in the root
    count: u64,     // as this turn sees it
    sealed: u64,    // as the last seal holds it
}

#[async_trait]
impl Extension for Notary {
    /// Every attempt, on any machine, starts here: from the last seal.
    async fn attach(&self, _blobs: ScopeBlobs, state: State) -> Result<Box<dyn Attached>> {
        let record = state.record("count");
        let count = std::str::from_utf8(&record.get())?.parse().unwrap_or(0);
        Ok(Box::new(Notarized { record, count, sealed: count }))
    }
}

#[async_trait]
impl Participant for Notarized {
    /// Before the seal: set what changed in the mutation. Its flush makes it
    /// durable with the run's memory, its SQL and every other participant,
    /// in one root.
    async fn prepare(&mut self, mutation: &mut Mutation) -> Result<()> {
        if self.count != self.sealed {
            mutation.set(&self.record, self.count.to_string().into())?;
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
  scope's SQL inside the turn's transaction, sealed in the same root. `llm` keeps
  its conversations this way.
- **Large state:** an extension writes big bytes to the scope's blobs before the
  seal and names them in a record, as `fs` does with its manifest.
