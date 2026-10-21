# Current storage format

Only this format (v15) is implemented. Use a dedicated namespace. Constants and
key construction live in `src/format.rs`; there is no migration or format
discovery. Storage is assumed reliable (a value reads back as written), so
pages have no checksums; decoding checks structure and the bounds below. A log
segment's zstd frame carries one.

| Key                                                         | Value                                                                                                            |
| ----------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------- |
| `v15/commit`                                                | Commit record, with the log; absent until the first                                                              |
| `v15/log/<16-digit hex number>`                             | Stored log value: segments, sealed or of a large transaction; immutable, written before the record that names it |
| `v15/page/<8-digit hex file id>/<16-digit hex page number>` | Checkpointed page of a paged file, overwritten in place                                                          |
| `v15/hint`                                                  | Working-set hint; advisory, may be absent or stale                                                               |

The commit record is the only key whose write commits. Integers are LEB128
varints unless stated otherwise.

## Commit record

| Field             | Encoding                                                                                                                                                                                                                                                                         |
| ----------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Magic             | `SC15`                                                                                                                                                                                                                                                                           |
| Version           | Commits ever made; at most 2^62                                                                                                                                                                                                                                                  |
| Epoch             | Checkpoints ever recorded; at most 2^62                                                                                                                                                                                                                                          |
| Next segment      | The number the next stored value gets; at most 2^62                                                                                                                                                                                                                              |
| Garbage           | Count, then ascending numbers, each from the one before: stored values the record does not name, to delete; below the next                                                                                                                                                       |
| Next file         | The id the next new file gets; at most 2^16; ids are never reused                                                                                                                                                                                                                |
| Directory         | Count, then per file: its id (ascending, as the gap after the previous id, less one), its name's length (at most 255) and UTF-8 bytes, then `0` and its value as a part (a record), or `1` and its length in 512-byte units, at most 2^44 bytes (a paged file); names are unique |
| Transaction count | At most the version, and 2^20                                                                                                                                                                                                                                                    |
| Transactions      | Each: its change count, then per paged file it changed (ascending ids, each once): the file's id, its length after the transaction in 512-byte units, its part count and its parts (segments of that file's pages)                                                               |
| Chain count       | At most 2^20                                                                                                                                                                                                                                                                     |
| Chain             | The host's values, each a part                                                                                                                                                                                                                                                   |

A part is `0`, a length and inline bytes (a segment of at most 1 MiB, or a
chain value or record of at most 16 MiB), or `1`, the number of a stored value
(below the next segment, not in the garbage) and the bytes' index in it. A
paged file's length in the directory is its length after the last
transaction that changed it. Nothing may follow. A missing record means
version 0, epoch 0 and no files.

The files are records and paged files. The SQL database is the paged file
`db`; others are the host's (`Session::file`). A file is never deleted, only
emptied, and its id is never reused.

The transactions are the log since the last checkpoint, in commit order.
Pages they change are authoritative from the log; any other page of a file is
read at `page/<file>/<n>`, and is a hole (zeros) if absent. A commit without
page changes adds none.

The records are the host's (`Session::record`): named whole values, each
replaced whole by a commit that sets it. One of at most the inline budget
(1 KiB by default) is inline; a larger one is a stored value of its own,
written by the commit that sets it. Checkpoints keep them. A record never
set reads as empty.

The chain is the host's (`Session::seal`): opaque values in the order it
appended them since it last restarted the chain. Checkpoints keep it. A
stored value may hold log segments and chain values together (a seal).

## Log segments

A transaction's changed pages, in segments of at most 240 entries. A stored
value at `log/<n>` is `SL15`, the segment count, then each segment's length
and bytes: one segment of a large transaction, or the segments a commit
sealed (the record's inline log, in order).

| Field       | Encoding                                                                                                                                                           |
| ----------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| Entry count | At most 240                                                                                                                                                        |
| Entries     | Ascending pages of one file (below 2^32): the gap after the previous entry's page (for the first, the page) shifted left once, plus 1 for an image or 0 for a hole |
| Images      | One zstd frame with a checksum, of the entries' images (4096 bytes each) in order; absent when all are holes                                                       |

The frame's dictionary is a raw prefix: the previous image of each entry with
an image, in entry order, where it has one. A page's previous image is its
latest in the log before this segment (since the last checkpoint), in the
same file: replay rebuilds them from an empty chain. A segment is at most 1 MiB.

## Pages

Tag u8 (0 raw, 1 LZ4 block, 2 zstd frame), logical length u16 LE (4096), then
payload. Raw payloads are exactly 4096 bytes. Compressed payloads must be
nonempty and smaller than 4096 bytes; decoding must produce exactly 4096 bytes.
The encoder (zstd level 3 by default) uses raw storage when compression does
not shrink the payload. Maximum value: 4099 bytes.

An absent `page/<n>` is a zero-filled hole.

## Working-set hint

| Field       | Encoding                                                     |
| ----------- | ------------------------------------------------------------ |
| Magic       | `SH15`                                                       |
| Entry count | u16 LE; at most 256                                          |
| Pages       | Ascending page numbers as LEB128 deltas, the first from zero |

Readers ignore a malformed hint.

See [DESIGN.md](DESIGN.md) for commit, replay, checkpoint and recovery rules.
