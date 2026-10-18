# Page compression experiment — 2026-10-10

Crate **0.11.0**, storage format **v14**, SQLite-compatible engine `turso_core` **0.8.2**.

Adaptive LZ4 materially reduces the bytes in this experiment's repetitive and
synthetic JSON workloads. It offers little benefit for random row payloads and
adds encoding CPU cost. KV get/put/delete counts are unchanged. LZ4 is the
default; `PageCompression::Raw` remains available for comparison or
workloads where attempting compression is not useful.

## Implementation and boundaries

- `src/page_codec.rs` uses pinned `lz4_flex` **0.14.0**, with safe encoding and
  decoding enabled. Each immutable page has a three-byte envelope: one-byte tag
  (0 raw, 1 LZ4 block), then u16 LE logical size (4096), followed by its payload.
  The codec uses independent LZ4 blocks, without a dictionary or frame wrapper.
- A compressed payload is retained only when smaller than the 4096-byte raw
  payload. Maximum encoded value: **4099 bytes**. Incompressible pages cost three
  extra bytes versus an unwrapped 4096-byte page (0.073%); raw fallback avoids larger compression expansion,
  but still pays for attempting compression under the LZ4 policy.
- Decoding validates encoded bounds, tag, logical size and decoded length. The
  decoder always gets a fixed 4096-byte destination, never an allocation sized
  from stored metadata. Invalid LZ4 back-references and incorrect expansion sizes
  are rejected. Storage is assumed reliable, so there is no checksum.
- The policy applies to checkpointed pages: a checkpoint encodes each page it
  writes to `page/<n>`, and a read decodes it. The log is compressed with zstd
  against the pages' previous images whatever the policy, so commits write the
  same bytes under either. The
  private WAL, dirty overlay and cached decoded page remain ordinary logical
  bytes. Commit and checkpoint ordering are unaffected by the policy.
- `StorageOptions::page_compression` selects new writes only. Raw and compressed
  pages coexist; a different policy on reopen does not require rewriting them.
  Switching policy and performing a read-only save causes no mutations.
- Namespace keys and magic numbers are v14/`SC14`/`SL14`/`SH14`. Start with a
  fresh namespace. Only this format is supported; there are no compatibility probes.

## Paired workload results

`benches/page_compression.rs` creates identical logical SQL pages with
Raw and Lz4 policies and checks their decoded equality. Four database/payload
sizes (100 × 64 B, 100 × 512 B, 100 × 9000 B and 10,000 × 256 B), three payload
families, seven operations and two policies give **168 configurations**,
repeated three times: **504 SQL observations**, or **84 raw/LZ4 workload pairs**.
Each baseline is a bulk load that a later save has checkpointed into page keys.
All traffic counters agree exactly across repeats.

Payloads are synthetic and deterministic: zero-padded numeric text, valid JSON
with varying IDs/versions and a seeded ten-word message vocabulary, and seeded
random bytes. They are stored as BLOBs so the random case has no hex expansion
in the database. JSON is a useful compressible case, not a claim to represent
production event distributions. SQL uses hex literals/results for transport;
logical read/write denominators count decoded payload bytes. There is no owner
cache, and open writes nothing.

Operations: cold PK lookup, ten-row range, full scan, one replacement, ten
adjacent replacements, ten scattered replacements and read-only save. Each
observation includes open; updates include SQL, the private checkpoint and the
save. Correctness checks after measurement reopen with the opposite write
policy, verify the changed payloads and row count, and run `integrity_check`.

**One-row replacement, total mutation key + value bytes (the commit record):**

|   Rows | Payload bytes | Payload family |   Raw | Adaptive LZ4 | Reduction |
| -----: | ------------: | -------------- | ----: | -----------: | --------: |
|    100 |            64 | Repetitive     |   461 |          461 |      0.0% |
|    100 |            64 | JSON           | 1,083 |        1,083 |      0.0% |
|    100 |            64 | Random         | 3,936 |        3,936 |      0.0% |
| 10,000 |           256 | Repetitive     |   323 |          323 |      0.0% |
| 10,000 |           256 | JSON           |   942 |          942 |      0.0% |
| 10,000 |           256 | Random         | 4,065 |        4,065 |      0.0% |

