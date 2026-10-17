# Protocol specification

`Protocol.tla` is the storage protocol of [DESIGN.md](../docs/DESIGN.md) as a
TLA+ state machine, and TLC checks every reachable state within small bounds.
It is the mathematical counterpart of the simulator campaigns: those sample
fault schedules at random, while this enumerates all of them up to a size.

## What is modelled

- **The store.** Each key has a visible and a durable value. Every put or
  delete ends one of four ways: durable and acknowledged, durable with the
  acknowledgement lost, visible but not durable (weak backends), or no effect.
  A store loss drops what is visible but not durable.
- **Processes.** Two processes take turns owning the namespace; ownership
  moves only after every request settles. Each keeps an owner cache across
  its sessions (the `meta` it saw, a claim, its last commit and cached pages)
  and loses it when it crashes.
- **Save.** One transaction of one or two segments; the commit part is sent
  once the first part is acknowledged. In the same round trip: a checkpoint
  slice (any prefix of whole transactions, a superset of the real policy),
  garbage deletes behind the durable `meta`, and with a cache the claimed
  `meta`. Without a cache `meta` follows in a second round trip. Cleanup
  failures keep what was durable.
- **Open.** Read `meta`, validate it against the cache, refresh cached pages
  from the log after another writer (or drop them), replay, and on a weak
  backend confirm the tail and `meta` this owner did not write.
- **Sessions.** A ready session reads its log, then its cache, then the page
  key; reads populate the cache.

Page values are transaction ids, so a state names which transaction last
wrote each page. A ghost history records the acknowledged transactions and at
most one whose outcome is unknown (a failed commit).

Not modelled: the SQL engine, page encodings, the working-set hint, prefetch
and readahead (they change which reads happen, not their values), large
self-checkpointing saves, overlapping sessions within one process (reader
marks), and deleted pages.

## Properties

- `OpensReadTheHistory`: every open reads exactly the acknowledged
  transactions in order, plus the unknown one or not. Never part of a
  transaction, never one acknowledged but lost, never one an earlier open
  decided was not committed.
- `ReadyReadsTheHistory`: a ready session reads the history at every moment,
  through its log, its cache and the page keys; no checkpoint, garbage
  collection or other owner exposes a stale page.

## Running

```sh
spec/check.sh                 # MC.cfg, MCNoCache.cfg, MCStrong.cfg
spec/check.sh MCDeep.cfg      # three transactions
```

The script downloads `tla2tools.jar` (TLA+ tools 1.8.0) on first use and
needs Java 11 or later.

| Configuration   | Bounds                                                                          | Distinct states | Result                                  |
| --------------- | ------------------------------------------------------------------------------- | --------------- | --------------------------------------- |
| `MC.cfg`        | cache, weak backend; 2 pages, 2 processes, 2 transactions, 4 sequences, 1 crash | 3,972,016       | No violation                            |
| `MCNoCache.cfg` | as `MC.cfg`, no cache                                                           | 417,252         | No violation                            |
| `MCStrong.cfg`  | as `MC.cfg`, strong backend (no confirmation)                                   | 267,532         | No violation                            |
| `MCDeep.cfg`    | as `MC.cfg`, 3 transactions, 5 sequences                                        | 91,795,185      | No violation (34 minutes on four cores) |

The two processes are symmetric, so TLC checks one of each pair of mirrored
states (`Symmetry`). Bounds are small by design: a cache's `meta` moves only
at a third save (commit, checkpoint into a claim, record the claim), so
garbage collection and refreshing another owner's changes first appear in
`MCDeep.cfg`. Dropping every cached page because the changed segments are
already collected needs at least four transactions and is outside these
bounds; the simulator's handover campaign covers it.

## Mutations

Each deliberately broken protocol must fail a property, or the model is too
weak to trust:

| Broken rule                                                                | Found by               |
| -------------------------------------------------------------------------- | ---------------------- |
| Open skips confirming what it did not write (weak backend)                 | `OpensReadTheHistory`  |
| `meta` passes a checkpoint whose page puts failed                          | `ReadyReadsTheHistory` |
| The commit part is sent before the first part is acknowledged              | `OpensReadTheHistory`  |
| Garbage is deleted up to the claim, not the durable `meta` (`MCDeep.cfg`)  | `OpensReadTheHistory`  |
| Open keeps cached pages another owner's checkpoint replaced (`MCDeep.cfg`) | `OpensReadTheHistory`  |

## The code against the model

TLC checks the protocol, not the Rust code. `MonitorKv` (in
`src/simulation/monitor.rs`) connects the two. It wraps the store in the
simulator's fault campaigns and checks every request the code issues against
the model's invariants:

1. A commit part is sent only once the transaction's earlier parts are known
   (durable).
2. A transaction starts where a known commit ends.
3. `meta` passes a transaction only when every page it changed holds, in
   every possible outcome of the requests so far, that image or a newer
   committed one.
4. A segment is deleted only below the known log start.
5. A page key gets only an image of a known committed transaction the log
   still covers.

"Known" is what an observer can rely on: acknowledged requests, or values
read from a store whose visible values are durable. An unknown outcome makes a
log key unknown and adds a possible value to a page key.

It runs in the save campaign (984 interrupted saves, about 71,000 requests),
the handover campaign (400 tenures of two owners) and the per-commit campaign
(621 cases), with no violation. Deliberately broken code is caught with the
exact request:

| Broken code                                             | Monitor reports                                  |
| ------------------------------------------------------- | ------------------------------------------------ |
| Garbage deleted up to the claim, not the durable `meta` | Segments deleted at or after the known log start |
| The commit part sent with the earlier parts             | A commit part whose earlier part is not known    |
| A failed checkpoint treated as done                     | `meta` passing a page not known at its key       |
