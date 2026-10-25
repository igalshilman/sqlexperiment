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
