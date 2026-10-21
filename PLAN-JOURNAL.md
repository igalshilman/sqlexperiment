# Plan: the journal, a transaction every participant joins

A plan for an agent working without this conversation. Read it whole before
starting. The rules of [CLAUDE.md](CLAUDE.md) apply: no backwards
compatibility, small commits, frequent PRs merged once green, decide yourself
and ask the owner only critical questions.

## 0. Progress (branch `claude/journal`)

- **Step 1, records: done.** `Session::record`, `Session::seal`; the runtime's
  participants keep their state in records. Agent bench: 2.6 MiB over 200
  steps, as before.
- **Step 2, files: done.** `Session::file`; pages keyed by a page id (the
  file's id in the high 32 bits, so the cache, hint and readahead are
  unchanged); keys `v15/page/<file>/<page>`.
- **Step 3, based files and the split: done.** `Journal::open` (and
  `open_reader`) opens the namespace: records, files, `scoped(prefix)`,
  `commit()`. `Sql::open(&journal)` runs the engine on the file `db`, and
  `Sql::open_with_options(store, options)` opens a journal of its own for a
  host whose state is all SQL (the Node.js addon and the Python extension).
  `Durability::Participant`: `Sql::flush` moves the SQL's commits into its
  file, and `Journal::commit` makes them durable with the rest; a commit with
  SQL not flushed is refused. With `Durability::Commit`, `flush` seals. The
  engine a session parks is taken by the next journal's open and handed to
  its `Sql`; a journal that opens no SQL parks it again. The records crash
  campaign runs in both modes.
- **Step 4, memory on the journal: done, before the split** (the split is a
  rename; step 4 is what the measurements are about). The memory file is
  `memory/<golden fingerprint>`, one per golden image, since a scope's next
  run may be another program; a new run writes holes for the previous run's
  pages (the old file opened without its base). A settled run writes holes
  for its pages rather than `set_len(0)`, which on a based file would write
  a hole for every page up to the memory's length. `codec.rs` and the chain's
  use in the runtime are gone; `vm::VERSION` is 13.
- **The chain is removed from sqlite-kv** (`ChainUpdate`, `Session::chain`,
  the record's chain fields; `Session::seal()` takes no argument). The crash
  campaign that covered it now covers records alone
  (`tests/integration/records.rs`).
- **Step 5, extensions on journal handles: done.** `Extension::attach(blobs,
  journal)` gets the journal scoped to `<name>/`; extension names may not
  hold `/` or `.`. `fs` keeps its manifest in the record `fs/manifest` and
  its running command in `fs/running`, so the manifest blob is gone.
- **Step 7, rename: done for the Rust crate.** `core/sqlite-kv` is
  `core/journal`, crate `journal`. Decided: the Node.js addon and the
  Python extension keep their names (`sqlite-kv`): to their users they are
  SQL over a key/value store, which the name says.
- **Measured after step 4** (in-memory store, medians):

  | | Before | After |
  |---|--:|--:|
  | Agent, stored over 200 steps | 2.6 MiB | 2.5 MiB |
  | Agent, restore | 1.5 ms | 2.5 ms |
  | Agent, model turn | 1.4 ms | 1.7 ms |
  | Checkpoints, heap-churn turn, restored | 7.7 ms | 12.7 ms |
  | Checkpoints, write-heavy turn, live | 6.2 ms | 3.4 ms |
  | Checkpoints, sparse turn, bytes written | ~1.0 KB | ~1.5 KB |

  Restore regressed by less than 2x: no extents (step 6). The sparse turns'
  extra bytes are the commit record's records (the memory's page list and
  the directory).

