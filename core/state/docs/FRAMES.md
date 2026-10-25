# Frame packs: a tail of mutations, checkpoints of compressed frames

An experiment, on `claude/frame-packs`, that simplifies the tail and
checkpoints of [TAIL_EXPERIMENT.md](TAIL_EXPERIMENT.md). The split stays:
Restate keeps a tail of small mutations, one entry per flush, and S3 (or
Restate itself) keeps checkpoints, each one object. What changes is the
checkpoint's format. Packs made copy-on-write unnecessary, so the radix chunk
map, chunks, bases and deltas are gone. A file is a row of frames, each
frame `frame_pages` pages (4: 16 KiB) compressed with zstd on its own, and
a checkpoint rewrites the frames the tail changed, or patches the pages it
changed in them.

## Layout

| Store   | Key                                     | Holds                                                                                                           |
| ------- | --------------------------------------- | --------------------------------------------------------------------------------------------------------------- |
| Restate | `v20/head`                              | The commit point: identity, frame size, version, the checkpoint and the one before, the tail's extent and parts |
| Restate | `v20/tail/<version>/<part>`             | One flush's mutation: records set, log appends and truncations, sizes, pages (a zstd frame each, or a hole)     |
| Restate | `v20/manifest/<version>-<nonce>/<part>` | A checkpoint's manifest: where every frame, record and log entry is                                             |
| S3      | `checkpoint/<version>-<nonce>`          | A pack: frames and large values                                                                                 |

Without S3, packs are kept in Restate in parts of 64 KiB
(`v20/pack/<key>/<part>`). A tail entry over 1 MiB, and a manifest over 64
KiB, are split into parts.

**The manifest** is flat, compressed with zstd, in Restate beside the head
(which gives its length), wherever the packs are. It lists:

- every file: its size, its base's id, where each of its frames is (pack, offset, length), or nothing for a hole, and where its patches are (below);
- every record: inline if small, else where its frame is;
- every log: its first index, and its entries, inline or where;
- the bytes each pack it names holds, for compaction.

## Flush

1. **Entry.** The mutation, as is: records' values, log changes (appends
   that the same mutation truncates are dropped), sizes, and each page
   written as a zstd frame of its own, compressed against its _reference_:
   the page as the head's checkpoint holds it (its patch, else its frame),
   else its base's page. The reference stays the same for the whole tail,
   so an entry decodes alone, and a small change to a page costs bytes in
   proportion to what changed since the checkpoint.
2. **Put** the entry's parts, then the head; **sync**: one round trip.
3. **Apply** the entry to the overlay in memory: pages written since the
   checkpoint, the records and logs as they are, and, for a file that
   shrank, the page count it was cut to.

## Checkpoint

When the tail is long enough, a flush first writes the durable state as a
checkpoint, then names it in its head. Long enough is `checkpoint_entries`
(128) entries, or `checkpoint_bytes` (1 MiB); with packs in Restate, where a
checkpoint costs only its bytes, at most the bytes the checkpoint's packs
hold, so the tail of a small database stays short.

1. **Frames.** A frame the overlay changed only a few pages of is
   _patched_: each page it changed is put as a patch, compressed against
   the frame's page, while its patches stay within half the frame's size.
   Any other frame the overlay touched (or a cut reached) is rebuilt from
   the checkpoint's frame and patches (fetched, in parallel) and the
   overlay's pages, then compressed again, without patches. A frame that
   is all hole is not stored.
2. **Values.** Records set since, and log entries appended since: inline
   in the manifest if small, else a frame of their own.
3. **Compaction.** A pack that the manifest needs less than half of has
   its live ranges copied, raw, into the new pack.
4. **Put** the pack: frames, values. One PUT (none if it stores nothing
   outside its manifest), and the manifest, queued in Restate before the
   head.

The flush's head names the checkpoint, and its sync deletes the tail that
the checkpoint before it covered. The tail since then stays for readers that
opened on that checkpoint. Garbage collection deletes packs older than the
head's previous checkpoint that neither of its checkpoints names. A
checkpoint that fails fails nothing: the tail grows, and the next flush
tries again.

