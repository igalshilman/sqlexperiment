# Protocol specification

`Protocol.tla` is the storage protocol of [DESIGN.md](../docs/DESIGN.md) as a
TLA+ state machine, and TLC checks every reachable state within small bounds.
It is the mathematical counterpart of the simulator campaigns: those sample
fault schedules at random, while this enumerates all of them up to a size.

## What is modelled

- **The store.** One value per key, and reads return it: what is durable.
  Every put or delete ends one of three ways, independently and in any order:
  durable and acknowledged, durable with the acknowledgement lost, or no
  effect. Nothing orders two writes.
- **Processes.** Two processes take turns owning the namespace; ownership
  moves only after every request settles. Those in `Cached` keep an owner
  cache across their sessions (the record they saw, its epoch, their last
  commit and the epoch's pages) and lose it when they crash.
- **Save.** One transaction, inline in the record or in a stored segment put
  first; the record goes out once the segment is acknowledged, with the
  garbage's deletes in the same round trip. One writer: a cached process
  commits only on its own last record.
- **Checkpoint.** At any time a ready session's log is not empty: each page's
  latest image, then, once all are acknowledged, a record with an empty log at
  the next epoch, then the deletes of the stored segments no record names any
  more. A failed page put keeps the log; a record put of unknown outcome keeps
  the session on its log, and forgets the record (so a cached process commits
  no more in that session).
- **Open.** Read the record, replay its log (every stored segment it names
  must hold its transaction), keep cached pages of the same epoch, and drop
  them for another.
- **Sessions.** A ready session reads its log, then its cache, then the page
  key; reads populate the cache.

Page values are transaction ids, so a state names which transaction last
wrote each page. A ghost history records the acknowledged transactions and at
most one whose outcome is unknown (a failed commit).

Not modelled: the SQL engine, page encodings, effects (they ride in the
record), the working-set hint, prefetch and readahead (they change which
reads happen, not their values), transactions of several stored segments,
overlapping sessions within one process (reader marks), and deleted pages.

## Properties

- `OpensReadTheHistory`: every open reads exactly the acknowledged
  transactions in order, plus the unknown one or not, and replays without a
  missing segment. Never part of a transaction, never one acknowledged but
  lost, never one an earlier open decided was not committed.
- `ReadyReadsTheHistory`: a ready session reads the history at every moment,
  through its log, its cache and the page keys; no checkpoint, garbage
  collection or other owner exposes a stale page.

## Running

```sh
spec/check.sh                 # MC.cfg, MCNoCache.cfg, MCMixed.cfg
spec/check.sh MCDeep.cfg      # three transactions, three segments
```

The script downloads `tla2tools.jar` (TLA+ tools 1.8.0) on first use and
needs Java 11 or later.

| Configuration   | Bounds                                                                        | Distinct states | Result       |
| --------------- | ----------------------------------------------------------------------------- | --------------- | ------------ |
| `MC.cfg`        | both processes cached; 2 pages, 2 processes, 2 transactions, 2 segments, 1 crash | 1,702,483       | No violation |
| `MCNoCache.cfg` | as `MC.cfg`, no cache                                                         | 95,885          | No violation |
| `MCMixed.cfg`   | as `MC.cfg`, one process cached and one not                                   | 854,074         | No violation |
| `MCDeep.cfg`    | as `MCMixed.cfg`, 3 transactions, 3 segments                                  | 31,607,575      | No violation (10 minutes) |

The two processes of `MC.cfg` and `MCNoCache.cfg` are symmetric, so TLC
checks one of each pair of mirrored states (`Symmetry`).

## Mutations

Each deliberately broken protocol must fail a property, or the model is too
weak to trust:

| Broken rule                                                            | Found by               |
| ---------------------------------------------------------------------- | ---------------------- |
| A record names a stored segment before its put is acknowledged         | `OpensReadTheHistory`  |
| The checkpoint record goes out before its pages are acknowledged       | `OpensReadTheHistory`  |
| The garbage is deleted with the checkpoint record, not after it        | `OpensReadTheHistory`  |
| Open keeps cached pages of another epoch                               | `OpensReadTheHistory`  |
| This owner's checkpoint keeps cached pages without writing its log's   | `ReadyReadsTheHistory` |

Two rules no bounded run breaks, as expected: the one-writer check guards
sessions that overlap within one process, which the model leaves out; and a
session may go on committing after a checkpoint record of unknown outcome,
since ownership moves only once its sessions end, so no other process can
have seen the epoch it might put again.

## The code against the model

TLC checks the protocol, not the Rust code. `MonitorKv` (in
`src/simulation/monitor.rs`) connects the two. It wraps the store in the
simulator's fault campaigns and checks every request the code issues against
the model's rules:

1. A record names only stored segments known to hold their segment.
2. A record holds the known record's log plus one transaction, at the next
   version, or is that record again.
3. A record that empties the log (the next epoch) comes only when every page
   the known log changed is known at its page key, with the log's latest
   image of it.
4. A page key gets only an image of a transaction in the known record's log.
5. A stored segment is put only at a number the known record has not
   allocated, and deleted only if it names it no more.
6. Every record decodes, its log against the segments it names.

"Known" is what an observer can rely on: acknowledged requests, and values
read. An unknown outcome makes a segment key unknown and adds a possible
value to a page key; a record of unknown outcome leaves the known one, the
older, in place until a read learns which landed.

It runs in the save, handover and per-commit campaigns with no violation.
Deliberately broken code is caught with the exact request:

| Broken code                                           | Monitor reports                                    |
| ----------------------------------------------------- | -------------------------------------------------- |
| The checkpoint record put with the pages, not after   | A checkpoint with a page not known at its key      |
| A large transaction's record put with its segments    | A record naming an unknown segment                 |
