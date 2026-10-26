# Storage format

Only this format (v21) is implemented. Use a dedicated namespace. Keys,
encodings and bounds live in `src/format.rs`; there is no migration or
format discovery. Storage is assumed reliable (a value reads back as
written); every zstd frame carries a checksum, and decoding checks
structure and the bounds below.

| Store               | Key                                     | Value                                                      |
| ------------------- | --------------------------------------- | ---------------------------------------------------------- |
| Restate             | `v21/head`                              | The head, the commit point; absent until the first flush   |
| Restate             | `v21/tail/<version>/<part>`             | A part of the tail entry of the flush that made `version`  |
| Restate             | `v21/manifest/<version>-<nonce>/<part>` | A part of a checkpoint's manifest; put once, never changed |
| Restate (`KvBlobs`) | `v21/pack/l0/<version>-<nonce>/<part>`  | A part of a pack in L0; put once, never changed            |
| S3 (`BlobStore`)    | `l1/<version>-<nonce>`                  | A pack in L1; put once, never changed                      |

The head is the only key whose write commits. `<version>` and `<part>` in a
tail entry's key are decimal. A pack's `<version>` is the head version
whose state it holds, and `<nonce>` a random number, each 16 lowercase hex
digits: no two puts ever use one name. A checkpoint's manifest is in
Restate wherever its pack is, under the same `<version>-<nonce>`, cut into
parts of 64 KiB (the last shorter), numbered in decimal from 0; a
checkpoint that stores nothing outside its manifest puts no pack. A pack
is in one of two tiers, which its key and every reference to it name. In
L0 (`blobs::KvBlobs`), it is cut into parts of 64 KiB (the last shorter),
numbered in decimal from 0, under `v21/pack/l0/`; in L1, in S3
(`blobs::ObjectStoreBlobs`), it is one object, `l1/<version>-<nonce>`
under the namespace's prefix. Without S3, every pack is in L0.

Integers are LEB128 varints unless stated otherwise. A byte string is its
length, then its bytes. A name is a byte string of at most 255 bytes of
UTF-8. Nothing may follow the last field of a head, a tail entry or a
manifest.

## Head

`SH21`, then, uncompressed:

| Field       | Encoding                                                                                           |
| ----------- | -------------------------------------------------------------------------------------------------- |
| Id          | 8 bytes, little-endian: the database's random identity, drawn when it was made                     |
| Frame pages | Pages per frame (1 to 256), set when the database was made                                         |
| Version     | Flushes that changed something, ever: 1 to 2^62                                                    |
| Checkpoint  | `0` for none, or `1`, the pack's version, nonce, tier (`0` L0, `1` L1), then the manifest's length |
| Previous    | The checkpoint before, which readers that opened on it may still read; as above                    |
| Tail first  | The first version whose tail entry is kept; at least 1                                             |
| Tail bytes  | Bytes of the tail entries since the checkpoint                                                     |
| Parts       | Per version from tail first to the version, the number of parts of its tail entry                  |

The previous checkpoint is older than the checkpoint, or the same version,
and the checkpoint no newer than the version; there is no previous without
a checkpoint. A missing head means version 0, nothing stored, and the frame
size of the options that open it.

## Tail entry

What one flush changed, as it changed it: `SE21`, then, uncompressed (its
pages are zstd frames of their own), cut into parts of at most 1 MiB, put
at `v21/tail/<version>/0` onward. The parts, joined, are at most 1 GiB.

| Field   | Encoding                                                                                                                                                                                                                                                                                                           |
| ------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| Records | Count, then per record set: its name, its value (at most 16 MiB)                                                                                                                                                                                                                                                   |
| Logs    | Count, then per log changed: its name, the index of the first entry appended (at most 2^62), the index below which entries are dropped (at most 2^62), a count, then each entry appended (at most 16 MiB)                                                                                                          |
| Files   | Count, then per paged file changed: its name, its size after in 512-byte units (at most 10 GiB), its base's id plus one (`0` for none), a count, then per page written, ascending: its number as the gap from the page after the one before (the first from 0), and a byte string: empty for a hole, else the page |

An entry appended and dropped by the same flush is not stored. A page is a
zstd frame of its 4096 bytes, compressed with the page _before_ it as a raw
prefix: the page as the entries before it in the tail left it, from the
page as the head's checkpoint holds it (its patch, else its frame's page);
where the checkpoint holds none, or the tail made it a hole (a hole
written, or a shrink past it), its file's base's page, else zeros. Entries
decode in order. A page's number is below 2,621,440 (10 GiB).

## Pack

What one checkpoint wrote, back to back: the frames it rebuilt, the
patches, the values, the ranges it copied from older packs. Its manifest,
which says where they are, is in Restate.

| Range  | A zstd frame of                                                                                                                                               |
| ------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Frame  | `frame_pages` pages of a file (its pages `index × frame_pages` onward), compressed with its file's base pages (zeros past them) as a raw prefix, or with none |
| Patch  | One page of a file, compressed with the page as its frame holds it as a raw prefix                                                                            |
| Value  | A record's value, or a log entry, past `inline_record_bytes`; at most 16 MiB                                                                                  |
| Copied | A range of an older pack, moved as it was (compaction): any of the above                                                                                      |

A frame decodes to exactly `frame_pages × 4096` bytes. A frame that would
hold only its base's pages, or zeros, is not stored. A page of zeros where the file has
no base page reads as a hole.

## Manifest

`SM21`, then one zstd frame of the body (at most 1 GiB decoded):

| Field   | Encoding                                                                                                                                                                                                                                                                                                                                              |
| ------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Packs   | Count, then per pack the manifest names, its own included if it put one, ascending: its version, its nonce, its tier (`0` L0, `1` L1), and the bytes it stored (for compaction)                                                                                                                                                                       |
| Records | Count, then per record: its name, its value (below)                                                                                                                                                                                                                                                                                                   |
| Logs    | Count, then per log: its name, the index of its first entry (at most 2^62), a count (at most 4096), then each entry, a value                                                                                                                                                                                                                          |
| Files   | Count, then per paged file: its name, its size in 512-byte units (at most 10 GiB), its base's id plus one (`0` for none), a count, then a location per frame (`0` for none: its base's pages, or zeros); then a count of patches, and per patch, ascending: its page as the gap from the page after the one before (the first from 0), and a location |

A value is `0` and a byte string (inline, up to `inline_record_bytes`), or
`1` and a location. A location is its pack's index in the packs above plus
one (`0` is none), its offset as the zigzagged difference from the end of
the location before it in the manifest (the first from 0), and its length.

A file has as many frames as its size needs (`⌈size / (frame_pages ×
4096)⌉`), and patches only below its size, each in a stored frame. A page
with a patch reads from it; any other from its frame. Names are unique
across records, logs and files, at most 65,536 of them. A missing
checkpoint means no files, records or logs.

The files are records, paged files and logs. The SQL database is the paged
file `db`; others are the host's (`State::file`). Nothing is deleted: a
record is set, a file cut, a log truncated. A paged file with a base
(`State::file_with_base`) is over a golden image the host holds, named by
the base's id: its pages and frames decode only with that base.

See [ARCHITECTURE.md](ARCHITECTURE.md) for the flush, checkpoint, open and
garbage rules.
