# Deferred map, compression and cache experiments

Status: design notes only, deliberately not implemented. SQL pages and K/V blocks
remain 4096 bytes. The next measurement work uses the existing three layouts.

## Measured priorities from 0.9.0

`AMPLIFICATION_RESULTS.md` adds evidence without implementing these proposals:

- Every measured cold open fetched one block five times. A byte-bounded cache
  scoped to one mounted snapshot could first target four redundant reads and
  16 KiB per open, without requiring cross-session reuse.
- In the 10,000-row Radix case, a one-page update wrote 2,331 map bytes on top of
  the 4,096-byte page. Total put values were 1.61 times page bytes. Compact node
  encoding has a measurable target here; the equivalent 100-row factor was 1.05.
- Ten adjacent replacements wrote one page; ten scattered replacements wrote ten
  in the large case. Locality and save grouping need to remain separate variables
  when comparing a codec or a cache.
- A scattered 10%-selectivity indexed payload lookup read 674 pages after open,
  versus four for the covering-index query. Cache admission should be tested with
  scans and row-fetching index workloads, not just repeated point reads.

## Compact mappings

The map associates a logical block number with a u64 immutable object ID. A set
representation alone cannot replace that association: a presence bitmap needs an
accompanying array of object IDs.

- Dense consecutive positions: omit positions and store an ID array.
- Sparse positions: sorted positions or gaps plus an ID array.
- A partially populated 256-way radix node: a 32-byte bitmap plus packed IDs;
  rank/popcount maps an edge to its value position. For very sparse nodes, the
  existing one-byte labels can be smaller.
- A full node: positions are implicit; no bitmap is needed.
- ID codecs: unsigned varints, frame-of-reference offsets, or bit packing with
  a fixed-width fallback. Keep u64 allocation semantics; compression changes
  serialization, not the ID space.

IDs are sequential in allocation order, not necessarily in logical page order.
An update can place a new large ID between old IDs. A codec must not assume
positive adjacent-ID deltas or permanently small IDs. Outliers can make a single
base/range encoding ineffective; smaller groups add headers and complexity.

Illustration, excluding headers: 32 consecutive pages use 512 bytes in the current
head override pairs, 256 bytes as implicit positions with u64 values, or at most
64 bytes for IDs below 16384 encoded as unsigned base-128 varints. These are
encoding arithmetic, not measured workload compression ratios.

Consider a complete compressed map in the head for small databases and external
nodes plus bounded overrides for larger ones. Bound inline data by encoded bytes
and decoded entry count. Preserve explicit tombstones where absence means fallback
to a base map. Keep lazy traversal; avoid a database-sized monolithic map. Any new
codec needs versioning, validation, malformed-input tests and crash/reopen coverage.

Roaring is more directly applicable to large dirty-page or retirement-ID sets.
For a 256-position node, a plain bitmap needs little machinery. Decoded map nodes
could use compact arrays instead of a BTreeMap, independently of their wire codec.

## Cache reuse across sessions

The current engine page cache and adapter node cache belong to one session.
Warm lookup tests reuse that session; they do not demonstrate cross-session reuse.

A proposed shared clean cache sits beneath Turso and above K/V, with a process-wide
byte budget. Cache immutable blocks and decoded map nodes; use shared buffers and
account for decoded metadata and ownership overhead. Dirty overlays remain private.
Admit newly written objects after head publication succeeds. Read the authoritative
head at each reopen initially; retaining mutable heads needs stronger ownership
rules than retaining immutable payloads.

ID reuse is the central correctness constraint. Failed saves may leave ID 123 with
bytes A, then a later save can overwrite that uncommitted ID with bytes B. A cache
key consisting only of namespace/kind/ID could return A. A conservative design adds
an ownership-scoped cache generation, invalidates it after uncertain saves, recovery,
namespace reset or ownership change, and prevents late completions from inserting
into the replacement generation. No extra durable allocator reservation is proposed.

Start with byte-bounded LRU; compare segmented LRU or TinyLFU admission when a large
scan competes with many hot tiny databases. Recovery deletes should invalidate relevant cache entries. Cache entries are hints,
not live roots; old snapshot handles must not survive save-time retirement. Engine initialization and copying into engine-owned
buffers remain costs even when backing reads hit this cache.

## Page compression

Evaluate per-object compression before transmission to the durable host, keeping
logical blocks at 4 KiB. Measure CPU, encoded bytes, and decode latency. Store a
codec tag and original-size validation; retain raw encoding when compression loses.
Backend compression after replication does not necessarily reduce replicated bytes.

## Evidence required before adoption

Measure map bytes, metadata puts/gets, CPU decode cost, cold versus cross-session
warm reads, cache bytes/hit rates, and scan pollution on actual small schemas and
a large-database control. None of these proposals is currently selected as a default.

References:
- https://github.com/RoaringBitmap/RoaringFormatSpec
- https://github.com/fast-pack/FrameOfReference
- https://arxiv.org/abs/1512.00727
