# Self-review and cleanup — 2026-10-10

Scope: the complete adapter and the pending page-compression work, with emphasis
on module boundaries, code clarity, test isolation and removal of obsolete paths.
This is an author review supported by executed tests, not an independent audit.

## Findings and changes

| Finding | Change |
|---|---|
| The KV contract module also shipped the in-memory durability model and traffic counters | `src/kv.rs` now contains only the backend trait. MemoryKv, failpoints and counters live under `src/simulation/`, gated by the opt-in `simulation` feature. |
| The normal I/O pump forced a yield for every operation, and updated measurement counters | Forced yields, byte counters, `IoStats` and `Session::io_stats` are compiled only with simulation. A default-feature public API test exercises an immediately-ready backend. |
| Test bodies and test-only helper implementations obscured production files | Every test and helper implementation now lives under `tests/`. Unit module declarations retain `cfg(test)` and private access; simulation-backed suites additionally require the feature. |
| Benchmarks were exposed as examples and analysis/data were scattered at the root | Three explicit Cargo bench targets, their scripts, current results and methodology now live under `benches/`. `examples/` contains only the lookup usage example. |
| Empty stores triggered twelve obsolete key probes | Removed discovery/rejection of older formats, the associated compatibility test, and the obsolete-format fixture. Only the current format remains. |
| Four pending-record length fields allocated a temporary Vec and used an unreachable branch | Decode into a fixed four-element array after checking the input header length. |
| Publication and cleanup eagerly allocated a Vec of boxed futures for every object | Futures are now constructed lazily through a bounded stream. Ordinary failures still drain work before returning, and cleanup still retains the pending record on failure. |
| Format literals and documentation could drift independently | Key/magic constants are centralized in `src/format.rs`; corrected the stale pending-record magic in the protocol description. |
| Historical reports, comparison tooling, logs and deferred designs dominated the project | Removed superseded artifacts and format comparisons. Kept only current source, focused documentation and reproducible current benchmark evidence. |

The map-cost test was reduced to its actual invariant: changed paths reuse
untouched objects and write the expected object set. It no longer prints a
benchmark report. Failure campaign output still reports the exercised cases.

The lazy mutation streams retain boxing at the stream boundary so save/recovery
futures remain `Send` on the pinned toolchain. This removes the eager collection,
not every per-request allocation. Concurrency and ordinary-error draining semantics
are unchanged and covered by the reordered-completion campaigns.

## Resulting boundaries

- Default library: backend contract, engine adapter, page/map codecs and the
  single save/recovery protocol. No simulator, fault backend or traffic counters.
- `simulation` feature: in-memory visibility/durability model, delayed/failing
  requests, metrics and deliberate scheduling yields.
- Test builds: private unit suites and checkpoint injection under `tests/unit/`;
  public API tests under `tests/public_api.rs`; feature-gated storage campaigns
  under `tests/integration/`. No public internals were added just to move tests.
- Benchmark builds: explicit `harness = false` targets requiring `simulation`.
  Benchmarks are not executed by the ordinary test command.

Raw page fallback and all three map layouts remain: they are active parts of the
current experiment, not compatibility implementations. There are no format probes,
older-format decoders, migrations or historical benchmark branches in the code.

## Executed validation

Rust/Cargo 1.90.0, locked dependencies, Linux x86_64.

| Check | Outcome |
|---|---|
| `cargo test --locked --no-default-features` | 7 tests pass: six codec tests plus save/reopen/discard with an immediately-ready backend |
| `cargo test --locked --features simulation` | 48 tests pass: 27 unit, 20 integration, one public API |
| Interruption campaigns | 4107 cases, unchanged coverage |
| `cargo clippy --locked --all-targets --all-features -- -D warnings` | Pass |
| `cargo fmt --check` | Pass |
| `cargo bench --locked --features simulation --bench amplification -- 3` | 12,258 observations / 2043 phases / 309 configurations; repeated counters reconcile |
| `cargo bench --locked --features simulation --bench page_compression -- 3` | 1512 SQL observations, 252 paired workloads and 72 codec timing samples; repeated traffic agrees |
| `cargo bench --locked --features simulation --bench small_databases -- 3` | 1485 observations; SQL/reopen checks pass |
| Both benchmark analyzers | Pass with only current-format input and paths |

One obsolete-format test was removed and one default-build public API test was
added, keeping the full test count at 48. Test-only checkpoint hooks, decoded-page
cache checks, mixed compression policies, folds, cancellation, unknown publication
outcomes and exact orphan accounting remain covered.

## Remaining design limits

No storage correctness defect was reproduced by this review. The existing host
contract remains essential: one owner and settlement of outstanding requests.
Memory budgets, request fencing, real-backend latency and process-crash/power-loss
validation remain future work. The refactor does not claim to address these.

Current benchmark methodology and raw evidence are under
[benches/](../benches/README.md); the format and commit protocol are documented in
[FORMAT.md](FORMAT.md) and [PROTOCOL.md](PROTOCOL.md).
