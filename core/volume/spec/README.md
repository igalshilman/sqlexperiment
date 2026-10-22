# Protocol specification

`Protocol.tla` is the storage protocol of [DESIGN.md](../docs/DESIGN.md) as a
TLA+ state machine, and TLC checks every reachable state within small bounds.
It is the mathematical counterpart of the simulator campaigns: those sample
fault schedules at random, while this enumerates all of them up to a size.

## What is modelled

- **The store.** A Restate object's state: one value per key, and reads
  return it. Puts and deletes are queued in issue order and land from the
  front, one at a time. A sync is acknowledged once the queue is empty. A
  failed sync, or a process that dies during one, drops what is still
  queued: a prefix landed, and nothing of the abandoned attempt lands later.
- **Processes.** Two processes take turns owning the namespace; ownership
  moves only once the owner's attempt is over. Those in `Cached` keep an
  owner cache across their sessions (the record they saw, its epoch, their
  last commit, the epoch's pages and the stored values the record names)
  and lose it when they crash. After an unknown outcome they forget the
  record and keep the rest.
- **Save.** One transaction, its writes queued in order, then one sync:
  the durable record's garbage deletes; the transaction's stored value, if
  it is large; a seal of the record's inline transactions, if chosen; the
  record naming both; and, if chosen, a checkpoint: each page's latest image,
  then a record with an empty log at the next epoch, whose garbage takes the
  log's stored values. One writer: a cached process commits only on its own
  last record.
- **Open.** Read the record, replay its log (every stored value it names
  must hold its transaction, read from the cache if held), keep cached pages
  of the same epoch, and drop them for another.
- **Sessions.** A ready session reads its log, then its cache, then the page
  key; reads populate the cache.

Page values are transaction ids, so a state names which transaction last
wrote each page. A ghost history records the acknowledged transactions and at
most one whose outcome is unknown (a failed sync).

Not modelled: the SQL engine, page encodings and records (they ride in the
record), files (a page here is abstract: a page of a paged
file is its file and its number, and nothing in the protocol tells files
apart), the working-set hint, prefetch and readahead (they
change which reads happen, not their values), transactions of several stored
values, overlapping sessions within one process (reader marks), and deleted
pages.

## Properties

- `OpensReadTheHistory`: every open reads exactly the acknowledged
  transactions in order, plus the unknown one or not, and replays without a
  missing segment. Never part of a transaction, never one acknowledged but
  lost, never one an earlier open decided was not committed.
- `ReadyReadsTheHistory`: a ready session reads the history at every moment,
  through its log, its cache and the page keys; no checkpoint, garbage
  collection or other owner exposes a stale page.
- `NothingLeaks`: every stored value present is named by the durable record,
  listed as its garbage, or at a number it has not allocated.

## Running

```sh
spec/check.sh                 # MC.cfg, MCNoCache.cfg, MCMixed.cfg, MCSeal.cfg
spec/check.sh MCDeep.cfg      # three transactions, three values, two pages
```

The script downloads `tla2tools.jar` (TLA+ tools 1.8.0) on first use and
needs Java 11 or later.

| Configuration   | Bounds                                                                              | Distinct states | Result               |
| --------------- | ----------------------------------------------------------------------------------- | --------------- | -------------------- |
| `MC.cfg`        | both processes cached; 2 pages, 2 processes, 2 transactions, 2 values, 1 crash      | 594,532         | No violation         |
| `MCNoCache.cfg` | as `MC.cfg`, no cache                                                               | 32,842          | No violation         |
| `MCMixed.cfg`   | as `MC.cfg`, one process cached and one not                                         | 295,512         | No violation         |
| `MCSeal.cfg`    | as `MCMixed.cfg`, 1 page, 3 transactions, 3 values: a seal and the commit naming it | 168,230         | No violation         |
| `MCDeep.cfg`    | as `MCMixed.cfg`, 3 transactions, 3 values                                          | 8,360,534       | No violation (2 min) |

The two processes of `MC.cfg` and `MCNoCache.cfg` are symmetric, so TLC
checks one of each pair of mirrored states (`Symmetry`).

## Mutations

Each deliberately broken protocol must fail a property, or the model is too
weak to trust (all found in `MCSeal.cfg`):

| Broken rule                                                     | Found by               |
| --------------------------------------------------------------- | ---------------------- |
| Writes land in any order, not the order issued                  | `OpensReadTheHistory`  |
| The record is queued before the large transaction's value       | `OpensReadTheHistory`  |
| The record names a seal that is never put                       | `OpensReadTheHistory`  |
| The checkpoint record is queued before its pages                | `OpensReadTheHistory`  |
| Open keeps cached pages of another epoch                        | `OpensReadTheHistory`  |
| The cache keeps the values of a commit whose outcome is unknown | `OpensReadTheHistory`  |
| This owner's checkpoint keeps cached pages without its log's    | `ReadyReadsTheHistory` |
| The garbage is deleted after the record, not before it          | `NothingLeaks`         |
| The checkpoint leaves the log's values out of its garbage       | `NothingLeaks`         |

The first is the one the protocol rests on: with writes in any order, a
record can land without the values it names.

One rule no bounded run breaks: the one-writer check guards sessions that
overlap within one process, which the model leaves out.

## The code against the model

TLC checks the protocol, not the Rust code. `MonitorKv` (in
`src/simulation/monitor.rs`) connects the two. It wraps the store in the
simulator's fault campaigns and checks every request the code issues against
the model's rules:

1. A record names only stored segments written before it.
2. A record holds the record's log plus one transaction, at the next
   version, or is that record again.
3. A record that empties the log (the next epoch) comes only once every page
   the log changed is at its page key, with the log's latest image of it.
4. A page key gets only an image of a transaction in the record's log.
5. A stored segment is put only at a number the record has not allocated,
   and deleted only if it names it no more.
6. Every record decodes, its log against the segments it names.

The monitor applies each write as it is issued, since a later one can rely
on it. A failed write makes its key unknown and adds a possible value to a
page key; after a crash, a read learns what was kept.

It runs in the durability, records and per-commit campaigns with no
violation. Deliberately broken code fails them:

| Broken code                                   | Reported                                                   |
| --------------------------------------------- | ---------------------------------------------------------- |
| The checkpoint record queued before its pages | The monitor: a checkpoint with a page not known at its key |
| A stored value named but its put left out     | Open: a missing log segment, after a crash at any write    |
