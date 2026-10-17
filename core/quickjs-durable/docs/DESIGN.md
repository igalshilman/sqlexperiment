# QuickJS/Wasm durable execution

This is the implementation specification for a single-writer durable JavaScript
runtime intended for embedding in Restate. It defines the guest bridge, snapshot
ABI, record formats, execution algorithms, host transactions and recovery rules.
It is self-contained: source links and references are supporting material, not
substitutes for protocol steps. Last consolidated: 17 October 2026.

The implementation in `rust/` uses QuickJS compiled to WebAssembly, Wasmtime,
4 KiB page tracking, and XOR/Zstd checkpoint payloads relative to a shared golden
image. The SQLite host implements the transaction contract. The atomic-KV WAL
specified below is **not implemented yet**, and there is no live Rust integration
with Restate. The root TypeScript code is a historical bsdiff/journal experiment;
it does not define this design.

Reading map: [ownership](#1-model-ownership-and-durability),
[interfaces](#2-guest-and-host-interfaces), [VM ABI](#3-quickjswasm-snapshot-abi),
[write tracking](#4-discovering-writes-and-choosing-a-diff),
[persistent format](#5-persistent-format-capture-and-reconstruction),
[engine algorithms](#6-engine-algorithms-and-checkpoint-boundaries),
[SQLite](#7-sqlite-validation-host), [KV WAL](#8-atomic-kv-host-proposed-redo-wal),
[examples](#9-worked-execution-and-failure-examples),
[acceptance](#10-restate-binding-and-implementation-acceptance),
[measurements](#11-measurements-and-reproduction),
[limits and provenance](#12-scope-limits-provenance-and-references).

## 1. Model, ownership and durability

An **application version** names immutable source and an exact runtime build.
Its **golden image** is initialized before any invocation input is bound. An
**invocation** has a globally unique run ID and a host-selected application KV
scope. A **turn** starts or resumes an invocation and ends at one checkpoint.
An **effect** is a registered request whose result can be delivered in a later
turn. A **checkpoint** is the current VM image plus its scheduling metadata.

The host owns scheduling, transaction commit/abort, initial invocation delivery,
effect dispatch and recovery. The engine owns execution and snapshot creation.
It returns a provisional view; returning that view is not a commit.

Assumptions and invariants:

1. KV is reliable. A successful atomic put durably replaces one complete value;
   recovery reads observe settled writes. No snapshot/blob/manifest integrity
   hashes are required. Namespace and source/build identity hashes serve different
   purposes and do not validate snapshot contents.
2. One writer owns the entire conflicting domain: checkpoint records, application
   KV, effect/result records and discovery indexes. No old owner may finish a
   delayed write after a replacement owner starts. Per-run ownership alone is
   insufficient when runs share application state or a WAL slot.
3. A transaction provides read-your-own-writes and isolation from conflicting
   commits. Each turn publishes the following changes atomically:

   | Component                   | Changes                                                       |
   | --------------------------- | ------------------------------------------------------------- |
   | VM                          | Changed-page payloads, full page index, manifest and head     |
   | Application state           | All guest KV mutations and deletions                          |
   | New effects                 | RPC/custom requests, timers, internal resume wakeups          |
   | Consumed result             | Removal/acknowledgement of the completion delivered to the VM |
   | Host metadata, when changed | Run/scope binding, input identity and work-discovery indexes  |

4. Effects dispatch, wakeups become runnable, and terminal replies publish only
   after the transaction is committed and storage recovery/application is complete.
5. Before a durable commit decision, an execution error or cancellation aborts
   all staged work. After that decision, failure requires completing the stored
   decision. An ambiguous acknowledgement must be resolved before further work.
6. Snapshot only after returning completely from Wasm. The image contains QuickJS
   promises and queued jobs, never a native Wasm stack or a Rust future.
7. Recovery reconstructs the saved image. It does not replay JavaScript, issue
   historical KV reads, or repeat callbacks to regenerate a checkpoint.

A transaction covers a turn, not the entire invocation. Other invocations may
update the same scope while this invocation waits for an RPC. Captured local
variables retain their old values; a new KV read observes the new transaction.
Exclusive object ownership across the entire invocation is an additional host
scheduling policy.

## 2. Guest and host interfaces

### 2.1. JavaScript API and wire values

Application source defines `main(input, ctx)`. `ctx` and global `durable` refer to
one frozen API. Its nested KV and RPC objects are frozen too.

| Call                                 | Request fields, in addition to `id` | Result                          |
| ------------------------------------ | ----------------------------------- | ------------------------------- |
| `ctx.kv.get(key)`                    | `kind:"kv.get", key`                | JSON value, or null when absent |
| `ctx.kv.set(key, value)`             | `kind:"kv.set", key, value`         | null after staging              |
| `ctx.kv.delete(key)`                 | `kind:"kv.delete", key`             | null after staging              |
| `ctx.rpc.call(target, input = null)` | `kind:"rpc", target, input`         | JSON response                   |
| `ctx.call(name, args = null)`        | `kind:"call", name, args`           | Custom host response            |
| `ctx.sleep(ms)`                      | `kind:"sleep", deadline: now + ms`  | null at/after deadline          |

`target` is `{service: string, handler: string, key?: string}`. Service and handler
are nonempty; unknown fields are rejected. The optional key selects the remote
object/workflow, not the caller's KV scope. KV keys are strings; custom names are
nonempty strings. There is **no public JavaScript checkpoint method**.

Host-facing values are acyclic JSON: null, booleans, finite numbers, strings,
arrays and plain objects. Reject undefined, functions, BigInt, cycles and other
object prototypes. Validate both the original value and the parsed result of
JSON serialization: getters can return different values on successive accesses.
Strings and object keys crossing into Rust must contain Unicode scalar values;
lone UTF-16 surrogates produce a catchable TypeError before issuing a request.
Arbitrary objects, cycles and UTF-16 code units may remain inside the VM heap.
Guest error diagnostics replace lone surrogates with U+FFFD.
JSON numbers have JavaScript's numeric semantics; do not rely on lossless integer
transport beyond its safe-integer range. A missing KV key and a stored JSON null
are intentionally indistinguishable through `kv.get` in this interface.

The outcome encoding is exactly one of:

```json
{ "ok": true, "value": 42 }
```

```json
{ "ok": false, "error": { "name": "NotFound", "message": "Missing item" } }
```

Validate the discriminator: a success must have `ok:true`, a failure `ok:false`.
Root return `undefined` becomes successful null. A root value outside the JSON
contract becomes a failed outcome. A failed operation rejects its promise with
an Error carrying the supplied name/message and may be caught by the application.
A terminal guest failure may commit prior KV mutations. Infrastructure errors,
VM traps and cancelled turns instead abort the transaction.

### 2.2. Snapshot-resident promise bridge

Keep these values inside the QuickJS heap:

- `runId`, initially unset; `nextOperation`, initially zero; logical `now`.
- PRNG state and host-supplied maximum time.
- An insertion-ordered map `pending[id] = {operation, resolve, reject}`.
- An optional `rootOutcome`.

Request creation validates/copies arguments before assigning an ID, then:

```text
request(fields):
    require runId is initialized
    id = runId + "/" + decimal(nextOperation)
    nextOperation += 1
    promise = new Promise((resolve, reject) =>
        pending.set(id, {operation: {id, ...fields}, resolve, reject}))
    attach a no-op rejection handler to promise
    return promise
```

The handler avoids unhandled rejections from losing races; the original promise
still rejects for application consumers. Creating a request performs no host I/O.
The operation counter covers local KV requests and internal wakeups as well as
external effects. IDs are stable across restore. Uncommitted IDs may be reused
because uncommitted requests must never have dispatched.

The internal bridge exposes five host hooks, not application API methods:

| Hook                 | Behavior                                                                                                             |
| -------------------- | -------------------------------------------------------------------------------------------------------------------- |
| `init(configJson)`   | Once only: set run ID, time, PRNG seed and time limit                                                                |
| `start(inputJson)`   | Queue `main(input, durable)` through `Promise.resolve().then(...)`; capture its normalized root outcome              |
| `deliver(eventJson)` | Advance logical time monotonically; find and remove the pending entry; resolve/reject its promise; do not drain jobs |
| `pause()`            | Create an internal `{kind:"resume"}` request through the same ID/map mechanism                                       |
| `inspect()`          | Return JSON state with descriptors only, excluding resolvers                                                         |

`inspect()` returns:

```text
State = {
    pending: Operation[],       // insertion/issue order
    nextOperation: unsigned integer,
    now: milliseconds,
    status: "waiting" | "completed" | "failed",
    outcome?: Outcome
}
```

After draining jobs, a root still unsettled with no pending request is a failed
"Execution stalled: no durable wakeup" outcome. With any pending request, status
is waiting even if the root has settled. Otherwise status follows root success
or failure. Thus losing `Promise.race` branches and ignored promises must drain
before terminal publication. No cancellation or detached-effect lifetime exists.
When pausing with queued jobs, create the resume request **before** inspecting
state so the queued continuation is not misclassified as stalled.

### 2.3. Rust transaction contract

The following signatures describe the implemented API; `Json` denotes a JSON value:

```rust,ignore
trait Kv {
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>>;
    async fn put(&mut self, key: &str, value: &[u8]) -> Result<()>;
}

enum Resolution { Ready(Outcome), Pending }

trait Transaction: Kv {
    fn check_run(&self, run: &str) -> Result<()>; // default allows unscoped hosts
    async fn execute(&mut self, op: &Operation) -> Result<Resolution>;
    async fn acknowledge(&mut self, id: &str, outcome: &Outcome, now: u64)
        -> Result<()>;
}

Engine::new(transaction, shared_runtime)
Engine::with_checkpoint_interval(NonZeroUsize) // optional host request budget
Engine::register(application_id, source).await -> Result<()>
Engine::start(run, application_id, input, now).await -> Result<View>
Engine::complete(run, operation_id, outcome, now).await -> Result<View>
Engine::recover(run).await -> Result<View>
Engine::into_storage() -> Result<Transaction>
```

`execute` runs outside Wasm, serially in issue order. KV requests must resolve as
Ready, possibly after awaited transactional I/O. Pure custom functions and local
transactional RPCs may also return Ready. Pending means the host has staged a
recoverable effect under that exact ID. Sleep and Resume must return Pending;
Resume is a local wakeup already carrying successful null once commit finishes.
Never perform an irreversible external RPC inside this transaction.

`acknowledge` verifies that the selected outcome is the recoverable result, and
that effective delivery time is no earlier than result availability, before
staging consumption. It must not durably remove the result independently of the
next VM checkpoint. A host with an external durable inbox may implement that
contract without a separate result table. The memory test adapter assumes its
embedding supplies that result; SQLite verifies its stored result directly.

Set an engine usability flag false before registration/turn execution and restore
it only after success. If a polled future is dropped, an error occurs, or an
unwind interrupts execution, leave it false. Reject reuse, recovery and storage
extraction from that engine. Failed extraction consumes/drops the transaction;
SQLite rolls back on drop. A fresh attempt starts from committed state. This is
an API guard, not permission for adapters to publish staged data independently.

Commit is explicitly host-owned:

```rust,ignore
let mut engine = Engine::new(host.begin(run, scope)?, runtime);
let provisional = engine.complete(run, id, outcome, now).await?;
let transaction = engine.into_storage()?;
host.commit(transaction)?; // resolve an ambiguous outcome through storage recovery
publish(provisional);     // only after confirmed commit/application
```

Every host call is awaitable: checkpoint `get/put`, guest operation execution and
acknowledgement. The engine awaits them outside any Wasm call, so a remote store
may suspend in each. No worker thread or serialized Rust future is part of this
protocol.

## 3. QuickJS/Wasm snapshot ABI

### 3.1. Pinned runtime and supported state

The reference build uses Wasmtime **49.0.3** and the extension-free WASI reactor
from npm **quickjs-wasi@3.6.2**, copied unchanged to `rust/assets/quickjs.wasm`.
WASI is the WebAssembly System Interface: imported host functions for facilities
such as clocks and random bytes. It is not a durability service or an OS image.
This host supplies the reactor's small import surface directly, without adopting
the upstream JavaScript wrapper's event loop or a general WASI environment.

The VM image consists of:

```text
Image = { memory: all bytes of linear memory zero,
          meta: { stack: i32, runtime: i32, context: i32 } }
```

The pointers are wasm32 offsets for the same pinned binary. Linear memory holds
QuickJS objects, closures, promise reactions/job queue, allocator metadata, C
globals and the C stack region. `stack` is exported mutable `__stack_pointer`;
`runtime`/`context` are exported through getter functions. Memory length is part
of the image. The indirect function table is reconstructed by instantiating the
same module; it must not acquire dynamic state between checkpoints.

Validate the original module before compiling: imports are functions only; all
mutable globals, including hidden ones, consist solely of the exported i32 stack
pointer. Reject table set/grow/fill/copy/init, element drop, memory.init and data.drop.
The instrumentation path additionally permits one private defined wasm32 memory
and the supported scalar/bulk instruction set; reject SIMD, atomics/shared memory,
memory64, imported/multiple memories and GC proposals. Do not silently accept a
new reactor ABI because its exported names happen to match.

The comparison with Vercel's pinned wrapper established the same core fields:
memory, stack pointer, runtime pointer and context pointer. Its extensions also
record extension identities and memory/table bases and reconstruct table entries;
this implementation has no extensions. Host callback registrations, native
resources and Rust futures are not restored by copying memory. No forced GC is
required to capture an otherwise safe boundary.

### 3.2. Golden creation, eval and restore

Compile the instrumented module once and share it. Each current turn uses a fresh
Wasmtime Store/instance, deliberately exercising cold restore.

Golden creation, once per application version:

1. Instantiate with logical time zero and host identity `application/<application_id>`.
2. Call `_initialize()`, then `qjs_init2(~((1<<9)|(1<<10)))`; require return zero.
   These flags disable weak references/finalization and the performance intrinsic.
3. Apply QuickJS allocator and stack limits. Evaluate bridge code, then application
   source as a global script, and drain all QuickJS jobs.
4. Require `typeof main === 'function'`. Durable requests during initialization
   throw because the bridge has no run ID yet.
5. Free temporary handles/strings, read runtime/context pointers and stack global,
   and copy complete linear memory. Compress and stage the golden/catalog atomically.

Runtime-dependent initialization values become shared defaults. Keep per-invocation
input, identity and entropy out of top-level initialization. Reusing an application
ID with another source or build is rejected.

Relevant reactor exports, with wasm32 pointers/handles represented as i32:

| Export                                                                | Use                                                             |
| --------------------------------------------------------------------- | --------------------------------------------------------------- |
| `wasm_malloc(size) -> ptr`, `wasm_free(ptr)`                          | Allocate/free NUL-terminated source and bridge argument buffers |
| `qjs_eval(code_ptr, byte_len, filename_ptr, flags=0) -> value_handle` | Evaluate a global script                                        |
| `qjs_is_exception(handle) -> i32`, `qjs_get_exception() -> handle`    | Detect/extract exceptions                                       |
| `qjs_get_string_len(handle, length_ptr) -> cstring_ptr`               | Convert result to UTF-8 bytes; length is little-endian u32      |
| `qjs_free_cstring(ptr)`, `qjs_free_value(handle)`                     | Release temporary values before capture                         |
| `qjs_is_job_pending() -> i32`, `qjs_execute_pending_job() -> i32`     | Drain jobs until none; a negative execution result is an error  |
| `qjs_get_runtime_ptr()`, `qjs_get_context_ptr()`                      | Read snapshot offsets                                           |
| `qjs_set_runtime_and_context(runtime, context)`                       | Rebind a restored heap                                          |
| `qjs_set_memory_limit(bytes)`, `qjs_set_max_stack_size(bytes)`        | Apply limits on initialization and after restore                |

Reject null handles/allocations and out-of-bounds memory operations. Bridge hooks
accept JSON strings. Double JSON-encode the argument when constructing the eval
source, for example `__durable.deliver(<JSON string literal>)`, to avoid source
injection. Preserve explicit lengths so embedded NUL characters in values work.
Any trap/error during this process discards the working instance and transaction.

Restore in this order:

1. Reconstruct memory from storage as specified in section 5. Instantiate the same
   compiled module with fresh host imports, fuel, logical time and memory limits.
2. Require image length at least initial module memory, a multiple of 64 KiB and
   no more than the configured maximum; grow memory to that exact length.
3. Copy reconstructed bytes into linear memory zero. Clear the transient tracker.
4. Restore `__stack_pointer`; call `qjs_set_runtime_and_context` with saved offsets.
5. Reapply allocator/stack limits. These rebinding writes belong to the new dirty
   epoch. Do not call `_initialize`, `qjs_init2`, bridge initialization or top-level
   application evaluation over the restored heap.
6. Bind invocation state on first start, or deliver one completion, then run jobs.

Before capture, call the runtime/context getters and read the stack global, then
select/copy pages. If metadata getters touch memory, those writes must be included.

### 3.3. Host imports, time, randomness and bounds

The host's `now` stays fixed during a turn. On completion use
`max(delivery_now, previous_state.now)`. All times/deadlines are milliseconds in
`0..=18_446_744_073_709`, allowing conversion to unsigned 64-bit nanoseconds.
Sleep requires a nonnegative safe-integer duration and an in-range sum.

| Import                                                                                             | Binding                                                                                                                              |
| -------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------ |
| `clock_time_get(i32 clock, i64 precision, i32 out) -> i32`                                         | Clock 0 or 1: write `now * 1_000_000` as little-endian u64; unsupported clocks return errno 52                                       |
| `random_get(i32 ptr, i32 len) -> i32`                                                              | Write repeated bytes of SHA-256(host identity), bounded by guest memory; deterministic bootstrap input, not cryptographic randomness |
| `fd_write(i32 fd, i32 iov, i32 count, i32 written) -> i32`                                         | For fd 1/2, validate at most 1,024 vectors and bounds, discard output and write total byte count; other fds return errno 8           |
| `fd_fdstat_get(i32 fd, i32 out) -> i32`                                                            | For fd 1/2, write the 24-byte zeroed character-device descriptor with byte zero set to 2; other fds return errno 8                   |
| `fd_close(i32) -> i32`, `fd_seek(i32, i64, i32, i32) -> i32`                                       | Unsupported, errno 52                                                                                                                |
| `env.host_get_timezone_offset(i32 hi, i32 lo) -> i32`                                              | Return zero: UTC                                                                                                                     |
| `env.host_interrupt() -> i32`                                                                      | Return zero; Wasmtime fuel bounds execution                                                                                          |
| `env.host_module_normalize(i32, i32) -> i32`, `env.host_module_load(i32, i32) -> i32`              | Return null/zero: no external module loader                                                                                          |
| `env.host_call(i32, i32, i32, i32, i32) -> i32`, `env.host_promise_rejection(i32, i32, i32) -> ()` | Unexpected use traps; no native callback registry is enabled                                                                         |

Unprefixed imports in the table belong to `wasi_snapshot_preview1`; successful
WASI calls return zero. An I/O vector is a little-endian u32 pointer and u32 length;
`fd_write` writes its total as a little-endian u32. Validate all pointed-to ranges.
Every import writing guest memory uses the same range-marking helper as source
and argument copies. Imports must be rebound on each fresh Store.

The bridge overrides Math.random with xorshift32: `s ^= s << 13; s ^= s >>> 17;
s ^= s << 5; return (s >>> 0) / 2^32`. For a run, seed from the first four SHA-256
bytes of its UTF-8 ID, interpreted big-endian; replace zero with one. The state is
in the heap and survives checkpoints. Golden initialization starts from seed one.
These streams are deterministic and are unsuitable for secrets.

Reference limits, not a general resource-management API:

| Resource                                                  |                           Limit |
| --------------------------------------------------------- | ------------------------------: |
| Guest linear memory                                       |                          64 MiB |
| QuickJS allocator                                         |                          32 MiB |
| QuickJS stack                                             |                          32 KiB |
| Wasmtime fuel per fresh turn                              | 200,000,000, including barriers |
| Tracker memory                                            |    64 KiB, excluded from images |
| Application source, input, host outcome                   |                      1 MiB each |
| VM string extraction, including aggregate inspected state |                           1 MiB |
| Stored JSON metadata                                      |                          16 MiB |
| Application/run ID                                        |             1–1,024 UTF-8 bytes |

The 32 KiB stack limit is deliberately lower than upstream's 512 KiB ceiling:
the latter exhausted the instrumented Wasmtime native stack in the recursion
probe. Reapply the smaller limit after restore. Remaining native traps still
abort the turn. Wasm memory limits do not bound total host-process memory.

## 4. Discovering writes and choosing a diff

Separate discovery of written pages from encoding changes. An allocator hook
alone cannot see a store into an existing object, QuickJS reference-count updates
or C globals. A host memory allocator sees allocation/growth, not each Wasm store.
Use portable instrumentation of final Wasm for this baseline.

Add a second fixed memory, exported as `__durable_dirty_pages`, with one byte per
4 KiB application page. For 64 MiB, 16,384 bytes are used inside one 64 KiB Wasm
page. Original guest pointers still address memory zero. The tracker is transient,
never serialized and never recursively instrumented.

Rewrite each allowed scalar store, including narrow integer stores and floating
point stores, preserving its address, immediate offset, value and original trap
semantics. Perform the original store first; after success, mark both end pages
for its width. Scratch locals preserve operands. For a successful bulk copy/fill,
mark the entire destination interval. The transform can instrument memory.init
for test modules, but the snapshot ABI rejects that operation in the reactor.

```text
mark(offset, length):
    if length == 0: return
    first = offset / 4096
    last = (offset + length - 1) / 4096
    tracker[first .. last inclusive] = 1
```

Check host-side arithmetic and bounds. For guest writes, preserve the original
Wasm operation so overflowing/out-of-bounds effective addresses still trap; do
not let tracker arithmetic turn a trapping access into a valid one. Mark all
cross-page destinations. Newly grown memory is zero-filled: persist the new
length even if no new page is written. Reject unsupported writing instructions
rather than silently missing them.

At a safe boundary compare only marked pages with the parent image, treating
pages beyond parent length as zeros. Dirty means written; changed means different
at capture. A page written and restored to its original value needs no new payload.
Reset the tracker on restore before rebinding pointers/limits. A future warm-VM
path must retain the dirty epoch until commit, or restore committed state after
abort; it must resolve an uncertain commit before choosing a new epoch.

Selected encoding: **changed 4 KiB pages XORed against golden, batched with Zstd
level 3**. Reasons and alternatives from the experiment's existing measurements:

| Candidate                 | Trade-off                                                                                                                                                                                               |
| ------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| bsdiff4                   | Smaller heap-churn patches, expensive whole-image matching; historical scalar median ~143 ms, sparse-8-MiB ~1,084 ms among successful samples, with timeouts on larger pairs                            |
| Page XOR + Zstd           | Exploits stable memory offsets; historical scalar encode ~0.26 ms; ~73 KiB heap-churn patch versus ~25 KiB for bsdiff. Dirty tracking can eliminate most discovery scans                                |
| Replacing changed pages   | Simpler, but wastes space on unchanged bytes inside a dirty page                                                                                                                                        |
| Zstd patch-from           | Credible general matcher; ordinary dictionary compression is not equivalent to its patch-from configuration. Historical CLI timings include process/filesystem overhead                                 |
| Xdelta/VCDIFF, HDiffPatch | General alternatives; no special knowledge of Wasm heaps. The tested Xdelta Wasm port ran out of memory; its bundled native source was measured separately. HDiffPatch was researched, not measured     |
| Kernel write tracking     | mprotect/fault handling or Linux userfaultfd/PAGEMAP_SCAN can track writes without per-store barriers, but require owned mappings, runtime trap coordination, growth/reset handling and feature probing |

These are workload-dependent historical codec results, not current host-commit
latencies. Wasm compilation alone does not make a diff algorithm heap-aware.
Wasmtime COW initialization/Wizer can help preinitialization, but do not implement
this invocation transaction protocol. The pinned public engine APIs supplied no
ready-made dirty-page checkpoint list. Allocator layout cooperation may reduce
writes to golden pages, but must supplement complete write tracking.

## 5. Persistent format, capture and reconstruction

### 5.1. Record identities and layouts

Let `H(s)` be lowercase hex SHA-256 of UTF-8 s, `A=H(application_id)`, `R=H(run)`.
The first checkpoint sequence is zero. Its checkpoint/blob ID is `run/R/sequence`.
A blob reference is an ID; prepend `blob/` to fetch it. This is naming and build
identity, not snapshot content addressing or integrity verification.

| Logical key                 | Encoded value                                    |
| --------------------------- | ------------------------------------------------ |
| `application/A`             | Application JSON record                          |
| `blob/application/A`        | Zstd-compressed complete golden memory           |
| `blob/run/R/sequence`       | Compressed changed-page batch, if nonempty       |
| `checkpoint/run/R/sequence` | Manifest JSON record                             |
| `run/R/input`               | Original input JSON for duplicate-start checking |
| `run/R/head`                | Raw UTF-8 checkpoint ID, not a JSON string       |

Record field spelling matches the Rust experiment. Application/manifest fields
use snake_case; nested State uses camelCase. Optional State.outcome is omitted;
Manifest.previous is null for sequence zero. Integer page keys are decimal strings
in a JSON object. JSON whitespace/property order are not semantically significant.

```text
VmMeta = {stack: i32, runtime: i32, context: i32}
Application = {
    format: 1, id: original_application_id,
    source_hash: H(source), build: exact_runtime_build_id,
    blob: "application/A", memory_len: byte_count, meta: VmMeta
}
PageRef = {blob: blob_id, slot: zero_based_entry_index}
Manifest = {
    format: 1, application: original_application_id, build: runtime_build_id,
    previous: prior_checkpoint_id_or_null, sequence: unsigned_integer,
    meta: VmMeta, memory_len: byte_count,
    pages: {decimal_page_index: PageRef, ...},
    state: State,
    changed_pages: count, examined_pages: count, new_blob_bytes: count
}
```

`pages` is the **complete current override map** relative to golden, not a list of
edits to the previous manifest. Missing entries inherit the golden page or zeros
beyond golden length. `previous` is optional history navigation; reconstruction
never follows it. A page reference can point into an older batch without needing
that batch's old manifest.

For the reference implementation the build ID is SHA-256 of concatenated compiled-
input Wasm bytes (transformed in tracked mode), UTF-8 bridge source, then the restore
contract as a literal: `abi=<version>;wasmtime=49.0.3;stack=<bytes>;heap=<bytes>;
intrinsics=<flags>`. It covers what a restored image depends on, not the host's source
code: editing the host keeps stored runs restorable. Bump the ABI version when the
restore contract changes in a way these inputs don't show (import behavior, the bridge
protocol, VM metadata). A new implementation may use its own immutable compatibility
identity, but must pin all state-layout and restore semantics. Different source/runtime builds require new application
versions; retain old artifacts for old runs. There is no heap migration.

The returned View contains checkpoint ID, application ID, sequence, State, image
length and diagnostics: changed/examined/override page counts, new blob bytes and
manifest bytes. It contains no host commit acknowledgement.

### 5.2. Binary page batch

Golden payload: Zstd level-3 compression of exactly `Application.memory_len` bytes.
Changed-page payload: Zstd level-3 compression of this uncompressed structure:

| Offset/entry      | Encoding                                                      |
| ----------------- | ------------------------------------------------------------- |
| 0..3              | ASCII `QPD1`                                                  |
| 4..7              | Entry count N, unsigned 32-bit little-endian                  |
| Each of N entries | Page index as u32 little-endian, then exactly 4,096 XOR bytes |

Entries are sorted by increasing page index and unique. Decompressed length must
be exactly `8 + N * 4100`; N and each index are bounded by `64 MiB / 4096`.
The largest raw page batch is `8 + (64 MiB / 4096) * 4100` bytes. Reject truncated,
extra-length, unsorted or out-of-range entries and invalid slot references. Bound
decompression output, including reading at most limit+1 bytes to detect overflow.
The reference also bounds compressed blobs at 64 MiB + 1 MiB. These are format
and allocation checks, not extra assumptions about unreliable KV.

### 5.3. Capture algorithm

Inputs are golden image G, reconstructed parent P, current override map M, live VM
and its dirty tracker. On first start, P=G and M is empty.

```text
capture(G, P, M, VM, next_sequence):
    require no active Wasm call or pending host operation future
    read VM metadata first
    require VM.length >= P.length, aligned to 64 KiB, within limit
    next_map = copy(M)
    batch = []
    for each marked page i in ascending order:
        current = VM.page(i)
        previous = P.page(i), or zeros beyond P.length
        if current == previous: continue
        base = G.page(i), or zeros beyond G.length
        if current == base:
            remove next_map[i]
        else:
            batch.append((i, current XOR base))
    encode nonempty batch under blob/run/R/next_sequence
    set next_map[i] = {blob: new_blob_id, slot: entry_number} for batch entries
    stage manifest with full next_map, current length/metadata/State
    stage head = manifest_id
    return provisional View
```

Count compared pages as examined_pages. Count actual changes even when their
existing override is removed as changed_pages. No new blob is needed for an empty
batch. A zero page that differs from a nonzero golden page remains an explicit
XOR override. Size-only zero growth still requires the new manifest length.

The reference reuses/updates the reconstructed parent buffer to form the next
image, copying only actual changes out of Wasm. This avoids another full capture
copy; it does not eliminate full-image cold restoration.

### 5.4. Reconstruction algorithm

```text
reconstruct(run):
    read head and manifest; require matching format/build
    read application; require matching ID/format/build
    decompress golden; require its exact declared length
    require golden_length <= manifest.memory_len <= limit; both 64-KiB aligned
    memory = golden bytes, extended with zeros to manifest.memory_len
    group manifest page references by blob ID
    for each referenced blob:
        decode and validate the batch
        for each live reference (page i, slot) into that blob:
            require i within memory and batch[slot].page_index == i
            memory.page(i) XOR= batch[slot].xor_bytes
        release decoded batch before loading the next blob
    return Image(memory, manifest.meta), manifest.State
```

The golden image is immutable per application version, so a runtime may keep it
decompressed across turns; the reference caches it by application ID and source
hash. Each live page is applied once, against golden/zero bytes. Do not apply a golden-
relative delta to the previous invocation page. Do not retain every decoded old
batch simultaneously: mostly superseded batches can amplify retained memory.
Each referenced batch is still read/decompressed in full; compaction is future
work. Retain golden and every referenced blob. No record GC is implemented.

## 6. Engine algorithms and checkpoint boundaries

### 6.1. Registration, start, completion and read-only recovery

Registration validates the ID/source, then either verifies an existing immutable
catalog's source/build or creates a golden. Stage golden and catalog together.
Only the outer host transaction publishes them.

Start validates run/application IDs, input and time and calls `check_run`.
If the run already has a head, require the same application and structurally equal
original input; return its current View without guest execution. Otherwise stage
input identity, restore golden, bind run ID/time/seed, queue main and execute one
turn. The host must retain/redeliver initial admission until that first commit;
an uncommitted input record cannot recover a lost initial request.

Completion validates outcome/time and `check_run`, then loads current state:

1. If the operation is pending, set effective time to max(input time, state.now).
   For sleep require effective time at least deadline. Verify/stage result
   consumption through `acknowledge(id, outcome, effective_time)`.
2. Restore the image, initialize `handled` with **all IDs pending in the old
   manifest**, and deliver the one selected completion into the captured promise.
3. Execute a turn and stage the next checkpoint with sequence+1 in the same tx.

If the ID is not pending, accept a no-op only for canonical `run/<decimal n>` with
`n < nextOperation`; return current View without acknowledgement or guest work.
This includes locally resolved operations. There is no historical result receipt
to distinguish them. Foreign-run, noncanonical and never-issued IDs fail. A late
conflicting duplicate does not replace the committed outcome.

Read-only `recover(run)` validates run binding, reconstructs the image and returns
its View. It does not instantiate/enter Wasm, consume a result or re-emit a request.
The host reconnects pending IDs to its durable effects and chooses deliveries.
An invocation with a stored result and a VM still waiting for it is normal.

### 6.2. Turn pump

All native calls below return before calling the transactional host:

```text
handled = IDs from the pre-turn manifest's pending list, or empty for a start
ready_count = 0
loop:
    drain all QuickJS jobs
    state = inspect bridge
    saw_ready = false
    for op in state.pending, in issue order:
        if op.id in handled: continue
        add op.id to handled
        resolution = await transaction.execute(op) // no active Wasm call
        if Ready(outcome):
            require op is not Sleep or Resume
            validate outcome and size
            deliver outcome into its guest promise, with this turn's logical time
            // delivery queues jobs; do not run those jobs inside this batch
            saw_ready = true
            ready_count += 1
        else:
            require op is not a KV request
            // host has staged a durable effect; leave its promise pending
    if not saw_ready:
        break with state
    if configured_budget exists and ready_count >= configured_budget:
        invoke internal pause(), then inspect without draining jobs
        require last pending request is Resume
        require await transaction.execute(that Resume) returns Pending
        break with that state
capture and stage one image/manifest/head
return provisional View
```

Pre-populating `handled` prevents existing effects from being registered again
when one sibling finishes. Process the entire issued batch before testing the
budget; otherwise a capture could contain unregistered requests. Validate state
and outcome before capture. Any execution, host, serialization or staging error
poisons the engine and aborts the pre-decision transaction.

Default policy has no request budget: continue through local operations until
only durable effects remain or the invocation is terminal. A configured positive
budget counts Ready requests, not JS statements or elapsed time. A batch can
exceed it. A checkpoint may preserve queued continuations of already-resolved
promises; the committed Resume makes them schedulable after a crash.

| Situation                                   | Action                                                           |
| ------------------------------------------- | ---------------------------------------------------------------- |
| Local KV/custom result                      | Deliver and continue in the same turn unless budget reached      |
| Rust operation future awaiting I/O          | Retain VM, transaction and tracker; do not capture               |
| Durable RPC/timer wait, no runnable jobs    | Capture and commit pending effect identities                     |
| Host budget reached                         | Finish batch, stage ready Resume, capture before queued jobs run |
| Root settled, no pending requests           | Capture terminal state; publish only after commit                |
| Infinite synchronous job or microtask chain | Fuel may trap; abort. No native-stack preemption snapshot        |

Any available effect can be delivered next, not just Resume. If an RPC arrives
before Resume, its turn also drains already queued jobs. More than one Resume
can coexist; each remains a pending effect until consumed, and none replays an
old host operation. Final status must still wait for all pending IDs to drain.

## 7. SQLite validation host

Use a file-backed SQLite database with one writer connection, `journal_mode=WAL`,
`synchronous=FULL`, `wal_autocheckpoint=1000`, and zero busy timeout. Reserve the
writer with `BEGIN IMMEDIATE` before executing JS. A conflicting writer fails;
there is no retry thread or worker pool. SQLite's WAL checkpoint is database
maintenance, distinct from a VM checkpoint; no WAL truncation is required per turn.

Schema:

```sql
CREATE TABLE checkpoints (key TEXT PRIMARY KEY, value BLOB NOT NULL) WITHOUT ROWID;
CREATE TABLE state (
  scope TEXT NOT NULL, key TEXT NOT NULL, value BLOB NOT NULL,
  PRIMARY KEY(scope, key)
) WITHOUT ROWID;
CREATE TABLE runs (run TEXT PRIMARY KEY, scope TEXT NOT NULL) WITHOUT ROWID;
CREATE TABLE effects (
  id TEXT PRIMARY KEY, run TEXT NOT NULL, operation BLOB NOT NULL,
  outcome BLOB, completed_at INTEGER,
  CHECK((outcome IS NULL) = (completed_at IS NULL))
);
CREATE INDEX effects_run ON effects(run);
```

JSON state, operations and outcomes are stored as UTF-8 bytes. SQL NULL outcome
means result unavailable; successful JSON null is an encoded Outcome, not SQL NULL.

`begin(run, scope)` starts the transaction, inserts the binding if absent, then
requires the stored scope to match. The binding commits with that transaction
and cannot change on reopening. `check_run` requires the engine's run to equal
the transaction's run, including turns that issue no guest requests.

The transaction's checkpoint get/put use `checkpoints`. Its `execute` validates
operation ownership, then:

- KV get selects `(scope,key)` or returns JSON null; set upserts serialized JSON;
  delete removes the row. Return Ready(success(value/null)).
- RPC/custom/sleep inserts an effect with no outcome and returns Pending.
- Resume inserts an effect with successful null and `completed_at=0`, returning
  Pending. Effective delivery time is clamped to the VM's current logical time.

`effects(run)` reads committed rows. Dispatch only uncompleted external requests,
using the stable operation ID. Deliver completed rows to `Engine::complete`.
Read-only consumers spanning several SQL queries use one read transaction if
they need one consistent multi-table view.

`record_result(id, outcome, now)` is a separate short `BEGIN IMMEDIATE` transaction:
validate discriminator, size and time; find an effect without an outcome. If none
exists, return false. For a timer require now at least its deadline. Store outcome
and time and commit. First recorded result wins. It advances no guest state.
Reject unusable timestamps before recording: otherwise first-result-wins can
retain a result that the VM will never accept.

During completion, `acknowledge` selects the row for `(id, transaction.run)` with
an available result, requires equal outcome and effective delivery time >= stored
completion time, then deletes exactly one row. A mismatch fails without losing
the committed result. Consumption, subsequent KV mutations, new effects and the
new VM checkpoint all commit together. Dropping the transaction rolls them all
back. `into_storage()?.commit()` performs the host commit after successful execution.

A crash between recording and consuming a result leaves it available. A crash
after consuming it but before the VM commit restores both the old VM and result.
A crash after commit restores the new VM without that pending ID. If COMMIT's
outcome is uncertain, let database recovery establish committed state before retry.
Actual power-loss durability depends on filesystem/device sync semantics; the
implemented crash tests exercise process termination.

## 8. Atomic-KV host: proposed redo WAL

This section completely specifies a minimal replacement for SQLite's multi-key
transaction. It requires only atomic durable get/put, a strict single writer and
a host visibility gate. It is storage redo of already-computed bytes, not a guest
operation journal. The implementation and fault tests remain outstanding.

### 8.1. Namespace, encoding and discovery

Reserve physical `host/transaction` for control. All ordinary logical keys map to
physical `data/` followed by the logical key. The guest cannot access either raw
namespace. The staging API exposes only logical keys; it cannot stage a write to
the control key. Prefixes for checkpoints, state, effects and bindings are disjoint.

For a concrete put-only encoding, represent base values as one leading byte:
`0x01 || value_bytes` is a present value; exactly `0x00` is a tombstone. Missing
and tombstoned keys read absent. JSON null remains ordinary present value data.
The overlay is a map of logical key to final present value or deletion, with
reads consulting the overlay first. Repeated writes coalesce to the last value.

Suggested additional logical keys for this adapter:

| Key                      | Logical value before base-value encoding                                               |
| ------------------------ | -------------------------------------------------------------------------------------- |
| `binding/R`              | JSON `{run, scope}`; immutable binding                                                 |
| `state/H(scope)/H(key)`  | Application JSON value                                                                 |
| `effect/H(operation_id)` | JSON `{run, operation, outcome, completed_at}`; last two fields null or both populated |
| `active_runs`            | JSON array of run IDs whose committed State is waiting                                 |

The hash function is the naming function from section 5. On first invocation
commit add the run to active_runs; on terminal commit remove it. A scheduler can
read that one index, load each manifest, and follow pending IDs to effect rows
without a listing API. A result-arrival transaction updates the effect row using
the same WAL wrapper; scanning active runs finds it afterward. No wakeup may rely
solely on the reusable WAL record, which is cleared before dispatch.

Treat active_runs as a set, encoded as a sorted unique array. Use the same logical
execute, first-result-wins recording and checked acknowledgement rules as section
7, replacing SQL reads/writes/deletes with overlay operations. Bind scope before
or with first invocation commit and check the run on every engine entry point.

Initial admission is retained/redelivered by the embedding until first commit.
For a completely standalone service, durable admission and its discovery index
would be another host transaction before starting the VM. Terminal results remain
addressable by run ID for caller retries after active_runs removal. A push-reply
transport needing autonomous resend must retain a durable reply outbox through
its acknowledgement; that transport is outside the minimal validation host.

Use UTF-8 JSON for the control record, independently of the base-value envelope:

```json
{ "format": 1, "state": "idle", "applied": 41 }
```

```json
{ "format": 1, "state": "redo", "tx": 42, "writes": [{ "key": "data/example", "value": "AQ==" }] }
```

Each redo value is standard base64 of the **complete encoded base value**, including
its leading tag. In the example `AQ==` means a present empty byte string; `AA==`
would mean a tombstone. Keys are unique physical data keys, sorted for repeatable
application. `tx` is the previous applied counter + 1, independent of run/checkpoint
counters. Absent control means idle/applied=0 on first use. Reject counter overflow,
invalid control formats and oversized serialized records before publication.

The complete redo must fit one store value. The first implementation rejects
larger transactions before deciding them. A later chunked protocol would need
durable immutable chunks before atomically publishing their complete descriptor;
it must not be silently substituted for this baseline.

### 8.2. Writer ownership and visibility gate

Hold writer ownership from the beginning of staging through commit/application.
Finish old storage recovery before loading a VM or reading base state for staging.
Readers may inspect the previous committed base state while the writer builds its
in-memory overlay. Each logical multi-key reader holds a shared gate for its whole
read. Before publishing redo, acquire the exclusive gate and drain prior readers.

Hold the exclusive gate through redo publication, all base assignments and idle
publication. It covers restoration, effect dispatch, delivery and maintenance as
well as ordinary reads. Raw KV clients bypassing it can see partial application;
individual atomic puts cannot hide a multi-key transition from those clients.
On process startup the gate starts closed until recovery finishes.

Ownership means more than a single live writer task: an old timed-out put must
not complete after a successor reuses its keys/WAL slot. Settle or order in-flight
writes before interpreting a failed acknowledgement or transferring ownership.
If the environment cannot ensure this, it needs fencing beyond this design.

### 8.3. Commit and recovery algorithms

Perform all fallible VM execution, page compression and encoding before publishing
the redo record. Stage checkpoint records, KV updates, effects, consumption,
bindings and index changes in the same overlay. Do not modify any base key yet.

```text
recover_storage():                         // ownership + exclusive gate held
    c = get(host/transaction), or Idle(0)
    validate c
    if c is Idle(N): return N
    for write in c.writes:
        await durable_put(write.key, decode_base64(write.value))
    await durable_put(host/transaction, encode Idle(c.tx))
    return c.tx

begin_transaction():                       // sole writer ownership acquired
    base_tx = recover_storage() behind exclusive gate
    allow shared readers; retain sole writer ownership
    overlay = empty                        // base is fully committed
    return Transaction { base_tx, overlay } // reads consult overlay first

commit(transaction):
    N = transaction.base_tx
    freeze transaction.overlay and serialize complete Redo(N+1)
    acquire exclusive gate; wait for logical readers to finish
    require control is Idle(N)             // ownership held since begin
    await durable_put(host/transaction, Redo(N+1))
    // DURABLE DECISION: must finish this transaction from here onward
    recover_storage()                      // applies values, then Idle(N+1)
    release gate and writer ownership
    return committed
```

A durable Redo means **committed and needing application**, not prepared. Base
writes can be applied sequentially; await durable acknowledgement of every write
before storing Idle. Applying an assignment sets exact bytes. Never redo an
increment, execute a callback, send an RPC, or XOR into an existing stored value.
Snapshot XOR payloads are stored as final encoded blobs; image reconstruction
later interprets them against golden.

On an apply error retain the redo and closed gate. Retry recovery; do not discard
the decision, expose partial base state or start another transaction. Repeated
recovery crashes are safe because absolute assignments/tombstones are idempotent.
Only after durable Idle may dispatch/new work/replies proceed. Head-last ordering
can aid inspection but is neither the commit point nor a substitute for the gate.

A lost acknowledgement is not evidence of abort. After settling/ordering the
uncertain operation, read control: Redo(N) must finish; Idle(N) is finished. An
older Idle proves no decision for N only if no delayed put can still publish it.
Keep ownership/gate closed until resolved. The applied counter helps resolve that
one transaction; it is not a general client-request deduplication journal.

### 8.4. Crash states and cost

| Failure point                         | Durable state                 | Recovery                                            |
| ------------------------------------- | ----------------------------- | --------------------------------------------------- |
| During execution/overlay construction | Old Idle, base untouched      | Abort provisional work                              |
| Atomic redo publication               | Old Idle or complete new Redo | Resolve uncertain write, then abort or roll forward |
| After redo, before any base writes    | Redo(N)                       | Apply all assignments                               |
| After some or all base writes         | Redo(N)                       | Reapply all assignments, then Idle(N)               |
| Atomic idle publication               | Redo(N) or Idle(N)            | Finish redo or proceed                              |
| After idle, before reply/dispatch     | Idle(N)                       | Read committed head and persistent work index       |

Before Redo there are no base mutations. After Redo, all bytes needed to finish
are durable, and the record survives until every assignment is durable. The gate
hides intermediate mixtures. These facts establish an all-or-nothing host-visible
transition under the stated assumptions, even though base writes are individually
atomic. They do not make arbitrary remote side effects atomic with local storage.

For N final keys, cost is N+2 durable puts: Redo, N assignments, Idle. Payloads are
written in both redo and base, plus JSON/base64 overhead. There are no per-key
progress records, CAS, undo records or content-integrity hashes. Acknowledge only
after Idle; background apply or concurrent decided transactions would require a
new visibility/ordering protocol and are outside this baseline.

## 9. Worked execution and failure examples

### 9.1. KV mutation, RPC and completion

```js
async function main(input, ctx) {
  const n = await ctx.kv.get('count');
  await ctx.kv.set('count', n + 1);
  const answer = await ctx.rpc.call({ service: 'Worker', handler: 'double' }, n + 1);
  await ctx.kv.set('lastAnswer', answer);
  return answer;
}
```

With count=41 and no request budget:

| Transaction                             | Application KV | VM checkpoint                            | Effect R                  |
| --------------------------------------- | -------------- | ---------------------------------------- | ------------------------- |
| T1: execute until RPC wait              | count=42       | C0 contains n=41 and pending RPC promise | Request for 42 registered |
| T2: result arrival                      | Unchanged      | Still C0                                 | Outcome 84 available      |
| T3: consume result and run continuation | lastAnswer=84  | C1 completed with 84                     | Consumed/removed          |

```mermaid
sequenceDiagram
    participant H as Host and QuickJS
    participant D as Transactional storage
    participant R as Remote service
    H->>D: T1 commit count, effect R and C0
    H->>R: Dispatch R with stable ID
    R-->>H: Result 84
    H->>D: T2 commit result availability
    H->>D: T3 read C0 and result; stage consumption
    Note over H: Restore and resume captured promise
    H->>D: T3 commit lastAnswer, C1 and consumption
    Note over H: Return committed result
```

A crash before T1 commits leaves no increment or dispatchable request. Initial
admission is redelivered. After T1 and before dispatch, recovery discovers R and
sends/reconnects it. After remote execution but before T2, local storage cannot
know whether the remote action happened: the transport/receiver must recover or
deduplicate by R's stable ID. After T2, C0 plus the available result is recoverable.
An abort during T3 preserves both. After T3 commits, a repeated completion is a
no-op and a repeated start returns the committed outcome.

With the KV WAL, T3 freezes final assignments for lastAnswer, tombstone(effect R),
new blob/manifest/head and active_runs removal. If a crash occurs after effect
removal but before head replacement, the physical base is mixed. Redo recovery
reinstalls the complete frozen transaction before anything reads it. It does not
rerun the continuation to recreate C1.

### 9.2. A KV read that fetches remotely

For `const n = await ctx.kv.get("n"); await ctx.kv.set("n", n+1)`, a host read may
return Poll::Pending while doing transactional I/O. Keep the transaction, VM and
tracker alive; no native Wasm call is active. Once I/O completes, return Ready and
continue. With no budget, both operations can share one checkpoint.

A crash/cancellation during the read aborts the whole pre-decision turn. An
explicit retry from the previous committed image may read a newer value; there
is no committed read-result receipt to replay. The remote read must nevertheless
participate in the host's isolation/ownership contract. An arbitrary unrelated
HTTP read is not automatically transactional KV.

### 9.3. Host-scheduled capture and fan-out

With a Ready budget of two, a loop performing get then set checkpoints after each
pair. The set's success has resolved its promise, but its continuation may still
be queued. Stage Resume with the image, commit, then deliver Resume to continue.
A ten-pair budget counts twenty Ready operations. There can be a final extra
checkpoint to drain the last continuation and record the terminal outcome.

For `Promise.all([rpcA(), rpcB()])`, the first checkpoint registers both requests
atomically. Completing B removes/consumes only B; `handled` prevents re-emitting A.
The next image retains B's resolved value while waiting for A. Completing A then
returns values in application array order. For Promise.race, the root winner may
be known earlier, but this prototype still waits for the losing effect to drain.

## 10. Restate binding and implementation acceptance

The Rust core requires an embedding, not an SDK journal. Map host invocation
identity and object/workflow scope to start/check_run, application state access
to KV requests, target service/handler/key to RPC routing, sleep to the host timer
service, and Resume to local scheduling. The host chooses an available completion
and uses its explicit durable recovery path to call complete.

Persist golden/runtime artifacts, retain initial admission until first commit,
and include checkpoint storage, KV changes, effect registration and consumption
in the same host transaction. Scope selection and RPC authorization belong to
the host. Publish only committed terminal status; outcome alone may exist while
waiting. Choose an explicit concurrency policy for different invocations sharing
a scope. This protocol has not yet been bound to a live Restate server's internals.

An implementation can be built and accepted in this order:

1. Pin the reactor, bind imports and implement golden creation plus full-memory
   cold restore. Validate pointer/stack restoration, closures, cycles, maps,
   pending promises and queued jobs before optimizing capture.
2. Implement the JSON bridge and turn pump with an isolated in-memory transaction.
   Add stable IDs, duplicate handling, run checks and poisoning on errors/cancel.
3. Add golden-relative page maps, exact batch codec, growth and override removal.
   Verify reconstructed memory/behavior without following old manifests.
4. Add write barriers and host marking, retaining a full-scan control and a
   verification oracle: every actually changed page must have been marked.
5. Bind a real atomic transaction, then add effects/results and result consumption.
   Verify before/after commit crash behavior and mismatch rejection.
6. Implement the KV WAL and visibility gate; only then claim KV-only durability.

Required acceptance cases:

| Area               | Cases                                                                                                                                                                                                                   |
| ------------------ | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Snapshot ABI       | Hidden mutable globals/table mutation/passive lifecycle rejected; stack limit catches recursive overflow before/after restore                                                                                           |
| Tracking           | Every scalar store width, offsets and cross-page writes; overlapping bulk writes; zero lengths; traps; growth; host writes; epoch reset; deliberate missed-write detection                                              |
| Images             | Golden isolation; sparse writes; zero growth; zero over nonzero golden; return-to-golden removes override; multiple referenced old blobs; bounded decoding                                                              |
| Promises           | all/race/any/allSettled, rejection, fan-out delivery order, captured queued KV result, RPC before Resume and multiple Resume delivery orders                                                                            |
| Transactions       | Read-your-writes; true pending host read; cancel before/after mutations; trap; failure before/after each staging action; no extraction/reuse after failure                                                              |
| Identities         | Changed source/build/input rejected; immutable scope across reopen; wrong-run transaction rejected even without requests; canonical duplicate IDs are no-ops                                                            |
| Completions        | Exact stored result and time checked; early/out-of-range timer result rejected; aborted consumption leaves result; duplicate after committed consumption does not execute guest                                         |
| WAL, still pending | Crash before/after every put, including redo/idle; repeated recovery crashes; lost acknowledgements with settled writes; overwrites/tombstones; no reader/dispatcher passes partial apply; indexes survive WAL clearing |

The current Rust revision passed 46 tests with `sqlite-host,verify-dirty` (including
two child helpers), and 39 overlapping full-scan control tests. Both passed Clippy
with warnings denied. Tests include 24 injected staging failures, two file-host
SIGKILL cases and four SQLite SIGKILL cases around start/completion commits.
These results validate the implemented hosts, not the proposed WAL or power loss.

## 11. Measurements and reproduction

Recorded locally on 17 October 2026: Rust 1.99.1, Wasmtime 49.0.3, SQLite 3.53.2,
Linux/x86_64 container filesystem. One warm-up plus three measured runs per case,
100 updates per run; 942 measured VM commits total. No network delay, warm VM
cache or dirty verification scan in timing runs. Small samples; no confidence
intervals or controlled before/after performance claim.

| Workload                   | VM commits/run | Turn p50 ms | Turn p95 ms | COMMIT p50 ms | Run p50 ms |
| -------------------------- | -------------: | ----------: | ----------: | ------------: | ---------: |
| KV, every update           |            101 |       2.932 |       3.653 |         0.530 |      308.2 |
| KV, every ten updates      |             11 |       3.623 |       5.951 |         0.501 |       40.3 |
| Sparse 8 MiB, every update |            101 |      18.359 |      22.601 |         0.708 |     1857.0 |
| RPC, every update          |            101 |       2.806 |       3.381 |         0.449 |      332.6 |

Turn includes BEGIN, cold restore, execution, capture, SQL staging and COMMIT,
including automatic SQLite WAL checkpoint work when triggered. Run also includes
effect lookups and RPC result commits; the RPC case adds 100 result transactions.
Compilation and golden registration are excluded. Small images are ~1.31 MiB;
the sparse case is ~9.31 MiB and emits ~6.67 KiB of new payload at its median turn.
Its median turn grows from ~16.1 ms over the first ten turns to ~20.6 ms over the
last ten because more old batches must be read/decompressed. Small diffs do not
make cold-restore cost proportional to changed bytes.

Raw current host measurements: `rust/sqlite-bench.json`, including runtime build
identity. Historical codec results are in `bench/codec-results.json`; old Rust
tracking/full-scan and integrity-hash comparison JSON files retain their own build
IDs/notes and are not current host measurements. Report scripts only print their
summaries; they do not regenerate separate Markdown documentation.

From the repository root:

```sh
cd rust
cargo test --release --features sqlite-host,verify-dirty --lib --tests
cargo test --release --features sqlite-host,full-scan --lib --test recovery --test host --test sqlite
cargo clippy --release --features sqlite-host,verify-dirty --all-targets -- -D warnings
cargo clippy --release --features sqlite-host,full-scan --all-targets -- -D warnings
cargo run --release --features sqlite-host --example sqlite -- /tmp/quickjs.sqlite
# Run the same example/path again: recover without incrementing again.
cargo bench --features sqlite-host --bench sqlite
python3 benches/sqlite_report.py
cargo bench --bench checkpoints
cargo bench --features full-scan --bench checkpoints
python3 benches/compare.py
```

`verify-dirty` is the expensive all-page correctness oracle; `full-scan` is the
uninstrumented benchmark/control build. They have distinct runtime build IDs.
Memory/file examples are `cargo run --release --example host -- /tmp/qjs-host`
and `cargo run --release --example recovery`. The file host publishes its entire
memory transaction domain as one fsync/rename/directory-fsync value; it is not
an implementation of the multi-key WAL. Tests live under `rust/tests`; benchmarks
under `rust/benches`. The library creates no executor or background thread.

For historical codec reproduction, Node 24+ and Python are required: `npm ci`,
create `.venv`, install `bench/requirements.txt`, then run `node bench/corpus.ts`
and `.venv/bin/python bench/compare.py`. Optional Xdelta preparation uses
`node bench/xdelta.ts`, `bash bench/build-xdelta.sh` and `XDELTA_LIB=/tmp/qjs-libxdelta.so`.
A zstd CLI enables patch-from rows; historical CLI/process and library versions
are recorded with the raw results. The Rust runtime has no Node/Python dependency.

## 12. Scope limits, provenance and references

The baseline deliberately has no warm-VM cache, compaction, GC, heap migration,
background WAL apply, concurrent writers, generic Wasm snapshots or dynamic native
extensions. Full page indexes and checkpoint history accumulate. Future GC must
retain every blob/golden/runtime artifact reachable from retained heads, not just
remove blobs whose checkpoint manifest is old. Snapshot memory limits are not
whole-process memory limits. A request budget cannot preempt arbitrary JS.

Applications are cooperative: the internal bridge and JavaScript built-ins are
not hardened against tampering. There is no guest filesystem/network/module-loader
API; RPC routing and authorization belong to the embedding. A non-idempotent remote
provider still needs a transport recovery/deduplication contract. These limitations
are separate from assuming reliable local KV.

The shipped binary's provenance fingerprint is SHA-256
`d4c9375f2b1ca4dc95f72c8aa2982a7a9951ac8011490d79c6582df732b4bbd9`.
This identifies the pinned dependency, not a snapshot integrity scheme. It comes
from the published quickjs-wasi 3.6.2 package, not a claimed reproducible source
build; the package does not supply a complete toolchain/submodule manifest.
Retain `rust/assets/quickjs-wasi-LICENSE`, `quickjs-ng-LICENSE`, `wasi-libc-LICENSE`
and component notices in `rust/assets/licenses/`. Replacing the binary requires
ABI review and new cold-restore validation/goldens.

Supporting sources for the already selected mechanisms:

- [Vercel driver at quickjs-wasi 3.6.2](https://github.com/vercel-labs/quickjs-wasi/blob/quickjs-wasi%403.6.2/src/index.ts), [C ABI](https://github.com/vercel-labs/quickjs-wasi/blob/quickjs-wasi%403.6.2/c/interface.c), [extensions](https://github.com/vercel-labs/quickjs-wasi/blob/quickjs-wasi%403.6.2/src/extensions.ts).
- [Zstd patching](https://github.com/facebook/zstd/wiki/Zstandard-as-a-patching-engine), [bsdiff](https://www.daemonology.net/bsdiff/), [Xdelta](https://jmacd.github.io/xdelta/), [HDiffPatch](https://github.com/sisong/HDiffPatch).
- [Wasmtime COW initialization](https://docs.wasmtime.dev/examples-fast-instantiation.html), [LinearMemory allocator](https://docs.wasmtime.dev/api/wasmtime/trait.LinearMemory.html), [Wizer](https://github.com/bytecodealliance/wizer).
- [Linux pagemap/PAGEMAP_SCAN](https://docs.kernel.org/admin-guide/mm/pagemap.html), [userfaultfd](https://docs.kernel.org/admin-guide/mm/userfaultfd.html), [soft-dirty](https://docs.kernel.org/admin-guide/mm/soft-dirty.html).
- [DFINITY memory tracker precedent](https://github.com/dfinity/ic/blob/3633026472367a413912bf797490c9336ba762f5/rs/memory_tracker/src/lib.rs).
- [SQLite transactions](https://www.sqlite.org/lang_transaction.html), [WAL](https://www.sqlite.org/wal.html), [synchronous settings](https://www.sqlite.org/pragma.html#pragma_synchronous).
