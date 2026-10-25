# A tail in Restate, checkpoints in S3

An experiment, on `claude/log-checkpoints`: the same state API (records,
paged files and logs, changed through a `Mutation` and made durable by its
flush), over a storage split by what each store is good at.

- **Restate keeps the tail**: each flush appends its mutation, as one small
  entry, to a tail of entries since the last checkpoint. Restate fences it,
  orders it, and does not bill per request. A flush is one round trip, and
  never waits on S3.
- **S3 keeps checkpoints**: now and then, the state as of a durable version,
  materialized, is written to S3 as one object (a pack), and the tail it
  covers is dropped from Restate. S3 is cheap per byte and dear per request,
  so it gets few, large writes.

An open reads the latest checkpoint and replays the tail on top, locally.

## Why

On `claude/blob-claims`, every flush writes copy-on-write blobs: a delta per
changed chunk and a node per changed map path, nearly all under 512 bytes.
In Restate alone that is fine, but the database lives in Restate, key by
key (58 MB and 33,000 keys for 100 MB of text). With the blobs in S3, the
bill is all PUT requests: $15 per million flushes for a single-row update,
$109 to $146 for ten rows ([S3_EXPERIMENT.md](S3_EXPERIMENT.md)), while
storage costs next to nothing. S3 wants the writes batched, and Restate
already orders and fences the small ones: a tail and checkpoints give each
store the writes it is good at.

## Layout

| Store   | Key                            | Holds                                                                                                                              |
| ------- | ------------------------------ | ---------------------------------------------------------------------------------------------------------------------------------- |
| Restate | `v18/head`                     | The commit point: the database's identity, its version, the checkpoint it builds on, and the ones kept for readers                 |
| Restate | `v18/tail/<version>`           | One flush's mutation: the records it set, the pages it wrote, the sizes it set, the log entries it appended and truncated          |
| S3      | `checkpoint/<version>-<nonce>` | A pack: the blobs a checkpoint wrote, then its manifest (the root, as on `claude/blob-claims`, and where each blob it names lives) |

Without S3, the packs go to Restate as well, in parts of 64 KiB
(`v18/pack/<nonce>/<part>`): one design, two places for checkpoints.

**The checkpoint format is today's.** A checkpoint is a root, chunk maps and
blobs (bases and deltas of chunks of 4 pages, map nodes, large records, log
entries), exactly as a flush on `claude/blob-claims` writes them, but all of
a checkpoint's new blobs go into one pack, and its manifest says, for each
blob it names, which pack holds it and where. A checkpoint is copy-on-write
over the one before: it writes the chunks the tail changed, and names the
rest in older packs. Making one is the existing flush, of the whole tail as
one mutation, into a pack instead of keys.

## Flush

1. **Entry.** Encode the mutation: records' values; each written page as a
   zstd frame, compressed against the page's previous image where the state
   holds it (SQL reads a page before it writes it; a VM's memory has its
   golden image), else alone; sizes; log appends and truncations.
2. **Put** `v18/tail/<v+1>`, then `v18/head` at version `v+1`; **sync**:
   one round trip. The head is the commit: a crash keeps a prefix, so an
   entry without its head is ignored, and the next flush puts over it
   (Restate fences, so only this owner writes there).
3. **Apply** the entry to the state's overlay: the tail, materialized, in
   memory.

A flush past `max_entry_bytes` (a large load) skips the tail: it writes a
checkpoint of the state with its mutation, then the head.

## Checkpoint

When the tail passes `checkpoint_entries` (128) or `checkpoint_bytes`
(1 MiB), the next flush first checkpoints the durable state, before its own
Restate write:

1. Commit the overlay, as one mutation, onto the last checkpoint: the
   existing copy-on-write flush, writing into a pack.
2. **Put the pack** to S3, as `checkpoint/<version>-<nonce>`: one PUT,
   whatever the tail changed (a multipart upload past a few MiB).
3. The flush's head then names the new checkpoint, and its sync deletes the
   tail entries the checkpoint before it covered: Restate keeps at most two
   checkpoints' worth of tail.

