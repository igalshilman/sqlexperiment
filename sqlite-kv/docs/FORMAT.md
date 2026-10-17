# Current storage format

Only this format (v13) is implemented. Use a dedicated namespace. Constants and
key construction live in `src/format.rs`; there is no migration or format
discovery. Storage is assumed reliable (a value reads back as written), so
pages have no checksums; decoding checks structure and the bounds below. A log
segment's zstd frame carries one.

| Key                                   | Value                                                |
| ------------------------------------- | ---------------------------------------------------- |
| `v13/meta`                            | Checkpoint record; absent until the first checkpoint |
| `v13/log/<16-digit hex sequence>`     | Log segment                                          |
| `v13/page/<16-digit hex page number>` | Checkpointed page, overwritten in place              |
| `v13/hint`                            | Working-set hint; advisory, may be absent or stale   |

## Pages

Tag u8 (0 raw, 1 LZ4 block), logical length u16 LE (4096), then payload. Raw payloads
are exactly 4096 bytes. LZ4 payloads must be nonempty and smaller than 4096 bytes;
decoding must produce exactly 4096 bytes into a fixed-size destination. The encoder
uses raw storage when compression does not shrink the payload. Maximum value:
4099 bytes.

An absent `page/<n>` is a zero-filled hole.

## Log segments

A save is one transaction of page images, written to consecutive sequence
numbers. Only its last segment has the commit flag. Integers after the flags
are LEB128 varints.

| Field                                    | Encoding                                                                                                                                               |
| ---------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------ |
| Magic                                    | `SL13`                                                                                                                                                 |
| Flags                                    | u8; bit 0 marks the commit segment, other bits must be zero                                                                                            |
| Sequence number                          | Must equal the key's sequence                                                                                                                          |
| Back to the transaction's first sequence | At most the sequence number                                                                                                                            |
| Back to the chain base                   | At most the first sequence                                                                                                                             |
| Database length                          | In 512-byte units, after the transaction; at most 2^44 bytes                                                                                           |
| Entry count                              | At most 240                                                                                                                                            |
| Entries                                  | Ascending pages (below 2^32): the gap after the previous entry's page (for the first, the page) shifted left once, plus 1 for an image or 0 for a hole |
| Images                                   | One zstd frame with a checksum, of the entries' images (4096 bytes each) in order; absent when all are holes                                           |

The frame's dictionary is a raw prefix: the previous image of each entry with
an image, in entry order, where it has one. A page's previous image is its
latest in the committed transactions from the chain base up to this
segment's transaction. Replay keeps those images; a segment whose base is
later than the replay's chain so far restarts it, and an earlier one is
invalid. A segment is at most 1 MiB.

## Working-set hint

| Field       | Encoding                                                     |
| ----------- | ------------------------------------------------------------ |
| Magic       | `SH13`                                                       |
| Entry count | u16 LE; at most 256                                          |
| Pages       | Ascending page numbers as LEB128 deltas, the first from zero |

Readers ignore a malformed hint.

## Checkpoint record

| Field                | Encoding                                                     |
| -------------------- | ------------------------------------------------------------ |
| Magic                | `SM13`                                                       |
| Log start            | u64 LE; replay starts here; at most 2^62                     |
| Deleted below        | u64 LE; nonzero and at most the log start                    |
| Database byte length | u64 LE, as of the log start; a multiple of 512, at most 2^44 |

Segments before the log start are checkpointed into pages; those before
"deleted below" are deleted too. A missing record means log start 1, deleted
below 1 and an empty database. The value is exactly 28 bytes.

See [DESIGN.md](DESIGN.md) for commit, replay, checkpoint and recovery rules.
