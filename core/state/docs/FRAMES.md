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

| Store   | Key                            | Holds                                                                                                           |
| ------- | ------------------------------ | --------------------------------------------------------------------------------------------------------------- |
| Restate | `v19/head`                     | The commit point: identity, frame size, version, the checkpoint and the one before, the tail's extent and parts |
| Restate | `v19/tail/<version>/<part>`    | One flush's mutation: records set, log appends and truncations, sizes, pages (a zstd frame each, or a hole)     |
| S3      | `checkpoint/<version>-<nonce>` | A pack: frames, large values, then the manifest                                                                 |

Without S3, packs are kept in Restate in parts of 64 KiB
(`v19/pack/<key>/<part>`). A tail entry over 1 MiB is split into parts.

**The manifest** is flat, compressed with zstd, at the end of the pack (the
head says where). It lists:

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
4. **Put** the pack: frames, values, manifest. One PUT.

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
entries, packs and heads together, and the gets a cold open makes.

| Workload        | Put B/flush          | Cold gets    |
| --------------- | -------------------- | ------------ |
| chat+effects    | 1907 → 2201 → 1942   | 7 → 5 → 35   |
| small: update 1 | 1112 → 1786 → 557    | 7 → 6 → 19   |
| 10k: update 1   | 1055 → 9322 → 644    | 12 → 6 → 6   |
| 10k: update 10  | 4017 → 10693 → 2037  | 9 → 6 → 7    |
| 10k: insert 1   | 2324 → 3449 → 1879   | 11 → 6 → 8   |
| 100k: update 1  | 853 → 12965 → 633    | 60 → 6 → 6   |
| 200k: update 1  | 1009 → 12444 → 667   | 62 → 6 → 8   |
| 200k: update 10 | 3141 → 108279 → 1357 | 55 → 7 → 9   |
| 1M: update 1    | 1983 → 13718 → 880   | 296 → 6 → 6  |
| 1M: update 10   | 4360 → 127000 → 1664 | 1331 → 7 → 6 |

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

**Packs in S3** (`S3=1`): dollars per million flushes at S3 Standard's
price uncached and Express One Zone's cached; tail+checkpoints → frame packs.

| Workload        | Restate B   | S3 GETs, uncached | S3 upload B | Cold S3 GETs | Standard $/M, uncached | Express $/M, cached |
| --------------- | ----------- | ----------------- | ----------- | ------------ | ---------------------- | ------------------- |
| chat+effects    | 1724 → 1937 | 1.20 → 3.07       | 185 → 176   | 4 → 4        | 0.54 → 1.29            | 0.02 → 0.02         |
| small: update 1 | 1059 → 1737 | 0.46 → 2.02       | 54 → 35     | 4 → 3        | 0.25 → 0.87            | 0.02 → 0.02         |
| 10k: update 1   | 756 → 429   | 7.64 → 7.64       | 300 → 214   | 10 → 4       | 3.13 → 3.13            | 0.02 → 0.02         |
| 10k: update 10  | 3854 → 1808 | 26.5 → 39.4       | 164 → 229   | 7 → 5        | 10.68 → 15.83          | 0.02 → 0.02         |
| 10k: insert 1   | 1603 → 1750 | 5.18 → 6.44       | 141 → 130   | 9 → 6        | 2.15 → 2.66            | 0.02 → 0.02         |
| 100k: update 1  | 522 → 400   | 13.8 → 8.2        | 331 → 234   | 58 → 4       | 5.61 → 3.34            | 0.03 → 0.02         |
| 200k: update 1  | 564 → 409   | 22.5 → 11.7       | 444 → 259   | 60 → 6       | 9.08 → 4.76            | 0.03 → 0.02         |
| 200k: update 10 | 2251 → 812  | 44.5 → 59.3       | 892 → 546   | 52 → 6       | 17.89 → 23.78          | 0.03 → 0.02         |
| 1M: update 1    | 632 → 411   | 234.1 → 10.5      | 1352 → 470  | 286 → 4      | 93.70 → 4.27           | 0.06 → 0.02         |
| 1M: update 10   | 2258 → 789  | 128.7 → 72.6      | 2102 → 876  | 534 → 4      | 51.57 → 29.11          | 0.09 → 0.02         |

A cached flush is two round trips here too, and a checkpoint one PUT.

- **Restate bytes:** about half on the larger databases, in both modes:
  a tail page costs what changed since the checkpoint. With packs in S3 a
  small database's tail is cut only every 128 flushes or 1 MiB, so its
  pages drift further from the checkpoint and cost more (small update 1:
  1737 bytes, against 557 with packs in Restate).
- **Cold opens:** the manifest and a frame, not a walk of map nodes: cold
  S3 GETs drop from up to 534 to 6. With packs in Restate a small
  database's open reads its tail, as long as its packs (chat: 35 gets, in
  4 round trips).
- **Uncached flushes** read the checkpoint's frames their pages are in,
  not chunk deltas: more GETs on small databases and ten-row updates (up
  to 4× for a small one, $0.87 rather than $0.25 a million on Standard),
  far fewer on large ones (1M update 1: $4.27 rather than $93.70). With a
  cache every workload costs $0.02 a million on Express.
- **Code:** 3,017 lines in the state crate, without comments and blanks,
  against 3,492 for tail+checkpoints and 3,051 for blob-claims; the SQL
  crate's page tidying is 50 more.