- **Against `main` (81f20a3), same machine, after step 4.** The storage
  benchmarks (amplification, small databases, log shipping) make the same
  gets, puts and round trips; only bytes grow: page keys by 9 bytes
  (`<file:08x>/`, +36% key bytes on reads), the commit record by the
  directory (+1.6% to +5% of its bytes); in all, +0.5% (log shipping) and
  +4.4% (small databases) bytes written. The runtime (this branch also
  carries ctx.llm and the participants, so not step 4 alone):

  | | `main` | Branch |
  |---|--:|--:|
  | Agent, stored over 200 steps | 3.2 MiB | 2.5 MiB |
  | Agent, largest step | 238 KiB | 47 KiB |
  | Agent, restore | 0.6 ms | 2.6 ms |
  | Agent, model turn / tool turn | 1.2 / 0.9 ms | 1.6 / 1.8 ms |
  | Checkpoints, heap-churn turn, live / restored | 6.0 / 7.5 ms | 9.3 / 11.3 ms |
  | Checkpoints, write-heavy turn, live / restored | 3.9 / 4.8 ms | 4.1 / 5.0 ms |
  | Checkpoints, sparse-256k turn, bytes written | 1.3 KB | 1.9 KB |

  On `main` the agent's conversation lives in the heap (174 live pages); on
  the branch it is in the scope's database, read for each model call
  (2.3 ms of the tool turn), and the heap is 66 pages.

## 1. Goal

