# Benchmarks

These are standalone Cargo bench targets (`harness = false`), separate from tests
and examples. All use the fixed embedded-root radix map and require the opt-in `simulation` feature. They count logical KV
operations and bytes in MemoryKv; they do not model a real remote backend.

Run from the repository root:

```sh
cargo bench --locked --features simulation --bench amplification -- 3 > benches/results/amplification.csv
cargo bench --locked --features simulation --bench page_compression -- 3 > benches/results/page-compression.csv
cargo bench --locked --features simulation --bench small_databases -- 3 > benches/results/small_databases.csv
cargo bench --locked --features simulation --bench log_shipping -- 2 > benches/results/log-shipping.csv
python3 benches/scripts/analyze_amplification.py
python3 benches/scripts/analyze_compression.py
python3 benches/scripts/analyze_log.py
```

The positional argument is the repeat count; the default is three. The paired
compression analyzer expects all three repeats. Cargo's `--bench` argument is
filtered by each custom entry point. The page compression target also writes
`benches/results/page-codec.csv`; an optional second argument overrides that path.

## Targets

| Target | Coverage | Evidence |
|---|---|---|
| `amplification` | PK/range/index/scan reads, mutations, rollback, recovery; four database sizes | [AMPLIFICATION_RESULTS.md](AMPLIFICATION_RESULTS.md) |
| `page_compression` | Paired Raw/LZ4 on repetitive, JSON and random payloads; isolated codec timing | [COMPRESSION_RESULTS.md](COMPRESSION_RESULTS.md) |
| `small_databases` | Tiny databases, index counts, save grouping and a larger control | `results/small_databases.csv` |
| `log_shipping` | Publish-every-save versus the delta log, with and without an object cache, in critical-path round trips | [LOG_RESULTS.md](LOG_RESULTS.md) |

All SQL result checks remain active in release builds. Successful writes are
reopened and verified. The analyzers check repeat agreement, complete workload
coverage and traffic accounting. They understand only the current page format.

## Measurements

`amplification` resets counters between open, SQL/query, checkpoint, save and
recovery-open phases. Each phase has a total row plus block, node, head, intent
and other-key rows. Do not add totals to their components. Warm query observations
reuse one session; cold totals include initialization. Unique keys are per-phase
counts and are not summed across phases.

| Metric | Numerator | Denominator |
|---|---|---|
| Read amplification | Get keys + returned encoded values | Returned logical application bytes |
| Application write amplification | Put keys + encoded values + delete keys | Changed logical application bytes |
| Page-to-KV write amplification | All encoded put values | 4096 × page puts |
| Storage/database ratio | Live encoded values | Logical database bytes |

Undefined denominators (misses, no-op writes, deletes) remain blank. Delete
traffic contains key bytes; reclaimed value bytes are a separate measure. Private
WAL/database writes are local I/O, not durable KV traffic. Failed-operation costs
are not inferred from successful-operation counters.

The compression target includes open in every SQL observation. Repetitions use
independent forks of the same baseline, with no cross-session cache. Raw and LZ4
policies are compared on identical decoded SQL pages. Codec timing samples up to
64 actual pages per dataset, with five warm-up passes and 200 measured passes;
policy order alternates between repeats.

Elapsed timings include simulator scheduling and, for SQL, host-driven engine
execution and parsing costs. Three observations do not establish a latency distribution. Wire framing,
replication, disk writes, backend compression and compaction are not measured.
