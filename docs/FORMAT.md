# Current storage format

Only this format (v13) is implemented. Use a dedicated namespace. Constants and
key construction live in `src/format.rs`; there is no migration or format
discovery.

| Key | Value |
|---|---|
| `v13/meta` | Checkpoint record; absent until the first checkpoint |
| `v13/log/<16-digit hex sequence>` | Log segment |
| `v13/page/<16-digit hex page number>` | Checkpointed page, overwritten in place |
| `v13/hint` | Working-set hint; advisory, may be absent or stale |

Every value is checksummed with CRC32C (Castagnoli), u32 LE: pages in their
envelope, everything else in a trailing field over all preceding bytes. A
value whose checksum does not match is an error (an ignored one, for the hint).

## Pages

Tag u8 (0 raw, 1 LZ4 block), logical length u16 LE (4096), CRC32C of the
payload u32 LE, then payload. Raw payloads are exactly 4096 bytes. LZ4 payloads
must be nonempty and smaller than 4096 bytes; decoding must produce exactly 4096
bytes into a fixed-size destination. The checksum is verified before the
payload is decoded. The encoder uses raw storage when compression does not
shrink the payload. Maximum value: 4103 bytes.

The same envelope is used for page images inside log segments. An absent
`page/<n>` is a zero-filled hole.

## Log segments

A save is one transaction of page images, written to consecutive sequence
numbers. Only its last segment has the commit flag.

| Field | Encoding |
|---|---|
| Magic | `SL13` |
| Sequence number | u64 LE; must equal the key's sequence |
| First sequence of the transaction | u64 LE; at most the sequence number |
| Flags | u8; bit 0 marks the commit segment, other bits must be zero |
| Database byte length | u64 LE, after the transaction; a multiple of 512, at most 2^44 |
| Entry count | u32 LE |
| Entries | Strictly ascending: page index u64 LE (below 2^32), image length u16 LE (zero for a hole), page image |
| Checksum | CRC32C of all preceding bytes |

A segment is at most 1 MiB. Every image is decoded when the segment is read.

## Working-set hint

| Field | Encoding |
|---|---|
| Magic | `SH13` |
| Entry count | u16 LE; at most 256 |
| Pages | Ascending page numbers as LEB128 deltas, the first from zero |
| Checksum | CRC32C of all preceding bytes |

Readers ignore a malformed hint.

## Checkpoint record

| Field | Encoding |
|---|---|
| Magic | `SM13` |
| Log start | u64 LE; replay starts here; at most 2^62 |
| Deleted below | u64 LE; nonzero and at most the log start |
| Database byte length | u64 LE, as of the log start; a multiple of 512, at most 2^44 |
| Checksum | CRC32C of all preceding bytes |

Segments before the log start are checkpointed into pages; those before
"deleted below" are deleted too. A missing record means log start 1, deleted
below 1 and an empty database. The value is exactly 32 bytes.

See [DESIGN.md](DESIGN.md) for commit, replay, checkpoint and recovery rules.
