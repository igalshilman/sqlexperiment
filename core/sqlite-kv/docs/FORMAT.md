# Current storage format

Only this format (v14) is implemented. Use a dedicated namespace. Constants and
key construction live in `src/format.rs`; there is no migration or format
discovery. Storage is assumed reliable (a value reads back as written), so
pages have no checksums; decoding checks structure and the bounds below. A log
segment's zstd frame carries one.

| Key                                   | Value                                                 |
| ------------------------------------- | ----------------------------------------------------- |
| `v14/commit`                          | Commit record, with the log; absent until the first   |
| `v14/log/<16-digit hex number>`       | Stored log segment of a large transaction; immutable  |
| `v14/page/<16-digit hex page number>` | Checkpointed page, overwritten in place               |
| `v14/hint`                            | Working-set hint; advisory, may be absent or stale    |

The commit record is the only key whose write commits. Integers are LEB128
varints unless stated otherwise.

## Commit record

| Field             | Encoding                                                                         |
| ----------------- | -------------------------------------------------------------------------------- |
| Magic             | `SC14`                                                                           |
| Version           | Transactions ever committed; at most 2^62                                        |
| Epoch             | Checkpoints ever recorded; at most 2^62                                          |
| Database length   | In 512-byte units, after the log; at most 2^44 bytes                             |
| Next segment      | The number the next stored segment gets; at most 2^62                            |
| Garbage           | Start and end: stored segments no transaction names, to delete; start ≤ end ≤ next |
| Effects           | Length (at most 1 MiB), then the host's bytes, opaque here                       |
| Transaction count | At most the version, and 2^20                                                    |
| Transactions      | Each: its database length in 512-byte units, its part count, then its parts      |

A part is `0`, a length and an inline segment (at most 1 MiB), or `1` and the
number of a stored segment (below the next segment, not in the garbage). The
last transaction's length is the database length. Nothing may follow. A
missing record means version 0, epoch 0 and an empty database.

The transactions are the log since the last checkpoint, in commit order.
Pages they change are authoritative from the log; any other page is read at
`page/<n>`.

## Log segments

A transaction's changed pages, in segments of at most 240 entries.

| Field       | Encoding                                                                                                                                               |
| ----------- | ------------------------------------------------------------------------------------------------------------------------------------------------------ |
| Entry count | At most 240                                                                                                                                            |
| Entries     | Ascending pages (below 2^32): the gap after the previous entry's page (for the first, the page) shifted left once, plus 1 for an image or 0 for a hole |
| Images      | One zstd frame with a checksum, of the entries' images (4096 bytes each) in order; absent when all are holes                                           |

The frame's dictionary is a raw prefix: the previous image of each entry with
an image, in entry order, where it has one. A page's previous image is its
latest in the log before this segment (since the last checkpoint): replay
rebuilds them from an empty chain. A segment is at most 1 MiB. Inline, it is a
part of the record; stored, its value at `log/<n>` is `SL14` and the segment.

## Pages

Tag u8 (0 raw, 1 LZ4 block), logical length u16 LE (4096), then payload. Raw payloads
are exactly 4096 bytes. LZ4 payloads must be nonempty and smaller than 4096 bytes;
decoding must produce exactly 4096 bytes into a fixed-size destination. The encoder
uses raw storage when compression does not shrink the payload. Maximum value:
4099 bytes.

An absent `page/<n>` is a zero-filled hole.

## Working-set hint

| Field       | Encoding                                                     |
| ----------- | ------------------------------------------------------------ |
| Magic       | `SH14`                                                       |
| Entry count | u16 LE; at most 256                                          |
| Pages       | Ascending page numbers as LEB128 deltas, the first from zero |

Readers ignore a malformed hint.

See [DESIGN.md](DESIGN.md) for commit, replay, checkpoint and recovery rules.
