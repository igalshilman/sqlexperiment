# Benchmarks

These are standalone Cargo bench targets (`harness = false`), separate from
tests and examples. All use the current (v17) storage format and require the
opt-in `simulation` feature. They count logical KV operations and bytes in
MemoryKv; they do not model a real remote backend.

Against a real Restate server, `make sql-bench` from the repository's root runs
[the end-to-end benchmark](../../../typescript/tests/sql/bench.ts): see
[RESTATE_RESULTS.md](RESTATE_RESULTS.md) (measured on the log format, v15,
before copy-on-write).

Run from `core/sql`:

```sh
cargo bench --locked --features simulation --bench flush_costs
cargo bench --locked --features simulation --bench amplification -- 3 > benches/results/amplification.csv
cargo bench --locked --features simulation --bench small_databases -- 3 > benches/results/small_databases.csv
cargo bench --locked --features simulation --bench cache_cpu -- 3 > benches/results/cache-cpu.csv
```

The positional argument is the repeat count; the default is three. Cargo's
`--bench` argument is filtered by each custom entry point.

## Targets

| Target            | Coverage                                                                                                                                                                                                                                                    | Evidence                                                |
| ----------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------- |
| `flush_costs`     | Bytes, keys and round trips per flush, stored size and cold reads, by workload, up to 1M rows; `CHUNK_PAGES`, `FANOUT`, `REBASE_RATIO`, `INLINE_BYTES`, `GARBAGE_AFTER`, `SWEEP_EVERY` set the options, `WORKLOAD` and `FLUSHES` pick and age the databases | [COW_EXPERIMENT.md](../../state/docs/COW_EXPERIMENT.md) |
| `amplification`   | PK/range/index/scan reads, mutations, rollback, recovery; four database sizes                                                                                                                                                                               | `results/amplification.csv`                             |
| `small_databases` | Tiny databases, index counts, save grouping and a larger control                                                                                                                                                                                            | `results/small_databases.csv`                           |
| `cache_cpu`       | Process-wide memory and disk cache by memory size: misses, round trips, allocations and CPU time                                                                                                                                                            | [CACHE_RESULTS.md](CACHE_RESULTS.md) (log format, v15)  |

All SQL result checks remain active in release builds. Successful writes are
reopened and verified.

## Measurements

`amplification` resets counters between open, SQL/query, save and
recovery-open phases. Each phase has a total row plus blob, root and
other-key rows. Do not add totals to their components. Warm query
observations reuse one session; cold totals include initialization. Unique
keys are per-phase counts and are not summed across phases.

| Metric                          | Numerator                               | Denominator                        |
| ------------------------------- | --------------------------------------- | ---------------------------------- |
| Read amplification              | Get keys + returned encoded values      | Returned logical application bytes |
| Application write amplification | Put keys + encoded values + delete keys | Changed logical application bytes  |
| Storage/database ratio          | Live encoded values                     | Logical database bytes             |

Undefined denominators (misses, no-op writes, deletes) remain blank. Delete
traffic contains key bytes; reclaimed value bytes are a separate measure.
Private WAL/database writes are local I/O, not durable KV traffic.
Failed-operation costs are not inferred from successful-operation counters.

Elapsed timings include simulator scheduling and, for SQL, host-driven engine
execution and parsing costs. Three observations do not establish a latency
distribution. Wire framing, replication, disk writes, backend compression and
compaction are not measured.
