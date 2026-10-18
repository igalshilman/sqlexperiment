# Current storage format

Only this format (v14) is implemented. Use a dedicated namespace. Constants and
key construction live in `src/format.rs`; there is no migration or format
discovery. Storage is assumed reliable (a value reads back as written), so
pages have no checksums; decoding checks structure and the bounds below. A log
segment's zstd frame carries one.

| Key                                   | Value                                                                   |
| ------------------------------------- | ----------------------------------------------------------------------- |
| `v14/commit`                          | Commit record, with the log; absent until the first                     |
| `v14/log/<16-digit hex number>`       | Stored log value: segments, sealed or of a large transaction; immutable |
| `v14/page/<16-digit hex page number>` | Checkpointed page, overwritten in place                                 |
| `v14/hint`                            | Working-set hint; advisory, may be absent or stale                      |

The commit record is the only key whose write commits. Integers are LEB128
varints unless stated otherwise.

## Commit record

| Field             | Encoding                                                                                                                   |
| ----------------- | -------------------------------------------------------------------------------------------------------------------------- |
| Magic             | `SC14`                                                                                                                     |
| Version           | Commits ever made; at most 2^62                                                                                            |
| Epoch             | Checkpoints ever recorded; at most 2^62                                                                                    |
| Database length   | In 512-byte units, after the log; at most 2^44 bytes                                                                       |
| Next segment      | The number the next stored value gets; at most 2^62                                                                        |
| Garbage           | Count, then ascending numbers, each from the one before: stored values the record does not name, to delete; below the next |
| Effects           | Length (at most 16 MiB), then the host's bytes, opaque here                                                                |
| Transaction count | At most the version, and 2^20                                                                                              |
| Transactions      | Each: its database length in 512-byte units, its part count, then its parts                                                |
| Chain count       | At most 2^20                                                                                                               |
| Chain             | The host's values, each a part                                                                                             |

A part is `0`, a length and inline bytes (a segment of at most 1 MiB, or a
chain value of at most 16 MiB), or `1`, the number of a stored value (below the
next segment, not in the garbage) and the bytes' index in it. The last transaction's length is the database length. Nothing may follow. A
missing record means version 0, epoch 0 and an empty database.

The transactions are the log since the last checkpoint, in commit order.
Pages they change are authoritative from the log; any other page is read at
`page/<n>`. A commit without SQL changes adds none.

The chain is the host's (`Session::commit_turn`): opaque values in the order
it appended them since it last restarted the chain. Checkpoints keep it. A
stored value may hold log segments and chain values together (a seal).

## Log segments

A transaction's changed pages, in segments of at most 240 entries. A stored
value at `log/<n>` is `SL14`, the segment count, then each segment's length
and bytes: one segment of a large transaction, or the segments a commit
sealed (the record's inline log, in order).

| Field       | Encoding                                                                                                                                               |
| ----------- | ------------------------------------------------------------------------------------------------------------------------------------------------------ |
| Entry count | At most 240                                                                                                                                            |
| Entries     | Ascending pages (below 2^32): the gap after the previous entry's page (for the first, the page) shifted left once, plus 1 for an image or 0 for a hole |
| Images      | One zstd frame with a checksum, of the entries' images (4096 bytes each) in order; absent when all are holes                                           |

The frame's dictionary is a raw prefix: the previous image of each entry with
an image, in entry order, where it has one. A page's previous image is its
latest in the log before this segment (since the last checkpoint): replay
rebuilds them from an empty chain. A segment is at most 1 MiB.

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
| Magic       | `SH14`                                                       |
| Entry count | u16 LE; at most 256                                          |
| Pages       | Ascending page numbers as LEB128 deltas, the first from zero |

Readers ignore a malformed hint.

See [DESIGN.md](DESIGN.md) for commit, replay, checkpoint and recovery rules.
