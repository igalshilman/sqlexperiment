# Copy-on-write blobs replace the log

Merged in [PR #246](https://github.com/igalshilman/sqlexperiment/pull/246),
as an experiment first. The state crate's log, checkpoints, page keys and
working-set hint are gone: a flush writes immutable zstd blobs and one root
that names them ([DESIGN.md](DESIGN.md), [FORMAT.md](FORMAT.md)). This report
asked four questions: does it simplify the code, does it make correctness and
caching easier to reason about, do shared handlers work, and what does it
cost.

## The design in one paragraph

A paged file is a row of chunks of 4 pages, at most 10 GiB. A chunk is a
**base** blob (its pages, one zstd frame) and maybe a **delta** blob: its
pages now, compressed with the base as zstd's reference prefix, so it costs
about what changed since the base. A flush writes a new delta for each
changed chunk, or a new base once the delta passes a quarter of the base;
then a new node on each changed path of the file's chunk map (a radix tree
of 64-entry nodes, its top in the root); then the root, which holds the
maps' tops and the small records; then syncs once. **One rule:** a blob is put before any root names
it, and deleted only `garbage_after` (16) flushes after the last root that
named it. A file over a base (a VM's memory over its golden image) compresses
its chunks' bases against the golden pages.

## Stronger use of zstd

The first copy-on-write try (before this branch's rewrite) packed each
flush's changed pages into one frame: 4.8 KB a flush to update one row of a
10k-row table, against the log's 7.9 KB, and 2.7× the log's bytes for small
changes to the same pages, because it rewrote whole pages. What changed:

- **Deltas against a base, with the base as the reference prefix**
  ("patch-from"). An update of a few bytes writes about those bytes and the
  frame's overhead, wherever the page is: 499 B for that update.
- **Pages compressed with their neighbours.** A chunk is one frame of 4
  pages, so a base compresses B-tree pages against each other: stored size
  is 12% below the log's per-page frames.
- **The golden image as the reference** for a VM's memory: a chunk's base is
  compressed against the golden pages it replaces.
- **The root is one zstd frame**, records included.

## Measurements

`core/sql/benches/flush_costs.rs`, one flush per session, 100–300 flushes,
against the log (main at `4cba925`, the same bench). Bytes are keys and
values put per flush.

| Workload                        | Log B/flush |      CoW B/flush | Puts/flush log / CoW | Deletes/flush log / CoW | Stored KiB log / CoW | Keys log / CoW |
| ------------------------------- | ----------: | ---------------: | -------------------: | ----------------------: | -------------------: | -------------: |
| Chat row + effects (300)        |       1,870 | **1,579** (−16%) |          1.58 / 2.11 |             0.32 / 1.00 |              62 / 65 |        39 / 34 |
| 100 rows, update 1              |         908 |    **871** (−4%) |          1.25 / 2.45 |             0.12 / 1.37 |              11 / 18 |         9 / 28 |
| 10k rows, update 1              |       7,853 |    **512** (15×) |          4.06 / 2.92 |             0.97 / 0.91 |        1,307 / 1,150 |      710 / 377 |
| 10k rows, update 10 (scattered) |      63,826 |  **2,732** (23×) |        20.85 / 19.68 |            1.00 / 16.30 |        1,338 / 1,184 |      704 / 650 |
| 10k rows, insert 1              |       1,676 |  **1,660** (−1%) |          1.61 / 3.22 |             0.33 / 1.95 |        1,308 / 1,168 |      717 / 231 |
| 100k rows, update 1 (100)       |       7,265 |    **426** (17×) |          3.80 / 2.91 |             1.15 / 0.00 |      12,729 / 11,142 |  7,022 / 1,944 |

| Workload            | Round trips/flush, cached log / CoW | Uncached log / CoW | Cold read gets log / CoW | Cold read KiB log / CoW |
| ------------------- | ----------------------------------: | -----------------: | -----------------------: | ----------------------: |
| Chat row + effects  |                         2.00 / 2.00 |        2.99 / 3.58 |                    8 / 3 |                  10 / 4 |
| 100 rows, update 1  |                         2.00 / 2.00 |        3.00 / 3.00 |                    6 / 5 |                   7 / 7 |
| 10k rows, update 1  |                         2.00 / 2.00 |        5.75 / 5.89 |                   18 / 7 |                 48 / 22 |
| 10k rows, update 10 |                         2.00 / 2.00 |      17.66 / 22.60 |                   28 / 7 |                 48 / 22 |
| 10k rows, insert 1  |                         2.00 / 2.00 |        3.42 / 6.03 |                   11 / 6 |                 21 / 22 |
| 100k rows, update 1 |                         2.00 / 2.00 |        7.05 / 6.89 |                   26 / 5 |                 78 / 24 |

(Round trips count the open and the flush. Cold reads are a fresh session's
first indexed lookup, without a cache.)

- **Updates in a larger database: 15–23× fewer bytes.** The log rewrote its
  inline tail with every flush and wrote each page twice (logged, then
  checkpointed); a delta is written once and costs what changed.
- **Small databases and appends: about even.** The log's inline record was
  already cheap there; copy-on-write puts at least two keys (a blob and the
  root) where the log often put one.
- **Storage is 11–12% smaller in the larger databases**, with fewer keys (a
  key per blob, not per page). Small databases store more, 18 KiB against
  11: a base and a delta per chunk, plus 16 flushes of garbage.
- **Cold reads take 2–5× fewer gets** in the larger databases, and up to 3×
  fewer bytes: a get returns a whole chunk. Small databases and appends are
  about even.
- **Without a cache, a flush that changes many chunks costs more round
  trips** (22.6 against 17.7 for update 10): it must fetch the bases it
  writes deltas against. With the owner's cache, which every SDK uses, both
  are 2.
- **Deletes per flush grow** with scattered updates (16 a flush for update
  10): each changed chunk's previous delta dies. They ride the same sync.

## The four questions

### Does it simplify the code?

Yes, more in concepts than in lines: by a quarter with files of at most
64 MiB, by 8% once the chunk map (384 lines) took them to 10 GiB.

| Non-comment lines (`grep -vE '^\s*(//\|$)'`) | Log (main) |   CoW |
| -------------------------------------------- | ---------: | ----: |
| `core/state/src`, without the simulator      |      2,646 | 2,445 |
| The simulator's monitor                      |        308 |   291 |
| `core/state` and `core/sql` tests            |      9,131 | 8,143 |
| TLA+ specification                           |    9 files |  none |

`log.rs`, `snapshot.rs`, `hint.rs`, `page_codec.rs` and `format.rs` (2,147
lines) became `store.rs` (1,151) and `map.rs` (384); the cache went from
475 to 362. The SDKs
lost their shared-handler retry loops, and the bindings `is_current`.

Gone as concepts: the log and its segments, seals, checkpoints and their
thresholds, epochs, page keys overwritten in place, the working-set hint,
page codecs (raw, LZ4, zstd, zstd against base), the cache's generations and
page versions, readers' version marks that held back checkpoints, and
`is_current`. Added: chunks, a base and a delta, the rebase ratio, the
garbage grace, and the chunk map.

### Is correctness easier to reason about?

Yes. The log needed four issue-order rules (values before the record, build
on the record, pages after the record that logs them and before the
checkpoint record, fresh values deleted after their last record), and replay
on every open. Copy-on-write has one rule, and no recovery: the root stored
names only blobs that are stored, and nothing else is ever overwritten. The
failure table went from five rows to three, and the monitor from six
invariants, which needed the log replayed to check, to six checks on each
write, none of which replays anything. The TLA+ specification modelled the
log protocol; nothing here needs one as much, and none was written.

**Caching became trivial.** A blob never changes once a root names it, so
the cache keeps it by (namespace, database, number), forever, with nothing
to invalidate: no epochs, generations or versions, and a root that changed
under the owner (another process wrote it) costs nothing but the parked
engine. The database is a random identity in the root: an object's state
cleared and made again numbers its blobs from 0 again, under another. One
care remains: a crashed flush's blob numbers are reused by the next flush,
so a blob is cached only once a durable root names it.

### Do shared handlers work?

Yes. A reader reads the root it opened, and the blobs it names stay for 16
of the owner's flushes, so a shared handler beside the exclusive one sees
one consistent state, with no check and no retry: the SDKs' "the database
changed while this shared handler read it" loop is gone. Readers now share
the owner's process-wide cache too (they read it and never insert). The
limit: a reader that outlives 16 owner flushes fails with "is gone", and
Restate retries the handler.

### Is the log gone?

Completely: format v16 (`v16/root`, `v16/blob/<n>`), with no
migration. The benches, analyzers and results that measured the log
(`log_shipping`, `page_compression`, their scripts and CSVs, the TLA+
specification and its review) are deleted; `CACHE_RESULTS.md` and
`RESTATE_RESULTS.md` are marked as measured on the log.

## What the self-review found

A review of the whole change, before merging, found three real bugs,
all in caching, none of which the campaign had caught:

- **Two databases, one cache key.** A namespace whose state is cleared and
  made again (or the same object run by two copies of a module, as a Python
  test did) numbers its blobs from 0 again, and the cache, keyed by
  (namespace, number), served the old database's blobs. The root now
  carries a random database id, and the cache key includes it.
- **An open during a flush.** A state opened while the owner's flush ran
  could read the flush's writes before they were durable, without the sync
  an unknown outcome triggers. A flush in progress now counts as unsettled.
- **An evicted unsettled owner.** The cache could evict an owner whose last
  flush had an unknown outcome, and with it the mark that its next open
  must sync. Unsettled owners are now pinned until a sync settles them.

And smaller ones: readahead recorded the wrong chunk, so a sequential scan
never grew its window (a test now counts the gets); a fetch for a file the
root does not yet name panicked; a decoded root could name chunks of 0
pages, a based file without its base, or `u64::MAX` as a base id. The
monitor gained the leak-nothing rule, decodes record blobs, checks the
database id, and forgets what a crash may have lost when it reads a root.

For code quality: the 250-line `commit` became a `Flush` builder filled by
`commit_file`, `commit_chunk` and `commit_records`, then `write`; a settings
wrapper became plain fields; the log's leftovers (an LZ4 dependency, a JDK
for the TLA+ tools, a tracing hook, log-era wording in the docs and the
SDKs) are gone.

## Validation

- State campaign, 2,000 seeds: 78,641 flushes, 20,713 crashes, 185,144 blobs,
  39,442 rebases, 121,494 consistent reads beside the owner, no monitor
  violations.
- SQL campaign, 300 seeds: 36,000 calls, 1,642 faults, 2,605 crashes, 1,371
  unknown outcomes.
- Every state, SQL and functions test (the functions in each page-tracking
  mode), the Node.js addon's and the TypeScript tests, and the Python
  packages' tests. (The three TypeScript files that load OpenCode need
  Node.js 24; on 22 they fail to load, on main as well.) The Node.js addon's test store kept a random subset of a
  failed request's writes, "worse than the contract": it now keeps a
  prefix, the contract, which copy-on-write relies on (a root kept without
  a blob it names would be lost).