A file made with a base needs it to rebuild its frames, so it takes no
write unless it is opened with that base.

## Open and read

- **Open:** the head; then, together, the manifest and the tail past the
  checkpoint; then, together, stored values and the frames and patches of
  small files. An owner whose cache still holds this head reuses the state
  it left: one round trip.
- **Read:** a page from the overlay, else its patch, else from its frame
  (a ranged GET each, cached forever by location; a pack never changes),
  else a hole.

## What it replaced

| tail+checkpoints (v18)                                        | frame packs (v19)                         |
| ------------------------------------------------------------- | ----------------------------------------- |
| Tail entries of copy-on-write blobs (chunk deltas, map nodes) | Tail entries that are the mutation itself |
| A radix map of chunks per file, each a base and a delta       | A flat row of frames per file             |
| A manifest of blob → location, plus a root and its map nodes  | One flat manifest                         |
| Blob numbers, next-blob rules, rebase ratios, fanouts         | Locations only                            |

## Results

`flush_costs`, the same workloads and defaults as tail+checkpoints
(`claude/log-checkpoints`), per flush, with a cache unless noted. Each
cell reads tail+checkpoints → the first frame packs (whole frames, 4
pages) → frame packs as they are now.

**Packs in Restate** (the default): bytes put to Restate per flush, tail
entries, packs, manifests and heads together, and the gets a cold open
makes.

| Workload        | Put B/flush          | Cold gets    |
| --------------- | -------------------- | ------------ |
| chat+effects    | 1907 → 2201 → 1938   | 7 → 5 → 35   |
| small: update 1 | 1112 → 1786 → 554    | 7 → 6 → 19   |
| 10k: update 1   | 1055 → 9322 → 638    | 12 → 6 → 6   |
| 10k: update 10  | 4017 → 10693 → 2032  | 9 → 6 → 7    |
| 10k: insert 1   | 2324 → 3449 → 1876   | 11 → 6 → 8   |
| 100k: update 1  | 853 → 12965 → 628    | 60 → 6 → 6   |
| 200k: update 1  | 1009 → 12444 → 660   | 62 → 6 → 8   |
| 200k: update 10 | 3141 → 108279 → 1350 | 55 → 7 → 8   |
| 1M: update 1    | 1983 → 13718 → 873   | 296 → 6 → 6  |
| 1M: update 10   | 4360 → 127000 → 1657 | 1331 → 7 → 6 |

Of those bytes, the manifests are 4 (small) to 300 (1M rows) per flush: a
checkpoint rewrites its manifest whole, about 1.7 bytes per frame (30 KiB
for 1M rows), once in up to 128 flushes.

A cached flush is two round trips in all three: the head's read and the
sync.

What brought the bytes down from the first frame packs:

- **Tail pages against the checkpoint's page.** A small change to a page
  costs what changed since the checkpoint, not a whole compressed page.
- **Patches.** A checkpoint puts the pages that changed in a frame as
  patches against the frame's page, while they stay within half the frame,
  instead of rewriting the frame: the large databases' checkpoints are
  30–200× smaller.
- **A relative trigger.** With packs in Restate, a checkpoint costs only its
  bytes, so the tail is cut once it holds more than the checkpoint's packs:
  a small database checkpoints often and keeps a short tail.
- **No sync for a pack.** Restate keeps writes in order and a crash keeps a
  prefix, so a pack's parts, put before the head that names them, are
  durable with its sync. Garbage collection lists once and syncs once.
- **Tidy pages.** The engine reuses page buffers without zeroing them, so a
  new page's free space held an old page's bytes, which compress badly.
  The SQL crate zeroes the bytes a b-tree page does not read before it
  writes it, as SQLite's `secure_delete` does: a cached insert puts 1879
  bytes rather than 3136.

**Packs in S3** (`S3=1`): what frame packs cost on S3 Standard, in
dollars per million flushes. Uncached is a process that reads the pages a
flush needs from S3; cached, one whose page cache holds them.

