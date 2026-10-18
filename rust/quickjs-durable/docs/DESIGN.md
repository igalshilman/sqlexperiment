# QuickJS/Wasm durable execution

This is the implementation specification for a single-writer durable JavaScript
runtime intended for embedding in Restate. It defines the guest bridge, snapshot
ABI, record formats, execution algorithms, host transactions and recovery rules.
It is self-contained: source links and references are supporting material, not
substitutes for protocol steps. Last consolidated: 17 October 2026.

The implementation in this crate uses QuickJS compiled to WebAssembly, Wasmtime,
4 KiB page tracking, and XOR/Zstd checkpoint payloads relative to a shared golden
image. Its storage is SQL tables on sqlite-kv in a Restate object's state (§7),
which the `restate` module drives from each program's `run` handler (§9.1).

Reading map: [ownership](#1-model-ownership-and-durability),
[interfaces](#2-guest-and-host-interfaces), [VM ABI](#3-quickjswasm-snapshot-abi),
[write tracking](#4-discovering-writes-and-choosing-a-diff),
[persistent format](#5-persistent-format-capture-and-reconstruction),
[engine algorithms](#6-engine-algorithms-and-checkpoint-boundaries),
[storage](#7-storage-sqlite-kv-in-the-objects-state),
[examples](#8-worked-execution-and-failure-examples),
[acceptance](#9-restate-binding-and-implementation-acceptance),
[measurements](#10-measurements),
[limits and provenance](#11-scope-limits-provenance-and-references).

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
A terminal guest failure may commit prior KV mutations. So does a VM trap: the
guest running out of fuel inside one job, or an error QuickJS cannot hand to a
script. The same image and delivery would trap again on every retry, so the
engine ends the run instead: it stages a failed outcome named `Trap` over the
last good image (the trapped one is mid-mutation) and abandons the run's
outstanding requests. Infrastructure errors and cancelled turns abort the
transaction.

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
"Execution stalled: no durable wakeup" outcome. Status is waiting until the root
settles, then follows its success or failure: **the run ends when `main`
settles**, as a request handler does, not when every promise has (as a Node.js
process would). Requests still pending then, such as a losing `Promise.race`
branch or an ignored promise, are dropped with the pending map: their results
are never delivered and their continuations never run, and the engine abandons
the run's outstanding effects in the terminal turn's transaction. A result that
arrives later finds no pending request and changes nothing.
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
    async fn abandon(&mut self, run: &str) -> Result<()>; // the run trapped
}

Program::compile(runtime, name, source) -> Result<Program>   // golden creation
Programs::new(shared_runtime).compile(name, source)?
Engine::new(transaction, shared_programs)
Engine::with_checkpoint_interval(NonZeroUsize) // optional host request budget
Engine::start(run, program, input, now).await -> Result<View>
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
embedding supplies that result; the sqlite-kv host checks it is still pending.

Set an engine usability flag false before registration/turn execution and restore
it only after success. If a polled future is dropped, an error occurs, or an
unwind interrupts execution, leave it false. Reject reuse, recovery and storage
extraction from that engine. Failed extraction consumes/drops the transaction;
the sqlite-kv host discards staged changes on drop. A fresh attempt starts from
committed state. This is an API guard, not permission for adapters to publish
staged data independently.

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
from npm **quickjs-wasi@3.6.2**, copied unchanged to `assets/quickjs.wasm`.
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

Golden creation, once per program version, ahead of time (§5.5):

1. Instantiate with logical time zero and host identity `program/<name>`.
2. Call `_initialize()`, then `qjs_init2(~((1<<9)|(1<<10)))`; require return zero.
   These flags disable weak references/finalization and the performance intrinsic.
3. Apply QuickJS allocator and stack limits. Evaluate bridge code, then application
   source as a global script, and drain all QuickJS jobs.
4. Require `typeof main === 'function'`. Durable requests during initialization
   throw because the bridge has no run ID yet.
5. Free temporary handles/strings, read runtime/context pointers and stack global,
   and copy complete linear memory: the program's golden image.

Runtime-dependent initialization values become shared defaults. Keep per-invocation
input, identity and entropy out of top-level initialization. Reusing an application
ID with another source or build is rejected.

Relevant reactor exports, with wasm32 pointers/handles represented as i32:

| Export                                                                                          | Use                                                             |
| ----------------------------------------------------------------------------------------------- | --------------------------------------------------------------- |
| `wasm_malloc(size) -> ptr`, `wasm_free(ptr)`                                                    | Allocate/free NUL-terminated source and bridge argument buffers |
| `qjs_eval(code_ptr, byte_len, filename_ptr, flags=0) -> value_handle`                           | Evaluate a global script                                        |
| `qjs_get_global() -> handle`, `qjs_get_prop_string(handle, name_ptr) -> handle`                 | Find a bridge hook                                              |
| `qjs_new_string(ptr, byte_len) -> handle`, `qjs_call(function, this, argc, argv_ptr) -> handle` | Call a hook; `argv_ptr` is an array of handles                  |
| `qjs_is_exception(handle) -> i32`, `qjs_get_exception() -> handle`                              | Detect/extract exceptions                                       |
| `qjs_get_string_len(handle, length_ptr) -> cstring_ptr`                                         | Convert result to UTF-8 bytes; length is little-endian u32      |
| `qjs_free_cstring(ptr)`, `qjs_free_value(handle)`                                               | Release temporary values before capture                         |
| `qjs_is_job_pending() -> i32`, `qjs_execute_pending_job() -> i32`                               | Drain jobs until none; a negative execution result is an error  |
| `qjs_get_runtime_ptr()`, `qjs_get_context_ptr()`                                                | Read snapshot offsets                                           |
| `qjs_set_runtime_and_context(runtime, context)`                                                 | Rebind a restored heap                                          |
| `qjs_set_memory_limit(bytes)`, `qjs_set_max_stack_size(bytes)`                                  | Apply limits on initialization and after restore                |

Reject null handles/allocations and out-of-bounds memory operations. Bridge hooks
accept JSON strings. Call them directly, without compiling source: look up
`__durable` and the hook with `qjs_get_global`/`qjs_get_prop_string`, make the
argument with `qjs_new_string`, call `qjs_call`, and free every handle before
capture. No value becomes source, so none can inject it; and a call costs a
fraction of an eval. Preserve explicit lengths so embedded NUL characters in
values work.
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
| Run ID                                                    |             1–1,024 UTF-8 bytes |
| Program name                                              | 1–128 letters, digits, `-`, `_` |

The 32 KiB stack limit is deliberately lower than upstream's 512 KiB ceiling:
the latter exhausted the instrumented Wasmtime native stack in the recursion
probe. Reapply the smaller limit after restore. Remaining native traps end the
run, as section 2.1 describes. Wasm memory limits do not bound total host-process memory.

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

Let `H(s)` be lowercase hex SHA-256 of UTF-8 s, and `R=H(run)`.
The first checkpoint sequence is zero. Its checkpoint/blob ID is `run/R/sequence`.
A blob reference is an ID; prepend `blob/` to fetch it. This is naming and build
identity, not snapshot content addressing or integrity verification.

| Logical key                 | Encoded value                                    |
| --------------------------- | ------------------------------------------------ |
| `blob/run/R/sequence`       | Compressed changed-page batch, if nonempty       |
| `checkpoint/run/R/sequence` | Manifest JSON record                             |
| `run/R/input`               | Original input JSON for duplicate-start checking |
| `run/R/head`                | Raw UTF-8 checkpoint ID, not a JSON string       |

Programs and their golden images are not stored here: the host supplies them
(§5.5). Record field spelling matches the Rust experiment. Manifest fields
use snake_case; nested State uses camelCase. Optional State.outcome is omitted;
Manifest.previous is null for sequence zero. Integer page keys are decimal strings
in a JSON object. JSON whitespace/property order are not semantically significant.

```text
VmMeta = {stack: i32, runtime: i32, context: i32}
PageRef = {blob: blob_id, slot: zero_based_entry_index}
Manifest = {
    format: 1, program: name, source: H(program_source), build: runtime_build_id,
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
identity, but must pin all state-layout and restore semantics. A run restores only
with the program source and build it started on: a changed program needs a new
name while the old one's runs finish. There is no heap migration.

The returned View contains checkpoint ID, program name, sequence, State, image
length and diagnostics: changed/examined/override page counts, new blob bytes and
manifest bytes. It contains no host commit acknowledgement.

### 5.2. Binary page batch

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
XOR override. Size-only zero growth still requires the new manifest length. When
the next manifest would reference more batches than the bound of section 5.4,
replace the batch with every live override, XORed against golden.

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

The golden image is immutable per program version, so a host keeps it
decompressed across turns: the reference's catalog holds it. Each live page is applied once, against golden/zero bytes. Do not apply a golden-
relative delta to the previous invocation page. Do not retain every decoded old
batch simultaneously: mostly superseded batches can amplify retained memory.
Each referenced batch is still read/decompressed in full, so capture bounds how
many a manifest references (16 in the reference): when the next manifest would
reference more, the turn writes every live override into its own batch, and the
manifest then references that one alone. A long run's restore reads at most that
many batches instead of one per turn. Retain golden and every referenced blob. No
record GC is implemented.

### 5.5. Programs

A program is static: a named source file, shipped in the binary. At startup the
process evaluates each into its golden image (§3.2) and keeps them in a catalog
(`Programs`) that every engine of the process shares. Evaluation is
deterministic: a fixed identity, clock and seed, and no other input, so the
same source on the same runtime build gives the same image, byte for byte, in
every process. A checkpoint written by one process therefore restores in
another, and nothing but the source needs shipping. A manifest records its
program's source hash and the runtime build; a run restores only with both.

## 6. Engine algorithms and checkpoint boundaries

### 6.1. Start, completion and read-only recovery

Start validates the run ID, input and time, finds the program in the catalog
and calls `check_run`. If the run already has a head, require the same program
(name and source hash) and structurally equal
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
    if (configured_budget exists and ready_count >= configured_budget)
            or remaining_fuel < half of the turn's fuel:
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
only durable effects remain, the invocation is terminal, or the turn has spent
half its fuel. That last stop matters: one local KV await costs roughly 300,000
fuel (more with write barriers), so a few hundred sequential awaits would
otherwise exhaust a turn, trap, and trap again on every retry. A configured positive
budget counts Ready requests, not JS statements or elapsed time. A batch can
exceed it. A checkpoint may preserve queued continuations of already-resolved
promises; the committed Resume makes them schedulable after a crash.

| Situation                                   | Action                                                                  |
| ------------------------------------------- | ----------------------------------------------------------------------- |
| Local KV/custom result                      | Deliver and continue in the same turn unless budget reached             |
| Rust operation future awaiting I/O          | Retain VM, transaction and tracker; do not capture                      |
| Durable RPC/timer wait, no runnable jobs    | Capture and commit pending effect identities                            |
| Host budget reached                         | Finish batch, stage ready Resume, capture before queued jobs run        |
| Root settled                                | Capture terminal state, dropping pending requests; publish after commit |
| Infinite synchronous job or microtask chain | Fuel traps; the run ends failed. No native-stack preemption snapshot    |

Any available effect can be delivered next, not just Resume. If an RPC arrives
before Resume, its turn also drains already queued jobs. More than one Resume
can coexist; each remains a pending effect until consumed, and none replays an
old host operation. Once the root settles, the run is final, whatever is pending.

## 7. Storage: sqlite-kv in the object's state

`namespace::NamespaceTransaction` keeps a scope's records in SQL tables in one
namespace of a key/value store, through sqlite-kv: in Restate, a virtual
object's state, one namespace per object key. sqlite-kv turns a session's page
writes into one atomic commit (a log transaction, usually a single put), so the
runtime needs no write-ahead log of its own.

```sql
CREATE TABLE checkpoints (key TEXT PRIMARY KEY, value BLOB NOT NULL);
CREATE TABLE state (key TEXT PRIMARY KEY, value BLOB NOT NULL);
CREATE TABLE effects (id TEXT PRIMARY KEY, run TEXT NOT NULL, operation BLOB NOT NULL);
```

- A turn is one sqlite-kv session; `commit` is one `save()`. Dropping the
  transaction discards every staged change. The namespace's owner (the object's
  exclusive handler) runs one turn at a time.
- The namespace is one application KV scope: every run in it shares `state`.
  KV requests resolve Ready in the turn, against `state`.
- `effects` is an outbox, `(id, run, operation)`, with no results: RPCs,
  timers and Resume are staged there and resolve Pending. The embedding waits
  for each itself, under its stable ID, once the turn that staged it has
  committed, and again after a crash. `acknowledge` checks that a delivered ID
  is still the run's outstanding request, and deletes it with the turn.
- When a run settles, `abandon` deletes its remaining requests in the same turn.

For tests and the benchmark, `MemoryTransaction` implements the same contract
over an in-memory map that the test publishes or discards as a whole.

## 8. Worked execution and failure examples

### 8.1. KV mutation, RPC and completion

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

### 8.2. A KV read that fetches remotely

For `const n = await ctx.kv.get("n"); await ctx.kv.set("n", n+1)`, a host read may
return Poll::Pending while doing transactional I/O. Keep the transaction, VM and
tracker alive; no native Wasm call is active. Once I/O completes, return Ready and
continue. With no budget, both operations can share one checkpoint.

A crash/cancellation during the read aborts the whole pre-decision turn. An
explicit retry from the previous committed image may read a newer value; there
is no committed read-result receipt to replay. The remote read must nevertheless
participate in the host's isolation/ownership contract. An arbitrary unrelated
HTTP read is not automatically transactional KV.

### 8.3. Host-scheduled capture and fan-out

With a Ready budget of two, a loop performing get then set checkpoints after each
pair. The set's success has resolved its promise, but its continuation may still
be queued. Stage Resume with the image, commit, then deliver Resume to continue.
A ten-pair budget counts twenty Ready operations. There can be a final extra
checkpoint to drain the last continuation and record the terminal outcome.

For `Promise.all([rpcA(), rpcB()])`, the first checkpoint registers both requests
atomically. Completing B removes/consumes only B; `handled` prevents re-emitting A.
The next image retains B's resolved value while waiting for A. Completing A then
returns values in application array order. For Promise.race, the run completes in
the turn the winner settles it; the losing effect is abandoned.

## 9. Restate binding and implementation acceptance

The Rust core requires an embedding, not an SDK journal. Map host invocation
identity and object/workflow scope to start/check_run, application state access
to KV requests, target service/handler/key to RPC routing, sleep to the host timer
service, and Resume to local scheduling. The host chooses an available completion
and uses its explicit durable recovery path to call complete.

Ship programs' sources, retain initial admission until first commit,
and include checkpoint storage, KV changes, effect registration and consumption
in the same host transaction. Scope selection and RPC authorization belong to
the host. Publish only committed terminal status. Choose an explicit concurrency policy for different invocations sharing
a scope.

### 9.1. The Restate binding

The `restate` module binds §7's storage to a live Restate server, on the
Rust SDK with journal replay off:

- Each program is a virtual object of its name, with no registration: the
  binary embeds its programs and evaluates them when it starts (§5.5). An object key is a
  scope; its exclusive handlers serialize its runs, one writer at a time.
- A run is one invocation of the object's `run` handler, which drives its
  turns itself. After each turn commits, the handler waits on Restate for the
  first pending request: an RPC as a call whose idempotency key is the
  operation ID, a timer as a sleep until the deadline; Resume is delivered at
  once. The completion is the next turn's event. Uncommitted IDs are never
  waited on, so their reuse is safe (§2.2).
- A retried attempt restores the last committed image and waits again on its
  pending requests under the same keys: Restate attaches a re-issued call to
  the original invocation, within the callee's idempotency retention. Nothing
  is replayed, and the host keeps no results: `effects` only checks that a
  delivered ID is pending.
- `run` returns once `main` settles, when the run ends (§2.2): requests still
  outstanding are dropped, and a call already made finishes at its callee
  unobserved.

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

Required acceptance cases:

| Area         | Cases                                                                                                                                                                           |
| ------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Snapshot ABI | Hidden mutable globals/table mutation/passive lifecycle rejected; stack limit catches recursive overflow before/after restore                                                   |
| Tracking     | Every scalar store width, offsets and cross-page writes; overlapping bulk writes; zero lengths; traps; growth; host writes; epoch reset; deliberate missed-write detection      |
| Images       | Golden isolation; sparse writes; zero growth; zero over nonzero golden; return-to-golden removes override; multiple referenced old blobs; bounded decoding                      |
| Promises     | all/race/any/allSettled, rejection, fan-out delivery order, captured queued KV result, RPC before Resume and multiple Resume delivery orders                                    |
| Transactions | Read-your-writes; true pending host read; cancel before/after mutations; trap; failure before/after each staging action; no extraction/reuse after failure                      |
| Identities   | Changed source/build/input rejected; immutable scope across reopen; wrong-run transaction rejected even without requests; canonical duplicate IDs are no-ops                    |
| Completions  | Exact stored result and time checked; early/out-of-range timer result rejected; aborted consumption leaves result; duplicate after committed consumption does not execute guest |

`make test-qjs` runs the runtime's tests with `verify-dirty` (every changed page
must have been marked) and again with `full-scan`, then the programs against a
Restate server, including a crash between a call's commit and its answer.

## 10. Measurements

`cargo bench --bench checkpoints` (and `--features full-scan`, the control)
measures capture and restore over `MemoryTransaction`, and writes its results
beside it as JSON.

Against Restate, measured on 18 October 2026 (4 vCPUs, one restate-server
1.7.13), compared with the same workloads as regular virtual objects of the
TypeScript SDK 1.17.3 on Node 22:

| Workload                                          | TypeScript       | QuickJS           |
| ------------------------------------------------- | ---------------- | ----------------- |
| Read and write the KV, a new key each, 32 at once | 520/s, p50 57 ms | 220/s, p50 140 ms |
| One wait, one run at a time                       | 4 ms             | 68 ms             |
| 5 waits, 32 runs at once                          | 243/s            | 40/s              |
| Journal entries and bytes written per wait        | 1, ~1 KiB        | ~30, ~55 KiB      |
| CPU per turn (SDK process + Restate)              | 0.6 + 1.2 ms     | 7 + 5 ms          |

A turn reads and writes the scope's database pages as state commands, which
the invocation's journal keeps until the run ends: a long run's journal grows
with every turn. The Rust SDK's server does not set `TCP_NODELAY`, which adds
~40 ms to each round trip (19 ms per wait with it). One synchronous JavaScript
job is capped at about 10^8 Wasm instructions, and the interpreter runs
compute-bound code ~30x slower than V8.

## 11. Scope limits, provenance and references

The baseline deliberately has no warm-VM cache, GC, heap migration,
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
Retain `assets/quickjs-wasi-LICENSE`, `quickjs-ng-LICENSE`, `wasi-libc-LICENSE`
and component notices in `assets/licenses/`. Replacing the binary requires
ABI review and new cold-restore validation/goldens.

Supporting sources for the already selected mechanisms:

- [Vercel driver at quickjs-wasi 3.6.2](https://github.com/vercel-labs/quickjs-wasi/blob/quickjs-wasi%403.6.2/src/index.ts), [C ABI](https://github.com/vercel-labs/quickjs-wasi/blob/quickjs-wasi%403.6.2/c/interface.c), [extensions](https://github.com/vercel-labs/quickjs-wasi/blob/quickjs-wasi%403.6.2/src/extensions.ts).
- [Zstd patching](https://github.com/facebook/zstd/wiki/Zstandard-as-a-patching-engine), [bsdiff](https://www.daemonology.net/bsdiff/), [Xdelta](https://jmacd.github.io/xdelta/), [HDiffPatch](https://github.com/sisong/HDiffPatch).
- [Wasmtime COW initialization](https://docs.wasmtime.dev/examples-fast-instantiation.html), [LinearMemory allocator](https://docs.wasmtime.dev/api/wasmtime/trait.LinearMemory.html), [Wizer](https://github.com/bytecodealliance/wizer).
- [Linux pagemap/PAGEMAP_SCAN](https://docs.kernel.org/admin-guide/mm/pagemap.html), [userfaultfd](https://docs.kernel.org/admin-guide/mm/userfaultfd.html), [soft-dirty](https://docs.kernel.org/admin-guide/mm/soft-dirty.html).
- [DFINITY memory tracker precedent](https://github.com/dfinity/ic/blob/3633026472367a413912bf797490c9336ba762f5/rs/memory_tracker/src/lib.rs).
