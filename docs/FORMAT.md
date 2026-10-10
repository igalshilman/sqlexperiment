# Current storage format

Only this format is implemented. Use a dedicated namespace. Constants and key
construction live in `src/format.rs`; there is no migration or format discovery.

| Key | Value |
|---|---|
| `v10/head` | Current snapshot head |
| `v10/pending_save` | Pending publication and reclamation record |
| `v10/node/<16-digit hex ID>` | Immutable map node |
| `v10/block/<16-digit hex ID>` | Immutable encoded 4096-byte page |

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
| Magic | `SK10` |
| Database byte length | u64 LE |
| Last allocated object ID | u64 LE |
| Radix height | u8, at most 8 |
| Root | Encoded radix node occupying the remainder of the value |

The fixed prefix is 21 bytes. The root is always embedded in the head. Height
zero requires an empty root; nonzero height requires a nonempty root. The same
node decoder bounds the root to 2305 bytes, so a head is at most 2326 bytes.
There is no layout selector, root-length field, override list or separate root
object. Compression policy is per-write and does not change the head.

The pending record uses `SP10`. Its exact schema and commit/recovery ordering are
documented in [PROTOCOL.md](PROTOCOL.md).