The checkpoint adds no Restate round trip, and its failure fails nothing:
the flush goes on, the tail grows, and the next flush tries again. S3 being
down stops checkpoints, not writes (up to a hard cap of tail).

## Open and read

- **Open:** get the head; then, together, the checkpoint's manifest (one
  ranged GET from S3, cached) and the tail entries (one round trip of
  gets). An owner whose cache remembers the head it wrote reads nothing
  more: its overlay and checkpoint are parked, so a warm open is one
  round trip, as now.
- **Read:** a page in the overlay is materialized from its frames, in
  order, on its first read (a frame compressed against the previous image
  needs it: the checkpoint's page, read anyway); any other page is the
  checkpoint's, a ranged GET of its chunk from the pack, cached forever (a
  pack never changes).
- **Readers** read the head, the checkpoint and the tail at open, and then
  only the checkpoint's packs.

## Garbage

- **Tail entries:** dropped from Restate when a checkpoint two checkpoints
  later is adopted, so a reader that opened on the one before still finds
  its tail.
- **Packs:** before writing a checkpoint, the owner lists the checkpoint
  prefix in S3 (one LIST per 1,000 objects; packs are few) and deletes every
  object that neither the head's checkpoint nor the one kept for readers
  names: the packs no live blob is in, a crashed or fenced attempt's packs,
  and late uploads.
- **Compaction:** a pack whose live bytes fall below half is rewritten into
  the next checkpoint's pack, its live blobs copied forward.

## Zombies, crashes and the cache

- **Restate fences the head and the tail**: a fenced attempt's flushes fail.
- **Pack names are unique** (a version and a random nonce): a fenced
  attempt's checkpoint, or a late upload, lands at a key no head names, and
  is deleted as garbage. Nothing in S3 is ever overwritten, so S3 needs no
  claims and no conditional puts.
- **The cache keys blobs by their pack**, so two attempts' blobs of the same
  number never meet.
- **Crashes:** a crash before the head's sync loses the flush, as now; one
  after an upload and before the head leaves an unnamed pack, deleted at the
  next checkpoint.

## What it should cost

From the measurements on `claude/blob-claims` (a delta per changed chunk:
240 to 2,700 bytes a flush), with a checkpoint every 128 flushes:

| Per flush       | Blobs in S3 (`claude/blob-claims`) | Tail and checkpoints                   |
| --------------- | ---------------------------------- | -------------------------------------- |
| Round trips     | 1 Restate + 1 S3 (+1 claim)        | 1 Restate                              |
| Restate writes  | the root and claim                 | an entry (~0.2 to 3 KB) and the head   |
| S3 requests     | 1.2 to 29 PUTs                     | ~0.02 (a PUT and a LIST per 128)       |
| S3, per million | $6 to $146 (Standard)              | about $0.08 (Standard)                 |
| Restate holds   | the root                           | at most two checkpoints' worth of tail |

A checkpoint uploads what the tail changed since the last one: for random
single-row updates in a 10k-row table, most of the database (about 1 MB) per
128 flushes; in a 1M-row one, about 0.5 MB. Upload bytes are free on S3
Standard.

## Trade-offs

- **Cold opens replay.** Up to 128 entries, fetched in one round trip and
  decoded as their pages are read. A cold read of a page not in the tail is
  an S3 ranged GET: slower than Restate's first byte.
- **A checkpoint flush is slower**: one S3 PUT (and a LIST) more, every 128
  flushes. Later, a reader beside the owner could write checkpoints, for the
  owner to adopt at its next flush.
- **Memory:** the overlay holds the tail's pages, materialized: bounded by
  the thresholds.
- **Two formats**: the tail's entries, and the checkpoints'. The second is
  the existing one, and its tests carry over.

## Plan

1. Packs: the manifest, a `KvStore` view that reads blobs from packs and
   writes a checkpoint's into one, and the 64 KiB-part store for Restate.
2. The tail: the head, the entry codec, the overlay.
3. The state on them: open, read, flush, checkpoint, garbage; readers and
   the cache.
4. Tests: the campaigns, crashes at every write, zombies and S3's partial
   failures (from `claude/blob-claims`).
5. The cost bench, against `claude/blob-claims`'s numbers.
6. A binding (TypeScript or Python) on S3.