## Trade-offs and open questions

- **10 GiB per file, through a chunk map.** The root first named every
  chunk, which capped a file at 64 MiB: there (200k rows, 56 MB, aged by
  `FLUSHES=3000 WORKLOAD=200k`) ten scattered updates cost 6.5 KB a flush,
  4.9 KB of it the root. Now a radix tree names them, as ZFS's indirect
  blocks do, its top node in the root, so a flush writes a node per level
  on each path it changes. A database that grows past 10 GiB fails at the
  statement (SQLITE_FULL, `PRAGMA max_page_count`), and the session goes on.
  The fanout, set per namespace, trades a flush's bytes against a cold
  open's gets; 64 is the default:

  | Aged 56 MB (`FANOUT=… FLUSHES=3000 WORKLOAD=200k`) | Update 1, B/flush | Update 10, B/flush | Cold open: round trips / gets |
  | -------------------------------------------------- | ----------------: | -----------------: | ----------------------------: |
  | The root names every chunk (before)                |               421 |              6,500 |                             – |
  | Fanout 16                                          |               506 |              3,375 |                       6 / 240 |
  | Fanout 32                                          |               531 |              3,556 |                       6 / 120 |
  | **Fanout 64**                                      |           **544** |          **3,943** |                    **5 / 61** |
  | Fanout 128                                         |               560 |              5,028 |                        5 / 34 |
  | Fanout 256                                         |               636 |              5,576 |                        5 / 20 |

  A 280 MB database (1M rows, three levels) costs 520 B a flush for one
  update and 2.0 KB for ten, about what 56 MB does. A cold open gets the
  map whole (up to 512 nodes, 512 MiB at the default), a level a round
  trip: 285 small gets there, in 6 round trips, so that no read after
  misses it; an uncached owner pays that every session. The fresh
  databases of the table above cost the same or less than without a map
  (100k rows, update 1: 342 B, from 426).

- **Zero pages are holes.** In a file without a base, a page of zeros reads
  as absent, which SQLite never relies on.
- **Granularity.** A change rebuilds a 16 KiB chunk's delta; `CHUNK_PAGES=1`
  costs less per change but compresses worse and makes the chunk maps 4×
  larger.
- **Uncached flushes fetch bases** before they write deltas (above).
- **Restate's state grows a key per blob**, about two per chunk, which its
  UI lists unless hidden by the `v16/` prefix (the SDKs set it).