A commit writes its transaction to the record's log, compressed against the
pages' previous images: a page costs about what changed in it, under either
policy. (v12 wrote whole page images to the log, and the policy cut these
bytes 64–94%.) The policy pays off where pages are stored and read: at
checkpoints, and on every cold read.

All 84 pairs have identical get/put/delete counts, page counts and commit record bytes. Encoded reads, mutations and stored values
never increase relative to raw pages. Random row payloads can still leave compressible
SQLite headers and free space, explaining their small improvement. In the codec
sample for 9000-byte random payloads, **58 of 64 pages used raw fallback**.
The unit tests separately exercise fully random 4096-byte pages and verify the
4099-byte fallback bound.

For the 10,000-row JSON cold PK lookup, read key/value traffic fell from
**16,519 to 9,329 bytes (43.5%)**, and stored value bytes from 2,857,016 to
1,118,355 (60.9%). The workload still performs the same dependent KV gets.

## Amplification matrix

The broader SQL amplification matrix covers **104 configurations**, three repeats,
**684 phases / 4104 class-phase rows**. Its analyzer reconciles object classes
and repeat counters. It adds index queries, rollback, insert/delete and interrupted
save recovery. See [AMPLIFICATION_RESULTS.md](AMPLIFICATION_RESULTS.md).
Page-to-KV amplification divides encoded put bytes by **4096 × page puts**.

## Codec time

Release build, Rust **1.97.0**, Linux x86_64, reported CPU **Intel Xeon @ 2.10GHz**.
The harness samples up to 64 evenly spaced actual SQL pages per dataset, with
five warm-up passes and 200 measured passes per repeat. It alternates policy
order, uses `black_box`, includes codec allocations and validates round trips.
There are **72 timed samples** (12 datasets × two policies × three repeats).

Median time per page for the 10,000-row / 256-byte datasets:

| Payload    | Raw encode | LZ4 encode | Raw decode | Adaptive decode |
| ---------- | ---------: | ---------: | ---------: | --------------: |
| Repetitive |    0.09 µs |    1.09 µs |    0.24 µs |         0.40 µs |
| JSON       |    0.09 µs |    5.98 µs |    0.22 µs |         2.01 µs |
| Random     |    0.13 µs |    1.53 µs |    0.26 µs |         0.37 µs |

These are elapsed times in isolated codec loops on this shared host, not remote
request timings or a production speedup. Min/median/max are retained. Raw SQL
elapsed observations include host-driven execution, SQL parsing and payload generation;
their three repeats are not a latency distribution. No p95/p99 claim is made.

## Validation

Tests live under `tests/`. The codec tests cover malformed input, bounded expansion,
incompressible fallback and 2000 seeded round trips. Simulator campaigns cover
mixed policies, cancellation, lost acknowledgments, confirmation, checkpoints
and garbage collection. See [review and validation](../docs/REVIEW.md)
for the current executed checks. All of these are simulator or in-process checks;
no real backend or power-loss claim follows.

## Reproduce

```sh
cargo bench --locked --features simulation --bench page_compression -- 3 > benches/results/page-compression.csv
cargo bench --locked --features simulation --bench amplification -- 3 > benches/results/amplification.csv
python3 benches/scripts/analyze_amplification.py
python3 benches/scripts/analyze_compression.py
```

The paired target also writes `benches/results/page-codec.csv` (optional second
argument selects another path). Raw observations and generated summaries are kept
under `benches/results/`. Benchmark targets are separate from `cargo test`.

Compression reduces bytes but does not reduce calls. Round-trip latency is
measured separately in [LOG_RESULTS.md](LOG_RESULTS.md); validating against a
real host backend remains future work.

Codec references: [lz4_flex 0.14.0](https://docs.rs/lz4_flex/0.14.0/lz4_flex/),
[LZ4 block format](https://github.com/lz4/lz4/blob/dev/doc/lz4_Block_format.md).
