# Core

What every SDK here builds on: the storage of a Restate object that runs
without Restate's journal (`journalReplay: false`). (The Restate SDKs' shared
core, which runs such a handler since 8.1.0, is used as released.)

| Path                        | What                                                                                                                                                                                                           |
| --------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| [`state/`](state/README.md) | **The state.** Records, paged files and logs over an asynchronous key/value store, changed through a mutation that a flush makes durable atomically, in one round trip, for one owner at a time: a Rust crate. |
| [`sql/`](sql/README.md)     | **SQL on the state**: SQLite-compatible SQL in the state's file `db`, flushed with it. The [Node.js addon](../typescript/packages/sql/README.md) and the [Python extension](../python/sql/README.md) wrap it.  |

## Layers

Four layers, each knowing only the one below it. Every change climbs down the
same way: a layer keeps it private until the one above hands it on, and only
the state's flush makes it durable.

```
host     restate-fns (Node.js, Python)          durable functions runtime
         flush after each SQL transaction       flush once per turn: the seal
             │                                      │ run, memory, extensions
             ▼                                      ▼
SQL      Sql: statements change a private view; Sql::flush(&mut mutation)
         moves what SQL committed into the mutation, as pages of the file `db`
             │
             ▼
state    records · files · logs, read as last flushed; a Mutation collects
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
| **Host**  | The SDK API: `ctx.db` in an object handler, `ctx.db.sql` in a durable function                                                                  | When to flush, and what goes in the same mutation                                           | How the state lays out what it stores, or SQL's pages |

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

**The hosts.** Both pair `Sql::flush` with the mutation's flush; they differ
in what else rides along, and how often.

| Host                                                                                | Flushes                                             | With SQL, in the same mutation                                                                                  |
| ----------------------------------------------------------------------------------- | --------------------------------------------------- | --------------------------------------------------------------------------------------------------------------- |
| `binding::Session`: the Node.js addon and the Python extension, under `restate-fns` | After each call that leaves no SQL transaction open | The record `effects`: the sends committed with the rows, an outbox, opaque to the storage                       |
| The [durable functions runtime](../rust/functions/README.md)                        | Once per turn: the seal                             | The run (record `run`), the VM's memory (record `memory`, a file and a log), each extension's records and files |

Deeper: the state's [README](state/README.md), [overview](state/docs/OVERVIEW.md),
[design](state/docs/DESIGN.md) and [format](state/docs/FORMAT.md); SQL's
[README](sql/README.md).

## Build and test

From the repository's root (see [its README](../README.md#build-and-test)):

```sh
make core        # the crates
make test-core   # their tests, with the simulation campaigns
make lint-core   # rustfmt and clippy, for the crates, the addon and the Python extension
```