| Workload             | Uncached $/M flushes | Cached $/M flushes | Stored in S3 | Storage $/month |
| -------------------- | -------------------- | ------------------ | ------------ | --------------- |
| chat+effects         | $1.02                | $0.07              | 0.06 MiB     | ~$0             |
| small: update 1      | $0.60                | $0.07              | 0.01 MiB     | ~$0             |
| 10k rows: update 1   | $2.73                | $0.08              | 1.2 MiB      | $0.00003        |
| 10k rows: update 10  | $15.42               | $0.08              | 1.1 MiB      | $0.00003        |
| 10k rows: insert 1   | $2.25                | $0.08              | 1.1 MiB      | $0.00003        |
| 100k rows: update 1  | $2.93                | $0.08              | 10.8 MiB     | $0.00024        |
| 200k rows: update 1  | $4.35                | $0.08              | 21.5 MiB     | $0.00048        |
| 200k rows: update 10 | $23.38               | $0.08              | 21.6 MiB     | $0.00048        |
| 1M rows: update 1    | $3.86                | $0.08              | 108 MiB      | $0.0024         |
| 1M rows: update 10   | $28.70               | $0.08              | 108 MiB      | $0.0024         |

- **Cached:** the checkpoints, nearly all of it: a PUT and a LIST each,
  about once in 128 flushes, at $5 a million requests.
- **Uncached:** the GETs, at $0.40 a million: a flush reads the frames
  its pages are in, so the cost follows the rows it touches more than the
  database's size. The manifest is in Restate, so an open makes no GET
  for it.
- **Storage:** a rounding error at any size.

**Read latency** (`read_latency`, `S3=1`): an uncached open and query,
with a Restate request taking 2 ms and an S3 request 20 ms, on a virtual
clock where requests issued together overlap; milliseconds at the median.

| Database  | Open | Point (primary key) | Point (index) | Range of 100 rows |
| --------- | ---- | ------------------- | ------------- | ----------------- |
| 100 rows  | 4    | 0                   | 0             | 0                 |
| 10k rows  | 24   | 40                  | 60            | 80                |
| 100k rows | 24   | 40                  | 80            | 80                |
| 1M rows   | 24   | 60                  | 120           | 100               |

- **Open:** the head, then the manifest and the tail together, from
  Restate (2 round trips); then the database's first frame from S3. With
  the manifest in the pack, the open was 42 ms: its GET, then the frame's.
- **Queries:** each S3 round trip is a b-tree level whose frame is not
  read yet, 2 to 6 of them; a second query in the same session reuses the
  upper levels (20 to 80 ms).
- **100 rows:** the database is still in its tail, in Restate.
- **Packs in Restate:** the same requests at 2 ms: an open in 6 ms, a
  query in 4 to 12.

Against tail+checkpoints, with a cached flush two round trips in both and
a checkpoint one PUT:

- **Restate bytes:** about half on the larger databases, in both modes:
  a tail page costs what changed since the checkpoint. With packs in S3 a
  small database's tail is cut only every 128 flushes or 1 MiB, so its
  pages drift further from the checkpoint and cost more (small update 1:
  1735 bytes, against 554 with packs in Restate).
- **Cold opens:** the manifest (in Restate) and a frame, not a walk of map
  nodes: cold S3 GETs drop from up to 534 to 5. With packs in Restate a small
  database's open reads its tail, as long as its packs (chat: 35 gets, in
  4 round trips).
- **Uncached flushes** read the checkpoint's frames their pages are in,
  not chunk deltas: more GETs on small databases and ten-row updates (up
  to 2.4× for a small one, $0.60 rather than $0.25 a million on Standard),
  far fewer on large ones (1M update 1: $3.86 rather than $93.70).
- **Code:** 3,017 lines in the state crate, without comments and blanks,
  against 3,492 for tail+checkpoints and 3,051 for blob-claims; the SQL
  crate's page tidying is 50 more.
