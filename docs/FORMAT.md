# Current storage format

Only this format is implemented. Use a dedicated namespace. Constants and key
construction live in `src/format.rs`; there is no migration or format discovery.

| Key | Value |
|---|---|
| `v11/head` | Current snapshot head |
| `v11/pending_save` | Pending publication and reclamation record |
| `v11/node/<16-digit hex ID>` | Immutable map node |
| `v11/block/<16-digit hex ID>` | Immutable encoded 4096-byte page |
| `v11/log/<16-digit hex sequence>` | Delta-log segment |

Object IDs are incrementing u64 values shared by pages and nodes, not hashes.
Published IDs are never reused. IDs above the recovered committed counter can be
reused only after aborted-attempt cleanup. Overflow fails before any mutation.

## Pages

Tag u8 (0 raw, 1 LZ4 block), logical length u16 LE (4096), then payload. Raw payloads
are exactly 4096 bytes. LZ4 payloads must be nonempty and smaller than 4096 bytes;
decoding must produce exactly 4096 bytes into a fixed-size destination. The encoder
uses raw storage when compression does not shrink the payload. Maximum value:
4099 bytes. This validates structure and length, without a payload checksum.

## Map nodes

The map associates logical page numbers with immutable object IDs. A node has up
to 256 edges. Its encoder chooses the smaller representation:

- Tag 0: sorted (edge u8, ID u64 LE) entries.
- Tag 1: count u16 LE; sorted edge bytes for counts up to 32, otherwise a 32-byte
  bitmap; minimum ID as unsigned LEB128; then LEB128 offsets from that minimum in
  edge order.

External nodes must be nonempty. Empty inline roots have zero bytes. Decoding
validates lengths, edge order, bitmap counts, minimal varints, nonzero IDs and
arithmetic bounds. Every child ID must precede its parent node's ID; inline roots
are bounded by the head's allocation counter. Full u64 IDs are supported. The
maximum encoded node is 2305 bytes.

## Head

| Field | Encoding |
|---|---|
| Magic | `SK11` |
| Database byte length | u64 LE |
| Last allocated object ID | u64 LE |
| Radix height | u8, at most 8 |
| First delta-log sequence | u64 LE; zero when the log is disabled |
| Root | Encoded radix node occupying the remainder of the value |

The fixed prefix is 29 bytes. The root is always embedded in the head. Height
zero requires an empty root; nonzero height requires a nonempty root. The same
node decoder bounds the root to 2305 bytes, so a head is at most 2334 bytes.
There is no layout selector, root-length field, override list or separate root
object. Compression policy is per-write and does not change the head.

The pending record uses `SP11`. Its exact schema and commit/recovery ordering are
documented in [PROTOCOL.md](PROTOCOL.md). Its object entries use kind 2 for
retired log segments, whose "ID" is the sequence number.

## Delta-log segments

A segment holds the pages changed by one save, after the private checkpoint:

| Field | Encoding |
|---|---|
| Magic | `SL11` |
| Sequence number | u64 LE; must equal the key's sequence |
| Database byte length | u64 LE, after this save |
| Entry count | u32 LE |
| Entries | Strictly ascending: page index u64 LE, encoded length u16 LE (zero for a hole), page value |

Page values use the page envelope above. A segment is at most 1 MiB; a larger
save publishes instead. Segments are replayed from the head's first sequence
until the first absent key. See [PROTOCOL.md](PROTOCOL.md#delta-log).
