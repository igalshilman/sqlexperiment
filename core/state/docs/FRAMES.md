# Frame packs: a tail of mutations, checkpoints of compressed frames

An experiment, on `claude/frame-packs`, that simplifies the tail and
checkpoints of [TAIL_EXPERIMENT.md](TAIL_EXPERIMENT.md). The split stays:
Restate keeps a tail of small mutations, one entry per flush, and S3 (or
Restate itself) keeps checkpoints, each one object. What changes is the
checkpoint's format. Packs made copy-on-write unnecessary, so the radix chunk
map, chunks, bases and deltas are gone. A file is a row of frames, each
frame `frame_pages` pages (16: 64 KiB) compressed with zstd on its own, and
a checkpoint rewrites the frames the tail changed.

## Layout

| Store   | Key                            | Holds                                                                                                         |
| ------- | ------------------------------ | ------------------------------------------------------------------------------------------------------------- |
| Restate | `v19/head`                     | The commit point: identity, frame size, version, the checkpoint and the one before, the tail's extent and parts |
| Restate | `v19/tail/<version>/<part>`    | One flush's mutation: records set, log appends and truncations, sizes, pages (a zstd frame each, or a hole)    |
| S3      | `checkpoint/<version>-<nonce>` | A pack: frames, large values, then the manifest                                                               |

Without S3, packs are kept in Restate in parts of 64 KiB
(`v19/pack/<key>/<part>`). A tail entry over 1 MiB is split into parts.

**The manifest** is flat, compressed with zstd, at the end of the pack (the
head says where). It lists:

- every file: its size, its base's id, and where each of its frames is (pack, offset, length), or nothing for a hole;
- every record: inline if small, else where its frame is;
- every log: its first index, and its entries, inline or where;
- the bytes each pack it names holds, for compaction.

## Flush

1. **Entry.** The mutation, as is: records' values, log changes (appends
   that the same mutation truncates are dropped), sizes, and each page
   written as a zstd frame of its own, against its base page if the file
   has a base.
2. **Put** the entry's parts, then the head; **sync**: one round trip.
3. **Apply** the entry to the overlay in memory: pages written since the
   checkpoint, the records and logs as they are, and, for a file that
   shrank, the page count it was cut to.

## Checkpoint

When the tail is long enough (`checkpoint_entries`, `checkpoint_bytes`), a
flush first writes the durable state as a checkpoint, then names it in its
head:

1. **Frames.** Every frame the overlay touched (or a cut reached) is
   rebuilt from the checkpoint's frame (fetched, in parallel) and the
   overlay's pages, then compressed again. A frame that is all hole is not
   stored.
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
  checkpoint; then, together, stored values and the frames of small files.
  An owner whose cache still holds this head reuses the state it left:
  one round trip.
- **Read:** a page from the overlay, else from its frame (a ranged GET,
  cached forever by location; a pack never changes), else a hole.

## What it replaced

| tail+checkpoints (v18)                                         | frame packs (v19)                         |
| -------------------------------------------------------------- | ----------------------------------------- |
| Tail entries of copy-on-write blobs (chunk deltas, map nodes)  | Tail entries that are the mutation itself |
| A radix map of chunks per file, each a base and a delta        | A flat row of frames per file             |
| A manifest of blob → location, plus a root and its map nodes   | One flat manifest                         |
| Blob numbers, next-blob rules, rebase ratios, fanouts          | Locations only                            |

## Results

`flush_costs`, `S3=1`, the same workloads and defaults as tail+checkpoints
(`claude/log-checkpoints`), per flush; dollars per million flushes at S3
Standard's and Express One Zone's prices. Each cell reads tail+checkpoints →
frame packs.

| Workload        | Cache | Restate B   | S3 GETs       | S3 upload B   | S3 stored MiB | Cold S3 GETs | Standard $/M  | Express $/M  |
| --------------- | ----- | ----------- | ------------- | ------------- | ------------- | ------------ | ------------- | ------------ |
| chat+effects    | no    | 1724 → 2147 | 1.20 → 1.04   | 185 → 291     | 0.07 → 0.09   | 4 → 2        | 0.54 → 0.48   | 0.05 → 0.05  |
| 10k: update 1   | no    | 756 → 3928  | 7.64 → 4.40   | 300 → 8099    | 1.17 → 1.98   | 10 → 4       | 3.13 → 1.84   | 0.25 → 0.17  |
| 10k: update 10  | no    | 3854 → 37090| 26.5 → 13.4   | 164 → 35093   | 1.13 → 1.98   | 7 → 4        | 10.68 → 5.68  | 0.81 → 0.58  |
| 100k: update 1  | no    | 522 → 3858  | 13.8 → 7.0    | 331 → 46203   | 10.93 → 21.20 | 58 → 4       | 5.61 → 2.87   | 0.43 → 0.37  |
| 200k: update 10 | no    | 2251 → 36553| 44.5 → 40.6   | 892 → 346041  | 21.96 → 42.02 | 52 → 4       | 17.89 → 16.57 | 1.36 → 2.32  |
| 1M: update 1    | no    | 632 → 3829  | 234.1 → 6.5   | 1352 → 30045  | 109.7 → 108.9 | 286 → 4      | 93.70 → 2.67  | 7.04 → 0.30  |
| 1M: update 10   | no    | 2258 → 36316| 128.7 → 54.7  | 2102 → 488690 | 109.9 → 119.3 | 534 → 4      | 51.57 → 22.19 | 3.89 → 3.17  |
| 1M: update 1    | yes   | 633 → 3830  | 1.13 → 0.03   | 1352 → 30047  | 109.7 → 108.9 | 286 → 4      | 0.53 → 0.09   | 0.06 → 0.11  |
| 1M: update 10   | yes   | 2259 → 36324| 2.09 → 0.08   | 2102 → 488704 | 109.9 → 119.3 | 534 → 4      | 0.92 → 0.34   | 0.09 → 1.53  |

Both make one Restate round trip a cached flush, and one PUT a checkpoint.

- **Requests:** fewer. A cold read is the manifest and a frame, not a
  walk of map nodes, so cold GETs drop from up to 534 to 4, and uncached
  flushes cost a third to a thirtieth on S3 Standard.
- **Bytes:** more.
  - A tail entry is 5–16× larger, since a page is compressed alone, not
    against its previous image.
  - A checkpoint rewrites whole frames, so it uploads 25–230× more bytes.
  - Compaction at ½ live stores up to 2× what is live.

  On Standard, upload bytes are free and storage is a rounding error; on
  Express they make a cached ten-row update cost $1.53 rather than $0.09.
- **Frame size:** `FRAME_PAGES=4` (16 KiB) uploads 3–4× fewer bytes and
  stores less, for slightly more GETs uncached:
  - 1M update 10, cached: $0.43 rather than $1.53 on Express.
  - 100k update 1: 13.9 MiB stored rather than 21.2.
- **Code:** 2,800 lines in the state crate, without comments and blanks,
  against 3,492 for tail+checkpoints and 3,051 for blob-claims; the monitor
  is 392 lines against 481.