Today the durable functions runtime (`rust/functions`) commits each turn
through `core/journal`, a crate that puts SQLite over a key/value store
(a Restate object's state). SQLite owns the commit record; the runtime bolts
two things onto it from the side: `effects` (the run's state, JSON) and the
`chain` (the program's memory, as page batches). Extensions (`fs`, `llm`)
get a third channel, a JSON record each (see `rust/functions/src/transaction.rs`
on the branch `claude/participants`).

The goal is to invert that. The **journal** is the base layer: one seal per
turn, written as one put, and everything a run keeps takes part in it the
same way, through two primitives, a **file** (4 KiB pages, random access,
written in place behind a log) and a **record** (a small whole value in the
seal). SQLite becomes one participant that opens the file `db`. The
program's memory becomes a file. The run's state becomes a record. An
extension opens whatever it needs. The journal hides how a seal reaches the
store: packing small changes into the commit record, chunking large ones
into stored values written first, delta compression, checkpoints, the
process-wide page cache, garbage.

None of sqlite-kv's machinery is rewritten. Its log, pages in place,
compression, cache, hint and garbage collection already *are* the journal,
for one file. The work is to give pages a file, give records a name, and
move the SQL engine from owner to client.

What it buys:

- One storage contract for the run's state, its memory, its tables and
  every extension, with random access for all of them (SQLite needs it;
  recovery is "open your files", not "replay your history").
- Rollback by construction: an abandoned seal drops the journal's buffered
  writes; every participant reopens its files at the last seal.
- Memory restores through the page cache: a parked run on the same process
  needs no store reads; the disk tier keeps it under memory pressure.
- `codec.rs`, the chain, `effects`, the `Sink`/`parts` JSON records and each
  extension's own garbage collection go away.

## 2. Starting point

Branches (local to the owner's session; ask for them to be pushed if they
are not on the remote):

- `main`: `fetch` merged (#178); the model gateway `ctx.llm` is PR #177
  (branch `claude/focused-cori-nw68l4`).
- `claude/extensions` (on #177): the extension contract (`Extension`,
  `Attached`, `Extensions`), `ctx.llm` on it, `ctx.fs` (a checkpointed
  filesystem over content-addressed blobs).
- `claude/participants` (on `claude/extensions`): `src/transaction.rs`
  (`Participant`, `Sink`, `Seal {seq, parts}`, `Sealing`, `Blobs`,
  `ScopeBlobs`, `DirBlobs`, `MemoryBlobs`); the run's state (`RunState`),
  memory (`Memory`) and tables (`Db`) are participants; `Run::turn` prepares
  all, seals, tells all (`committed`/`aborted`), collects blobs. Tests in
  `rust/functions/tests/transactions.rs`. **Start from this branch.**

How sqlite-kv works now (read `core/journal/docs/DESIGN.md` and
`FORMAT.md`; they are accurate):

| Key | Holds |
|---|---|
| `v14/commit` | The commit record: version, epoch, database size, next segment number, garbage, `effects` (opaque host bytes), the log since the last checkpoint (segments inline, or naming stored values), the host's `chain` (opaque values, appended or restarted) |
| `v14/log/<n>` | A stored value: log segments too large for the record, or the record's inline parts sealed once they outgrow `inline_log_bytes` (1 KiB in the runtime); immutable; written before the record that names it; deleted once no record names it |
| `v14/page/<n>` | A checkpointed page, overwritten in place; absent means zeros |
| `v14/hint` | Advisory: pages a cold open fetches first |

The read rule: a page is its latest image in the record's log, else the
cache, else `page/<n>`, else zeros (`Snapshot::block`). A save queues, in
order: garbage deletes, stored values, the record, a checkpoint if the log
is long (each changed page's latest image to `page/<n>`, the hint, then a
record with an empty log at the next epoch), then one sync
(`Snapshot::commit`, `Snapshot::checkpoint`). Segments are zstd frames whose
dictionary is the previous image of each page in the log (`log::Chain`).
The process-wide cache (`cache.rs`, foyer, memory + disk tiers) keeps
checkpointed pages keyed `(generation, page, version)` under a per-namespace
epoch, the last commit record this owner read or wrote, the stored values it
names, and an idle SQL engine. The simulation monitor
(`src/simulation/monitor.rs`) checks every write against six invariants;
`spec/Protocol.tla` states them for TLC.

Other users of the crate, whose API must keep working:
`typescript/packages/sql-node/src/lib.rs` and `python/sqlite-kv/src/lib.rs`
use `Session::{open_with_options, open_reader, save, commit_with, effects,
is_current}`, `Durability::Commit`, `PageCache`, `Value`.

In the runtime, `rust/functions/src/run.rs` holds the two built-in
participants: `Memory::prepare` encodes the turn's changed pages with
`codec::encode_pages` against the pages they replace (golden for a first
write), as `ChainUpdate::{Restart, Append, Clear}`; `restore` applies the
chain's batches over the program's golden image. `RunState::prepare` writes
JSON into the `Sink`. `Db::seal` calls `Session::commit_turn(effects, chain)`.

## 3. Target design

### 3.1 Primitives

```rust
// core crate (sqlite-kv, renamed `journal` in the last step)
pub struct Journal;                 // one namespace, this attempt's session
impl Journal {
    pub async fn open(store: Arc<dyn KvStore>, options: StorageOptions) -> Result<Self>;
    pub async fn open_reader(store, options) -> Result<Self>;   // beside the owner, read-only
    /// A paged file: an absent page reads as zeros.
    pub fn file(&self, name: &str) -> Result<File>;
    /// A paged file over `base`: an absent page reads as the base's, and a
    /// page's first image is compressed against it.
    pub fn file_with_base(&self, name: &str, base: Arc<dyn Base>) -> Result<File>;
    /// A whole value in the seal, replaced as a whole; empty if never set.
    pub fn record(&self, name: &str) -> Result<Record>;
    /// The seal: every file's and record's changes, one put (plus a
    /// checkpoint when the log is long), then one sync. Nothing changed:
    /// no write.
    pub async fn commit(&mut self) -> Result<SaveReport>;
    pub fn version(&self) -> u64;
    pub async fn is_current(&self) -> Result<bool>;
}

pub trait Base: Send + Sync { fn page(&self, index: u64) -> Option<&[u8]>; }

pub struct File(Arc<..>);          // handles are cheap to clone
impl File {
    pub async fn read(&self, page: u64) -> Result<Option<Page>>;   // None: a hole
    pub fn write(&self, page: u64, image: Option<Page>);           // buffered until the seal; None makes a hole
    pub fn len(&self) -> u64;
    pub fn set_len(&self, len: u64);                               // pages past it become holes
}

pub struct Record(Arc<..>);
impl Record {
    pub fn get(&self) -> Bytes;
    pub fn set(&self, bytes: Bytes);                               // buffered until the seal
}

pub struct Sql;                     // the SQL engine over a journal's file "db"
impl Sql {
    pub async fn open(journal: &Journal, options: SqlOptions) -> Result<Self>;
    // query, query_with, query_columns, execute, execute_with, transaction,
    // checkpoint, reused_engine, version: as Session today.
    /// Commit the open SQL transaction and flush its pages into the file.
    /// With Durability::Commit, also seal; with Durability::Participant, the
    /// host seals (Journal::commit).
    pub async fn flush(&mut self) -> Result<()>;
    pub async fn save(self) -> Result<SaveReport>;                 // flush, seal, park the engine
    // For sql-node and the Python extension: thin wrappers over the record
    // "effects" plus flush and seal.
    pub async fn commit_with(&mut self, effects: Bytes) -> Result<()>;
    pub fn effects(&self) -> Bytes;
}
```

`Durability` gains `Participant`: a SQL commit flushes into the file; only
`Journal::commit` makes anything durable. `Save` and `Commit` keep their
meaning for standalone callers.

### 3.2 The seal (format v15)

The commit record keeps its version, epoch, next segment number and garbage,
and gains a **directory**: each file's id (u32, allocated from `next_file`,
never reused), name, kind (paged or record) and, for a paged file, its size.
Files are never deleted; a paged file is emptied with `set_len(0)` (its
pages become holes, which a checkpoint deletes).

- The log: each transaction holds one entry per file it changed:
  `(file, size after, parts)`. Segment entries are `(file, page)`; a segment
  holds one file's pages. The compression dictionary (`Chain`) is keyed by
  `(file, page)`, and falls back to the file's base page, if the file has a
  base. A file's log is replayed only when the file is opened (lazily), so
  the base is at hand to decode.
- Records: a map `file id -> Part` (inline, or a stored value when larger
  than the inline budget), replaced whole when set. This is `effects` with a
  name, and `MAX_EFFECTS_BYTES` becomes `MAX_RECORD_BYTES`.
- The `chain` is removed.
- Keys: `v15/commit`, `v15/log/<n>`, `v15/page/<file:08x>/<page:016x>`,
  `v15/hint`. Magics `SC15`, `SL15`, `SH15`.
- Checkpointed pages of a file with a base are encoded against the base
  page (a new `page_codec` tag: zstd with the base page as dictionary); the
  reader has the base when it opens the file. Without this, memory's pages
  in place would cost a full page each, where today's first batch costs a
  delta against golden.

The write ordering rules of `DESIGN.md` §"The model" do not change: a page
is now identified by `(file, page)`, a record is a stored-or-inline value
like a chain value was. The monitor's six invariants hold as written, with
that identity.

### 3.3 Participants in the runtime

| Participant | Opens | Notes |
|---|---|---|
| `run` (`RunState`) | record `run` | JSON of `RunRecord`, as today's part "run" |
| `memory` (`Memory`) | file `memory` with base = the program's golden image; record `memory` | Writes the captured pages; `set_len(memory_len)`. The record keeps `VmMeta`, `memory_len`, and the list of present pages (the pages that differ from golden, run-length encoded), since the store cannot list keys. Restore: golden mapped copy-on-write (`cow.rs`) plus `file.read` of each present page. `Clear` becomes `set_len(0)`. |
| `tables` (`Db`) | `Sql` on file `db`, `Durability::Participant` | `prepare` = `flush`; the host seals |
| extension `<name>` | files and records under the prefix `<name>/` | `Attachments` hands each extension a `Journal` view that prefixes names, so extensions cannot collide with each other or the built-ins |
| `fs` | record `fs/manifest`, blobs as today | Contents stay content-addressed blobs (`Blobs`, `ScopeBlobs`): shared across checkpoints and read by `exec` outside the process |

`Participant` loses `prepare(sink)`'s sink: each participant holds its
handles and writes into them; `prepare(&mut self)` is the moment to do it.
`committed`/`aborted`/`holds` stay. `Seal`, `Sealing`, `Sink` and the
`parts` map are deleted; the journal's `version` is the seal's sequence
number.

`Run::turn` becomes: run the guest; `memory.captured(..)`; every
participant's `prepare`; `journal.commit()`; `committed` to all, or
`aborted` to all on an error (and the attempt ends, as today);
`blobs.collect(holds)`.

## 4. Decisions made (do not reopen)

1. **Two primitives, file and record.** A record is not a one-page file: it
   is read with the commit record at open (no page fetch), like `effects`.
2. **Files are never deleted**, only emptied. Ids are never reused.
3. **The present-page set of a sparse file is the participant's**, kept in
   its record. The journal keeps only each file's size.
4. **Lazy per-file replay.** `Journal::open` reads the commit record (and
   the hint); a file's log segments are decoded when the file is opened.
   `Sql::open` replays `db`; memory's log decodes at restore.
5. **Based files compress against the base**, in the log and in place.
6. **Blobs stay separate** from the journal, and so does their collection
   (`holds`). Unifying them with stored values is a later question.
7. **One seal per turn**, as today. Combining turns is out of scope.
8. **The crate keeps its name until the last step**, then becomes
   `core/journal`, crate `journal`.
9. **Format v15, runtime `vm::VERSION` 13.** No migration.

## 5. Steps

Each step is one PR that leaves `make test`, `make lint`, `make
test-functions` and `make lint-functions` green (see §7 for the commands and
the known flaky test). Record the measurements of §6 before step 1 and after
steps 4 and 6.

### Step 1: records (effects with a name)

Crate: replace `Commit.effects: Bytes` with the directory and `records:
BTreeMap<u32, Part>`; keep the chain for now. `Journal` does not exist yet:
add `Session::record(name) -> Record` and make `commit_with(effects)` /
`effects()` wrappers over the record `effects`. A record larger than
`inline_log_bytes` goes to a stored value (as a large chain value does);
garbage covers it. Bump the format to v15 now (magics, keys, `format.rs`,
`FORMAT.md`): the later steps change the same record.

Runtime: `RunState` and the extensions write records (`journal.record(name)`
per part) instead of the `parts` map; `Seal`/`Sealing`/`Sink` go. `Db::seal`
calls `commit_turn(chain)` with no effects. `tests/common/mod.rs`:
`Store::commit()` becomes `Store::record(name) -> Value` and `set_commit`
becomes `set_record(name, value)` (`tests/runs.rs:143`, `tests/files.rs:44`,
`tests/transactions.rs`).

Monitor: rule 2 compares records as it compared `effects` and the chain;
rule 5 covers stored records. `tests/integration/chain.rs` and
`durability.rs` reference `effects`: update. `tests/public_api.rs`: update.

sql-node and the Python extension: no change (the wrappers keep their API).

Done when every suite is green and `cargo bench --bench agent` stores
within 2% of the baseline.

### Step 2: files (pages with a file id)

Crate: the directory gains paged files; `Logged` becomes per file
`(file, size, parts)`, one or more per transaction; `Images` and `Chain` key
by `(file, page)`; `page_key(file, page)`; `PageKey` gains `file`;
`NamespaceCache::{get, token, insert_read, replace, remove, record_use}`
take `(file, page)`; `hint.rs` encodes `(file, page)` pairs (per file, a
list of page deltas); `Snapshot::block/fetch/open_with_options` take the
file; prefetch at open: a file no larger than `prefetch_pages` is fetched
whole, else the hint's pages. `Snapshot` holds one `Log` per file. Checkpoint
writes every file's changed pages; the policy (`checkpoint_after`,
`checkpoint_log_bytes`) sums over files.

Introduce `Journal` and `File` in `lib.rs` (the `Session` becomes `Sql` in
step 3; here `Session` opens the journal itself and uses file `db`):
`io.rs`'s `State`/`KvFile` address a `File` for `DB_PATH` instead of the
snapshot directly. Keep `WAL_PATH` private as it is.

Monitor: pages are `(file, page)`; rule 3 and 4 per file. Spec: check
`spec/Protocol.tla` for anything that distinguishes the chain or effects; a
page there is abstract, so a file-qualified page needs no change; say so in
`spec/README.md`. Run `spec/check.sh` if TLC is installed; otherwise note
that it was not run.

Tests: `tests/integration/log.rs`, `block_io.rs`, `cache.rs`,
`failpoints.rs`, `races.rs`, `publication.rs`, and the unit tests under
`tests/unit/` (`snapshot.rs`, `log.rs`, `cache.rs`, `hint.rs`,
`io.rs`): mostly mechanical. Add `tests/integration/files.rs`: two files
written in one seal are both there after a crash at any write (the
`failpoints` campaign pattern); a file emptied with `set_len(0)` has its
page keys deleted by the next checkpoint; a second file's pages are served
from the cache after a checkpoint, and dropped at another epoch.

Runtime: no change beyond compiling.

### Step 3: `Sql` as a participant, based files

Crate: split `Session` into `Journal` (open, open_reader, file,
file_with_base, record, commit, version, is_current, the cache's
record/epoch/values knowledge) and `Sql` (the engine, `flush`, `save`,
`transaction`, the warm-engine reuse via `NamespaceCache::{park,
take_engine}`, the wrappers `commit_with`/`effects`). Add
`Durability::Participant`. `publish()` becomes `flush()` (PRAGMA
wal_checkpoint, move `dirty` into the file) followed, under `Commit`, by
`journal.commit()`.

Based files: `Base`, `file_with_base`, the dictionary fallback in
`log::Chain::dictionary` and `decode_segment`, lazy per-file replay
(`log::replay` per file, on `file()`), and the `page_codec` tag for
in-place pages compressed against the base (`Checkpoint::new` and
`Snapshot::block` take the base from the file).

sql-node and the Python extension: `Journal::open` + `Sql::open`; `reader`
sessions become `Journal::open_reader` + `Sql::open`. Run `make test-addon
test-ts test-py`.

Tests: `tests/integration/sql.rs` and `durability.rs` on the new API;
`files.rs` gains: a based file's absent page reads as the base's; a first
image is smaller than the page (compressed against the base); after a
checkpoint it still reads back and is still small (`MemoryKv::stats`).

### Step 4: memory and the run's state on the journal

Runtime: `Memory` opens `memory` with base = the golden image (`Image`'s
`Golden` implements `Base`; see `model.rs` and `cow.rs`) and record
`memory` (`VmMeta`, `memory_len`, present pages run-length encoded).
`captured` writes the changed pages and updates the present set;
`prepare` sets the record; a settled run or a trap calls `set_len(0)` and
clears the set. `restore` reads each present page. Delete `codec.rs`
(`encode_pages`, `page_indexes`, `decode_pages` and `codec_tests`),
`ChainUpdate`, `MAX_CHAIN`, `Changed.old`, `base_bytes`/`delta_bytes`/
`batches`/`live_pages` bookkeeping (keep `live_pages` as the present set's
length for `View` and `footprint`). `RunState` keeps its record. `Db` opens
`Sql` with `Durability::Participant`; `Db::seal` goes; `Run::turn` calls
`journal.commit()`. `Scope::open` returns a `Journal`.

Crate: remove the chain (`ChainUpdate`, `Commit.chain`, `Sealed.chain`,
`Session::chain`, `MAX_CHAIN_VALUE_BYTES`, `tests/integration/chain.rs`,
the monitor's chain checks).

Tests: `tests/common/mod.rs` loses `batches()`; `tests/runs.rs`
`a_run_keeps_a_bounded_number_of_batches` becomes "a run's memory is
checkpointed and a restore reads pages in place" (assert the log is empty
after a checkpoint and the restore's reads through `MemoryKv::stats`, and
that the value is right); `small_turns_do_not_rewrite_a_big_image` asserts
bytes written per turn stay near the changed pages' size, not the image's
(`Store::written()`); `crashes.rs:165,173` and `runs.rs:284,413,541,560`
replace `batches()` with the memory file's state (its size is 0 and its
page keys are gone after a settled run: count keys under
`v15/page/<memory id>/` in `MemoryKv::keys()`). Bump `vm::VERSION` to 13.

Docs: `rust/functions/docs/DESIGN.md` §5 (persistent format, §5.2 the
binary page batch is gone) and §7; `HOST.md` §4; `README.md` "Where things
are"; `core/journal/docs/DESIGN.md` and `FORMAT.md`.

Done when every suite is green and §6's numbers are recorded.

### Step 5: extensions on journal handles

Runtime: `Extension::attach(journal: Journal (prefixed view), blobs, ..)`;
`Attachments` builds the view `<name>/`; `Attached` no longer returns a
record through a sink. `fs` keeps its manifest in record `fs/manifest`
(not a file: it is read whole at attach). `llm` keeps its conversations in
the tables. `Binding` unchanged. Update `tests/transactions.rs`'s `Counter`
and `docs/LLM.md`, `docs/FETCH.md` where they describe the contract.

### Step 6: measure, then extents if needed

Compare §6's numbers with the baseline. A restore now reads one key per
present page where it read a few chain values. If `restore_ms` in
`benches/checkpoints.rs` or `benches/agent.rs` regressed beyond 2×, add
extents: a paged file option `extent_pages` (1 for `db`, 16 for `memory`);
page keys become `v15/page/<file>/<extent>`; `block` fetches and the cache
stores an extent; a checkpoint writes whole extents (reading the pages of an
extent it does not fully hold). Measure again.

### Step 7: rename

`core/journal` becomes `core/journal`, crate `journal`
(`Makefile` `CORE`, `RUST_CRATES`; the path dependencies in
`rust/functions/Cargo.toml`, `typescript/packages/sql-node/Cargo.toml`,
`python/sqlite-kv/Cargo.toml` (rename that directory too, and the Python
package if its name is `sqlite_kv`); `flake.nix` if it lists the crate;
docs and READMEs). One commit, no code change.

### Later, not in this plan

- Park a VM's image into the cache's memory tier on `Run::park` (one
  budget with `budget.rs`).
- Blobs as journal stored values.
- `aborted` as a retry in process (drop buffers, reopen files) rather than
  the end of the attempt.
- Combining turns into one seal.

## 6. Measurements

Record before step 1, after step 4, after step 6, in the PR descriptions:

- `cd rust/functions && cargo bench --bench agent` (`bench-agent.json`):
  `stored_bytes` summed (baseline 2.6 MiB over 200 steps), `restore_ms`,
  `read_ms` of the request (3.6 ms at 636 KiB).
- `cargo bench --bench checkpoints`: live and restored turn times.
- `core/journal/benches/README.md` lists the crate's benchmarks and how to
  run them; `amplification` (reads, writes, recovery traffic) and
  `small_databases` are the ones that must not regress.
- `MemoryKv::stats()` in the tests above: bytes written and gets per
  restore.

## 7. Environment and commands

- Build with `CARGO_TARGET_DIR=/home/user/sqlexperiment/target` set (the
  session's disk is small; `rust/functions/target` once filled it).
- The Rust SDK checkout: `make rust/sdk/.build/sdk-rust/.patched`; if it
  says the build has edits the patches do not, `rm -rf rust/sdk/.build`
  and run it again.
- Core: `make test-core lint-core` (runs the simulation campaigns too).
- Runtime: `make test-functions` (three capture modes, then the programs
  against a Restate server) and `make lint-functions`. The Restate test
  `a_wait_survives_a_crash_and_one_no_one_answers_is_forgotten` is flaky on
  `main` (an attach that times out after 30 s); a failure of it alone is
  not yours. So is `a_run_waits_for_a_signal_from_outside` (2 runs in 10 on
  `claude/participants`): each `ctx.signal` is its own `wait` call, which
  sends `listen` to the signals object, so "the oldest wait" is the order
  the listens arrive in, not the order the program issued them.
- Other users of the crate: `make test-addon test-ts test-py`.
- Clippy runs with `-D warnings`; `cargo fmt` before each commit.

## 8. Risks

- **Restore round trips** (step 6): the one number that can get worse.
  Measure before deciding on extents.
- **Lazy replay and the hint.** A file's hint pages are prefetched at open
  before its log is decoded; keep the rule that a page in the log overrides
  a prefetched value (`Snapshot::with_fetched`).
- **Readers.** `open_reader` sessions (sql-node, Python) read beside the
  owner without a cache and check `is_current` after reading; a per-file
  log changes nothing there, but keep the test in `races.rs`.
- **The monitor.** Change it with the code, never after: it is what makes
  the fault campaigns mean something. If a rule needs restating for files
  or records, restate it in `DESIGN.md` in the same commit.
