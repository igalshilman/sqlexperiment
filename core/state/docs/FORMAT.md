# Storage format

Only this format (v16) is implemented. Use a dedicated namespace. Constants
and keys live in `src/store.rs`; there is no migration or format discovery.
Storage is assumed reliable (a value reads back as written); every zstd frame
carries a checksum, and decoding checks structure and the bounds below.

| Key                          | Value                                                                                             |
| ---------------------------- | ------------------------------------------------------------------------------------------------- |
| `v16/root`                   | The root; absent until the first flush                                                            |
| `v16/blob/<lowercase hex n>` | One zstd frame: a chunk's base or delta, a map node, or a large record or log entry; written once |

The root is the only key whose write commits. Integers are LEB128 varints
unless stated otherwise.

## Root

`SR16`, then one zstd frame of the body, which carries its size (at most
64 MiB) and a checksum:

| Field       | Encoding                                                                                                                                                |
| ----------- | ------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Id          | 8 bytes, little-endian: the database's random identity, drawn when its first root was made; the cache keys blobs by it                                  |
| Version     | Flushes that changed something, ever; at most 2^62                                                                                                      |
| Next blob   | The number the next blob gets; at most 2^62. Every blob named is below it                                                                               |
| Chunk pages | Pages per chunk (1 to 256), set when the namespace was made                                                                                             |
| Fanout      | Entries per map node (2 to 4096), set when the namespace was made                                                                                       |
| Garbage     | Count, then ascending blob numbers, each as the gap from the one before, with the version it died at, as the gap below the root's: blobs to delete      |
| Next file   | The id the next new file gets; at most 2^16; ids are never reused                                                                                       |
| Directory   | Count, then per file: its id (ascending, as the gap after the previous id, less one), its name's length (at most 255) and UTF-8 bytes, its kind (below) |

A file's kind is one of:

- `0`, a length and bytes: a record held inline.
- `1` and a blob number: a record in a blob of its own.
- `2`: a paged file. Its length in 512-byte units (at most 10 GiB), its
  base's id plus one (`0` for none), then its chunk map (below).
- `3`: a log. The index of its first entry (at most 2^62), a count (at most
  4096), then the blob number of each entry.

Names are unique. Nothing may follow. A missing root means version 0 and no
files.

## Chunk maps

A paged file's chunks, as many as its length needs, are named by a radix
tree of `fanout`-entry nodes: its height is the fewest levels that hold
them (`fanout^height ≥ chunks`, at least 1). Level 0 nodes are leaves; the
node at level `k`, index `i` covers chunks `i × fanout^(k+1)` onward. A
node has as many entries as the chunks below the file's length need, at
most `fanout`.

| Node  | Entries                                                                                                           |
| ----- | ----------------------------------------------------------------------------------------------------------------- |
| Leaf  | Per chunk, its base and its delta                                                                                 |
| Inner | Per child, the blob of the node below, or none: every chunk under it a hole (a chunk with neither base nor delta) |

Each entry is `0` for none, or else a blob number as the zigzag gap from
the previous one of its kind in the node (bases, deltas or children; the
first from 0), plus one. A delta without a base is invalid.

The root holds the height, then the top node (level `height − 1`, index 0).
Every other node is a blob: its entries in one zstd frame. A node names
only blobs below its own number, and a stored node is never all holes.

## Blobs

A chunk is `chunk_pages` pages of 4096 bytes, its file's pages
`chunk × chunk_pages` onward; pages past the file's end are zeros.

| Blob          | Frame                                                                                                              |
| ------------- | ------------------------------------------------------------------------------------------------------------------ |
| A chunk base  | The chunk's pages, compressed with its file's base pages as a raw-prefix reference (zeros past them), or with none |
| A chunk delta | The chunk's pages, compressed with the chunk's base (decoded) as a raw-prefix reference                            |
| A map node    | The node's entries, as above                                                                                       |
| A record      | The record's bytes (at most 16 MiB)                                                                                |
| A log entry   | The entry's bytes (at most 16 MiB)                                                                                 |

A chunk decodes to exactly `chunk_pages × 4096` bytes. A chunk with neither
base nor delta is its file's base pages, or zeros. In a file without a base,
a page of zeros reads as a hole.

The files are records, paged files and logs. The SQL database is the paged file
`db`; others are the host's (`State::file`). A file is never deleted, only
emptied, and its id is never reused. A paged file with a base
(`State::file_with_base`) is over a golden image the host holds, named by the
base's id: its chunks decode only with that base.

See [DESIGN.md](DESIGN.md) for the flush, open and garbage rules.
