# Current storage format

Only this format (v12) is implemented. Use a dedicated namespace. Constants and
key construction live in `src/format.rs`; there is no migration or format
discovery.

| Key | Value |
|---|---|
| `v12/meta` | Checkpoint record; absent until the first checkpoint |
| `v12/log/<16-digit hex sequence>` | Log segment |
| `v12/page/<16-digit hex page number>` | Checkpointed page, overwritten in place |

## Pages

Tag u8 (0 raw, 1 LZ4 block), logical length u16 LE (4096), then payload. Raw payloads
are exactly 4096 bytes. LZ4 payloads must be nonempty and smaller than 4096 bytes;
decoding must produce exactly 4096 bytes into a fixed-size destination. The encoder
uses raw storage when compression does not shrink the payload. Maximum value:
4099 bytes. This validates structure and length, without a payload checksum.

The same envelope is used for page images inside log segments. An absent
`page/<n>` is a zero-filled hole.

## Log segments

A save is one transaction of page images, written to consecutive sequence
numbers. Only its last segment has the commit flag.

| Field | Encoding |
|---|---|
| Magic | `SL12` |
| Sequence number | u64 LE; must equal the key's sequence |
| First sequence of the transaction | u64 LE; at most the sequence number |
| Flags | u8; bit 0 marks the commit segment, other bits must be zero |
| Database byte length | u64 LE, after the transaction |
| Entry count | u32 LE |
| Entries | Strictly ascending: page index u64 LE, image length u16 LE (zero for a hole), page image |

A segment is at most 1 MiB. Every image is decoded when the segment is read.

## Checkpoint record

| Field | Encoding |
|---|---|
| Magic | `SM12` |
| Log start | u64 LE; replay starts here |
| Deleted below | u64 LE; nonzero and at most the log start |
| Database byte length | u64 LE, as of the log start |

Segments before the log start are checkpointed into pages; those before
"deleted below" are deleted too. A missing record means log start 1, deleted
below 1 and an empty database. The value is exactly 28 bytes.

See [DESIGN.md](DESIGN.md) for commit, replay, checkpoint and recovery rules.
