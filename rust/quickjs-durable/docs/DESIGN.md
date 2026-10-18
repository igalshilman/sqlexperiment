# QuickJS/Wasm durable execution

This is the implementation specification for a single-writer durable JavaScript
runtime embedded in Restate. It defines the guest bridge, snapshot ABI, record
formats, execution algorithms, the commit protocol and recovery rules. It is
self-contained: source links and references are supporting material, not
substitutes for protocol steps. Last consolidated: 18 October 2026.

The implementation in this crate uses QuickJS compiled to WebAssembly, Wasmtime,
4 KiB page tracking, and Zstd page batches, each compressed against the
pages it replaces, over a shared golden image. A turn ends in one commit record per scope (§7), in a Restate object's
state, which the `restate` module drives from each program's `run` handler
(§9.1). The VM stays live from turn to turn of one attempt.

Reading map: [model](#1-model-ownership-and-durability),
[interfaces](#2-guest-and-host-interfaces), [VM ABI](#3-quickjswasm-snapshot-abi),
[write tracking](#4-discovering-writes-and-choosing-a-diff),
[persistent format](#5-persistent-format-capture-and-reconstruction),
[run algorithms](#6-run-algorithms-and-turn-boundaries),
[storage and commit](#7-storage-one-commit-per-turn),
[examples](#8-worked-execution-and-failure-examples),
[acceptance](#9-restate-binding-and-implementation-acceptance),
[measurements](#10-measurements),
[limits and provenance](#11-scope-limits-provenance-and-references).

## 1. Model, ownership and durability

A **program** is a named, immutable source, identified by its name and its
**version**, a 64-bit fingerprint of the source. Its **golden image** is the VM
after evaluating it, before any run's input is bound. A **scope** is the unit
of storage and ownership: in Restate, one virtual object key. A **run** is one
invocation of `main` in a scope, identified by a run ID. A **turn** starts or
continues a run and ends at one commit. A **request** is a guest operation; an
**effect** is a request the host answers in a later turn (an RPC, a timer, a
wakeup). An **attempt** is one try at a run in one process: a crash ends it,
and the next attempt continues from the last commit.

The host owns scheduling, effects and retries. The run owns execution,
capture and the commit protocol over the scope's store.

Assumptions and invariants:

1. Storage is trusted: no integrity hashes. A put replaces one complete value.
2. One writer owns a scope at a time: in Restate, the object's exclusive
   handler, for the whole run. A scope holds one run's image at a time; its
   KV is shared by its runs, in turn.
3. Writes may be acknowledged before they are durable, as long as a failure
   loses only a suffix of them, in issue order (Restate's state commands).
   Reads within an attempt see its own writes.
4. A turn happened once its commit record is stored, with everything it
   lists (§7). Effects are dispatched only after the commit that issued them,
   in issue order, so a crash that loses the commit loses them too.
5. Snapshot only after returning completely from Wasm. The image contains
   QuickJS promises and queued jobs, never a native Wasm stack or a Rust future.
6. Recovery reconstructs the committed image. It does not replay JavaScript,
   issue historical KV reads, or repeat callbacks.

## 2. Guest and host interfaces

### 2.1. JavaScript API and wire values

Application source defines `main(input, ctx)`. `ctx` and global `durable` refer to
one frozen API. Its nested KV and RPC objects are frozen too.

| Call                                 | Request fields, in addition to `id` | Result                          |
| ------------------------------------ | ----------------------------------- | ------------------------------- |
| `ctx.kv.get(key)`                    | `kind:"kv.get", key`                | JSON value, or null when absent |
| `ctx.kv.set(key, value)`             | `kind:"kv.set", key, value`         | null, the write in the turn     |
| `ctx.kv.delete(key)`                 | `kind:"kv.delete", key`             | null, the delete in the turn    |
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
run ends instead: it commits a failed outcome named `Trap`, drops the trapped
VM (it is mid-mutation) and the run's outstanding requests. An infrastructure
error ends the attempt: the turn is not committed, and the next attempt
starts from the last commit.

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
are never delivered and their continuations never run; the terminal commit
has no pending request. A result that arrives later changes nothing.
When pausing with queued jobs, create the resume request **before** inspecting
state so the queued continuation is not misclassified as stalled.

### 2.3. Rust interface

```rust,ignore
trait Store {
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>>;
    async fn put(&self, key: &str, value: Vec<u8>) -> Result<()>;
    async fn delete(&self, key: &str) -> Result<()>;
}

Program::compile(runtime, name, source) -> Result<Program>   // golden creation
Programs::new(shared_runtime).compile(name, source)?
Run::start(&programs, &store, run, program, input, now).await -> Result<Run>
run.deliver(&store, operation_id, outcome, now).await -> Result<View>
run.deliver_all(&store, vec![(operation_id, outcome), ...], now).await -> Result<View>
run.view() -> View
```

`Run::start` begins a run with its first turn, or, when the scope's commit is
already that run's, continues it as committed: another attempt. `deliver`
answers one pending request and runs a turn; `deliver_all` answers several,
in order, in one turn. Every store call is awaited
outside any Wasm call, so a remote store may suspend in each. An error leaves
the `Run` unusable: the attempt ends, and the next one starts from the store.
`MemoryStore` keeps every write in order, so a test can keep any prefix of
them, as a crash may.

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

Compile the instrumented module once and share it. A run's Wasmtime
Store/instance lives for its attempt: it is restored once, from golden for a
new run or from the commit for another attempt, and then runs turn after turn.

Golden creation, once per program version, when the process starts (§5.5):

1. Instantiate with logical time zero and host identity `program/<name>`.
2. Call `_initialize()`, then `qjs_init2(~((1<<9)|(1<<10)))`; require return zero.
   These flags disable weak references/finalization and the performance intrinsic.
3. Apply QuickJS allocator and stack limits. Evaluate bridge code, then application
   source as a global script, and drain all QuickJS jobs.
4. Require `typeof main === 'function'`. Durable requests during initialization
   throw because the bridge has no run ID yet.
5. Free temporary handles/strings, read runtime/context pointers and stack global,
   and copy complete linear memory: the program's golden image.

Runtime-dependent initialization values become shared defaults. Keep per-run
input, identity and entropy out of top-level initialization.

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
Any trap/error during this process discards the instance.

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

Starting a VM is most of what a short run costs, so the runtime keeps it cheap:

- **Imports resolved once.** The runtime links the module's imports once
  (`InstancePre`); each VM only instantiates it.
- **A pool of instance slots.** Wasmtime's pooling allocator reuses memory
  slots from VM to VM and keeps each slot's first 2 MiB mapped, so copying
  golden in writes memory already resident instead of faulting in every page.
  The pool holds 1,000 VMs at once; past that, starting one fails, and the
  binding retries the attempt, as for a storage failure. Its 2,000 memory
  slots reserve ~8 TiB of address space (4 GiB each, so that Wasm needs no
  bounds checks), not memory: the process ran at ~150 MiB resident under load.
- **Golden shared.** An image is the program's golden memory, shared, and the
  pages that differ from it; a new run copies nothing but into the VM.

Measured on 18 October 2026, a run that settles in its first turn (a KV read
and write) took 1,755 µs in `MemoryStore`, of which 1,112 µs restoring golden
(mostly page faults) and 253 µs copying it; the pool made it 1,073 µs, the
linked imports 924 µs, the shared golden 771 µs. Against a server, one-turn
runs went from ~298/s to ~340/s and `Promise.all` of 10 calls from ~24/s to
~28/s; runs of 5 waits stayed at ~130/s.
6. Bind run state on first start, or deliver one completion, then run jobs.
   Each later turn reuses the instance: it resets fuel and logical time,
   delivers, runs, captures, and clears the tracker once the commit is stored.

Before capture, call the runtime/context getters and read the stack global, then
select/copy pages. If metadata getters touch memory, those writes must be included.

### 3.3. Host imports, time, randomness and bounds

The host's `now` stays fixed during a turn. On completion use
`max(delivery_now, previous_state.now)`. All times/deadlines are milliseconds in
`0..=18_446_744_073_709`, allowing conversion to unsigned 64-bit nanoseconds.
Sleep requires a nonnegative safe-integer duration and an in-range sum.

| Import                                                                                             | Binding                                                                                                                    |
| -------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------- |
| `clock_time_get(i32 clock, i64 precision, i32 out) -> i32`                                         | Clock 0 or 1: write `now * 1_000_000` as little-endian u64; unsupported clocks return errno 52                             |
| `random_get(i32 ptr, i32 len) -> i32`                                                              | Write repeated bytes of the 64-bit FNV-1a fingerprint of the host identity; deterministic bootstrap input, not randomness  |
| `fd_write(i32 fd, i32 iov, i32 count, i32 written) -> i32`                                         | For fd 1/2, validate at most 1,024 vectors and bounds, discard output and write total byte count; other fds return errno 8 |
| `fd_fdstat_get(i32 fd, i32 out) -> i32`                                                            | For fd 1/2, write the 24-byte zeroed character-device descriptor with byte zero set to 2; other fds return errno 8         |
| `fd_close(i32) -> i32`, `fd_seek(i32, i64, i32, i32) -> i32`                                       | Unsupported, errno 52                                                                                                      |
| `env.host_get_timezone_offset(i32 hi, i32 lo) -> i32`                                              | Return zero: UTC                                                                                                           |
| `env.host_interrupt() -> i32`                                                                      | Return zero; Wasmtime fuel bounds execution                                                                                |
| `env.host_module_normalize(i32, i32) -> i32`, `env.host_module_load(i32, i32) -> i32`              | Return null/zero: no external module loader                                                                                |
| `env.host_call(i32, i32, i32, i32, i32) -> i32`, `env.host_promise_rejection(i32, i32, i32) -> ()` | Unexpected use traps; no native callback registry is enabled                                                               |

Unprefixed imports in the table belong to `wasi_snapshot_preview1`; successful
WASI calls return zero. An I/O vector is a little-endian u32 pointer and u32 length;
`fd_write` writes its total as a little-endian u32. Validate all pointed-to ranges.
Every import writing guest memory uses the same range-marking helper as source
and argument copies. Imports must be rebound on each fresh Store.

The bridge overrides Math.random with xorshift32: `s ^= s << 13; s ^= s >>> 17;
s ^= s << 5; return (s >>> 0) / 2^32`. For a run, seed from the low 32 bits of
the FNV-1a fingerprint of its UTF-8 ID; replace zero with one. The state is
in the heap and survives checkpoints. Golden initialization starts from seed one.
These streams are deterministic and are unsuitable for secrets.

Reference limits, not a general resource-management API:

| Resource                                                  |                           Limit |
| --------------------------------------------------------- | ------------------------------: |
| Guest linear memory                                       |                          64 MiB |
| QuickJS allocator                                         |                          32 MiB |
| QuickJS stack                                             |                          32 KiB |
| Wasmtime fuel per turn                                    | 200,000,000, including barriers |
| Tracker memory                                            |    64 KiB, excluded from images |
| Application source, input, host outcome                   |                      1 MiB each |
| VM string extraction, including aggregate inspected state |                           1 MiB |
| Commit record                                             |                          16 MiB |
| Run ID                                                    |     1–1,024 UTF-8 bytes, no `/` |
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

At a safe boundary compare only marked pages with the parent image (the memory
as of the last commit, which the run keeps beside its live VM, as golden and
the pages that differ from it), treating pages beyond parent length as zeros. Dirty means written; changed means different at
capture. A page written and restored to its original value needs no new
payload. Reset the tracker on restore before rebinding pointers/limits, and
after each commit is stored. A failed commit ends the attempt with its VM.

Selected encoding: **the changed 4 KiB pages, compressed as one Zstd frame
(level 2, long-distance matching) with their previous contents as its
prefix**, in a chain over a base compressed against golden (§5.1). Zstd
matches each page against what it replaces, wherever the data now sits: a
page a turn changed by a few pointers costs a few bytes, and so does one whose
contents moved, which an XOR against the same offsets cannot see. Before
comparing, capture zeroes the dead bytes of the pages it compares (§5.3): the
stack below its pointer, where between calls no frame is live, and the free
chunks of the heap. A turn's dead frames, and what it allocated and freed,
would otherwise count as changes: a turn that churns ~100 KiB of strings
writes ~1.5 KiB, against ~70 KiB without.

Measured on 18 October 2026 over traces of seven workloads (40 turns each, 80
for the agent; batch KiB summed over the run's turns, and Zstd time over all
of them):

| Encoding of a turn's changed pages            | steps | kv  | agent | shift | typed | churn | map | Total | Encode |
| --------------------------------------------- | ----- | --- | ----- | ----- | ----- | ----- | --- | ----- | ------ |
| XOR with previous, Zstd 3 (before)            | 47    | 161 | 198   | 80    | 234   | 3,543 | 740 | 5,006 | 132 ms |
| XOR with golden, Zstd 3 (before the chain)    |       |     |       |       |       |       |     | 12,921 | 279 ms |
| Prefix of previous, Zstd 3                    | 38    | 111 | 138   | 62    | 226   | 2,672 | 862 | 4,111 | 251 ms |
| Prefix of previous, Zstd 3, long matching     | 37    | 109 | 135   | 41    | 66    | 2,692 | 769 | 3,851 | 443 ms |
| Same, stack zeroed                            | 32    | 100 | 116   | 35    | 59    | 2,671 | 762 | 3,777 | 422 ms |
| Same, Zstd 2                                  | 32    | 102 | 115   | 37    | 62    | 2,762 | 827 | 3,939 | 300 ms |
| Same, and free heap chunks zeroed (selected)  | 33    | 94  | 109   | 36    | 62    | 2,455 | 524 | 3,315 | 303 ms |

The workloads: a sleep loop (steps), KV reads and writes with an RPC each
(kv), a growing chat history (agent), arrays and strings shifted at the front
(shift), scattered writes to a typed array (typed), a fresh array of 2,000
objects each turn (churn), and a Map of 5,000 entries updated in place (map).
Zstd 19 on the XORs reached 4,015 KiB in 14.6 s; Zstd 1 with a prefix loses
most matches without long matching. Encoding a typical 20-page turn takes
~0.24 ms against ~0.06 ms for the XOR. On a run's bytes in `MemoryStore`, the
steps, kv, agent, typed and churn runs wrote 31%, 38%, 45%, 53% and 35% less.

Reasons and alternatives from the experiment's earlier measurements:

| Candidate                 | Trade-off                                                                                                                                                                                               |
| ------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| bsdiff4                   | Smaller heap-churn patches, expensive whole-image matching; historical scalar median ~143 ms, sparse-8-MiB ~1,084 ms among successful samples, with timeouts on larger pairs                            |
| Page XOR + Zstd           | Exploits stable memory offsets; historical scalar encode ~0.26 ms; ~73 KiB heap-churn patch versus ~25 KiB for bsdiff. Blind to data that moves within or between pages; replaced, above              |
| Replacing changed pages   | Simpler, but wastes space on unchanged bytes inside a dirty page                                                                                                                                        |
| Zstd patch-from, image    | The whole previous image as prefix: barely smaller than the changed pages' previous contents, and indexes the whole memory each turn (~2 ms per MiB)                                                     |
| Xdelta/VCDIFF, HDiffPatch | General alternatives; no special knowledge of Wasm heaps. The tested Xdelta Wasm port ran out of memory; its bundled native source was measured separately. HDiffPatch was researched, not measured     |
| Kernel write tracking     | mprotect/fault handling or Linux userfaultfd/PAGEMAP_SCAN can track writes without per-store barriers, but require owned mappings, runtime trap coordination, growth/reset handling and feature probing |

These are workload-dependent historical codec results, not current host-commit
latencies. Wasm compilation alone does not make a diff algorithm heap-aware.
Wasmtime COW initialization/Wizer can help preinitialization, but do not implement
this run commit protocol. The pinned public engine APIs supplied no
ready-made dirty-page checkpoint list. Allocator layout cooperation may reduce
writes to golden pages, but must supplement complete write tracking.

**Garbage collection and the allocator, measured and left as they are.**
QuickJS frees most objects by reference counting; its cycle collector runs
once ~256 KiB were allocated since the last run (the threshold then adapts).
Measured on 18 October 2026 over eight workloads (the seven above with a 100-turn
agent, plus `cycles`, dropping 300 objects with cycles and closures a turn,
and `fan`, `Promise.all` of 10 calls), bytes written per run in `MemoryStore`:

| Workload | Default | Collect every turn | Threshold 1 MiB | Threshold 4 MiB or more |
| -------- | ------- | ------------------ | --------------- | ----------------------- |
| steps    | 61 KiB  | 66 KiB             | 61 KiB          | 61 KiB                  |
| kv       | 179 KiB | 194 KiB            | 179 KiB         | 179 KiB                 |
| agent    | 501 KiB | 498 KiB            | 487 KiB         | 487 KiB                 |
| map      | 811 KiB | 812 KiB            | 814 KiB         | 809 KiB                 |
| churn    | 2,473 KiB | 2,475 KiB        | 2,433 KiB       | 2,433 KiB               |
| cycles   | 653 KiB | 726 KiB            | 822 KiB         | 1,997 KiB (5 MiB heap)  |
| fan      | 626 KiB | 652 KiB            | 626 KiB         | 626 KiB                 |

Collecting at every turn's end costs CPU (up to ~1 ms a turn on the Map) and
writes more, not less: the collector relinks the objects it visits, which
rewrites pointers across the heap. Collecting less often saves at most ~3%,
and lets cyclic garbage pile up into the image. The default stays.

The allocator's own bytes (chunk heads, free-list links) are 0-14 KiB of what
these runs changed, against 33 KiB-7.6 MiB of payload; pages changed only by
them are 0.5-7% of changed pages. Another allocator would need a rebuilt
QuickJS binary for little: what a turn writes is what it allocates and keeps.
Zeroing free chunks (§5.6) already makes what it allocates and frees free.

## 5. Persistent format, capture and reconstruction

### 5.1. Record identities and layouts

A scope's store holds three kinds of key:

| Key         | Value                                                    |
| ----------- | -------------------------------------------------------- |
| `commit`    | The scope's current run as of its last turn: JSON, below |
| `batch/<n>` | An immutable compressed page batch (§5.2); n counts up   |
| `kv/<key>`  | The scope's KV value of `key`, as JSON                   |

Programs and their golden images are not stored: the process supplies them
(§5.5). The commit, in camelCase JSON:

```text
VmMeta = {stack: i32, runtime: i32, context: i32}
Commit = {
    run: run_id, program: name, version: u64, runtime: u64,
    turns: count, meta: VmMeta, memoryLen: byte_count,
    chain: [batch_number, ...], baseBytes: count, deltaBytes: count,
    livePages: count, nextBatch: u64, state: State,
    writes: {key: json, ...}, deletes: [key, ...], garbage: [batch_number, ...]
}
```

`chain` is the image, as batches applied in order over golden (zeros beyond
its length): the first, the **base**, holds each page that differs from
golden; each later one, a **delta**, the pages one turn changed. Base and
deltas apply alike: each batch is compressed against the pages it replaces,
in the memory as it stands. `baseBytes` and `deltaBytes` size the chain, for when to write a new
base (§5.3); `livePages` counts pages that differ from golden. A settled run's
chain is empty. `writes`, `deletes` and `garbage`
are what follows the commit (§7). `runtime` is the runtime's version, a
constant bumped by hand with anything an image depends on: the Wasm, the
bridge, the limits, the imports' behavior. A run restores only on the
program version and runtime it started on: a changed program needs a new
name while the old one's runs finish. There is no heap migration.

The returned View holds the run, program, turns, State, memory length, live
pages, and what the last turn wrote: changed and examined pages, batch and
commit bytes.

### 5.2. Binary page batch

| Part            | Encoding                                                                    |
| --------------- | --------------------------------------------------------------------------- |
| Magic           | ASCII `QPD2`                                                                |
| Count           | Page count N, LEB128                                                        |
| N page indexes  | Each as its gap from the previous index plus one (the first: from 0), LEB128 |
| Pages           | One Zstd frame: the N pages, back to back, `N * 4096` bytes                 |

The **reference** is the N pages the batch replaces, back to back: their
contents in the memory the batch applies to. The frame is compressed with the
reference as its prefix (`ZSTD_CCtx_refPrefix`), level 2, long-distance
matching, and a window covering reference and pages (at most 2^27, for twice
64 MiB); decoding needs the same reference. Indexes are sorted, unique and
below `64 MiB / 4096`, and the frame decodes to exactly the reference's
length. The indexes come first, outside the frame, because decoding needs the
reference they name.

### 5.3. Capture algorithm

Inputs are golden image G, the parent P (the memory as of the last commit, kept
beside the live VM as G and the pages that differ from it), the commit's chain, the live VM and its tracker. A new run
has P=G and an empty chain.

```text
capture(G, P, chain, VM):
    require no active Wasm call or pending host operation future
    read VM metadata first
    require VM.length >= P.length, aligned to 64 KiB, within limit
    batch = []
    for each marked page i in ascending order:
        previous = P.page(i), or zeros beyond P.length
        zero VM's dead bytes in page i: below the stack pointer, and
            free heap chunks past their links (§5.6)
        current = VM.page(i)
        if current == previous: continue
        batch.append(i, current, against previous)
        P.page(i) = current
    if the run settled: chain = []                  // no image is kept
    else if batch is not empty:
        if chain is empty: the batch is the base     // P was G
        else if chain has 256 batches, or deltaBytes + batch > 2 * baseBytes:
            batch = every page of P that differs from G, against G
            chain = []                               // a new base
        chain.append(nextBatch)
    return chain, batch
```

Count compared pages as examined, actual changes as changed. Zeroing dead
bytes in the VM itself keeps the VM equal to the image it restores from;
golden is captured with all its dead bytes zeroed. Dead bytes in pages not
compared stay as they were, in the VM and in P alike. Updating P with
each changed page keeps it equal to the VM's memory as of the commit: the
`verify-dirty` build checks that every page that differs from it was marked.
A new base bounds restoring to at most 256 batches, and at most three times
the base's size. The batches it replaces become garbage.

The size rule alone keeps a run's rewrites to a third of what it writes: a
new base, of size B, follows 2B of deltas. The batch count only bounds the
reads of a restore, which fetches the chain's batches all at once; each new
base it forces costs a whole image. Measured on 18 October 2026, total bytes
written in `MemoryStore` (batches and commits):

| New base when                          | sleep loop, 400 turns | agent, 400 turns | Map of 5,000 entries, 100 turns |
| -------------------------------------- | --------------------- | ---------------- | ------------------------------- |
| 16 batches, or deltas > 2 × base       | 633 KiB               | 1,085 KiB        | 5,878 KiB                       |
| 256 batches, or deltas > 2 × base      | 620 KiB               | 1,010 KiB        | 2,256 KiB (58 batches)          |
| deltas > 1 × base                      | 735 KiB               | 1,229 KiB        | 2,669 KiB                       |
| deltas > 4 × base                      | 570 KiB               | 904 KiB          | 2,547 KiB                       |

The sleep loop and the agent write mostly commits and small deltas, over a
base of 5-10 KiB once dead bytes are zeroed; the Map's base is ~600 KiB, and
16 batches rewrote it every 16 turns.

### 5.4. Reconstruction algorithm

```text
reconstruct(commit):
    require the program's version and the runtime's match the commit's
    require golden_length <= memoryLen <= limit, 64-KiB aligned
    memory = golden bytes, extended with zeros to memoryLen
    read every batch in commit.chain, at once
    for each batch in commit.chain, in order:
        indexes = its page indexes; require each within memory
        pages = its frame, decoded against memory's pages at indexes
        write pages into memory at indexes
    return Image(memory, commit.meta)
```

The process keeps each golden image decompressed in its catalog, shared by
every image made from it: an image holds only the pages that differ. An attempt
reconstructs once, when its first delivery needs the VM; reading a settled
run's commit needs no image.

### 5.5. Programs

A program is static: a named source file, shipped in the binary. At startup the
process evaluates each into its golden image (§3.2) and keeps them in a catalog
(`Programs`) that every run of the process shares. Evaluation is
deterministic: a fixed identity, clock and seed, and no other input, so the
same source on the same runtime build gives the same image, byte for byte, in
every process. A checkpoint written by one process therefore restores in
another, and nothing but the source needs shipping. A commit records its
program's version and the runtime's; a run restores only with both.

### 5.6. Dead heap bytes

The pinned binary allocates with wasi-libc's dlmalloc (32-bit). Its heap is
chunks back to back from the first, at a fixed address after the binary's
static data (`heap::START`), to the end of memory: each a 4-byte footer slot
and a 4-byte head (the size, with bit 1 set when the chunk is in use and bit 0
when the one before it is), the last the allocator's top foot. A free chunk
holds its links in its first 32 bytes (footer slot, head, list or tree
links); the rest is dead until an allocation writes it, and `calloc` clears
what it returns.

```text
dead(memory):
    at = heap::START; previous_in_use = true
    loop:
        size, in_use, flag = head at at
        require size >= 16, a multiple of 8, within memory; flag == previous_in_use
        if the chunk ends memory: require it and the one before free; return dead
        if not in_use:
            require previous_in_use                  // free chunks never touch
            if size > 32: dead += [at + 32, at + size)
        at += size; previous_in_use = in_use
```

A walk that fails any check finds nothing dead, and capture zeroes only the
stack; golden registration requires the walk to succeed, so a binary with
another heap layout fails at startup.

## 6. Run algorithms and turn boundaries

### 6.1. Start, delivery and another attempt

`Run::start(run, program, input, now)` validates the run ID, input and time,
then reads the scope's commit and applies what follows it (§7). If the commit
is this run's, it is another attempt: require the same program, check its
version and the runtime's, and return the run as committed, without guest
execution and without a VM. Otherwise a new run takes the scope: restore
golden into a new VM, bind run ID, time and seed, queue `main` and run its
first turn. The previous run's batches become garbage of that first commit.

`deliver_all(outcomes, now)` validates each outcome and the time, and
refuses an operation listed twice; `deliver` is it with one outcome:

1. Set effective time to max(input time, state.now). For each pending
   timer delivered, require effective time at least its deadline.
2. If no listed operation is pending, return the current View.
3. If the attempt has no VM yet, reconstruct the committed image into one.
4. Deliver the pending completions into their promises, in the listed order,
   and run one turn (§6.2): they commit together.

If the ID is not pending, accept a no-op only for canonical `run/<decimal n>`
with `n < nextOperation`; return the current View without guest work. This
includes requests answered in their turn, and those dropped when the run
settled. Foreign-run, noncanonical and never-issued IDs fail.

### 6.2. Turn

All native calls below return before the run awaits the store:

```text
begin: reset fuel, set logical time
seen = IDs pending in the last commit, or empty for a start
loop:
    drain all QuickJS jobs
    state = inspect bridge
    answered = false
    for op in state.pending, in issue order, not in seen:
        add op.id to seen
        if op is a KV request:
            get: the turn's own write or delete of the key, else kv/<key>
            set/delete: record it in the turn's writes
            deliver the result, with this turn's logical time
            answered = true
        // other requests stay pending: effects for the host
    if not answered: break with state
    if the turn has spent half its fuel:
        invoke internal pause(), then inspect without draining jobs
        require the last pending request is a Resume
        break with that state
capture (§5.3) and commit (§7)
```

Pre-populating `seen` keeps effects already issued from being issued again
when a sibling finishes. A turn continues through local requests until only
effects remain, the run settles, or it has spent half its fuel: one local KV
await costs roughly 300,000 fuel (more with write barriers), so a few hundred
sequential awaits would otherwise exhaust a turn, trap, and trap again on
every retry. The committed Resume makes the queued continuations schedulable
after a crash.

| Situation                                   | Action                                                               |
| ------------------------------------------- | -------------------------------------------------------------------- |
| KV request                                  | Answer in the turn and continue                                      |
| Store read awaiting I/O                     | Keep the VM and the tracker; no Wasm call is live; do not capture    |
| Effects only, no runnable jobs              | Capture and commit the pending requests                              |
| Half the fuel spent                         | Finish the batch, add a Resume, capture before queued jobs run       |
| Root settled                                | Commit the outcome with no image and no pending request; drop the VM |
| Infinite synchronous job or microtask chain | Fuel traps; the run ends failed                                      |

Any pending effect can be delivered next, not just a Resume. More than one
Resume can coexist. Once the root settles, the run is final.

## 7. Storage: one commit per turn

A turn writes, in this order:

1. `batch/<nextBatch>`, when pages changed;
2. `commit`: the turn happened once it is stored;
3. what follows the commit: `writes` to `kv/<key>`, `deletes` of `kv/<key>`,
   and deleting `batch/<n>` for each garbage batch.

A failure loses a suffix of the writes (§1), so a stored commit always has its
batch. The next attempt reads the commit and applies what follows it again:
puts and deletes of the same values, so applying twice changes nothing, and a
crash between the commit and its last write loses nothing. A crash before the
commit loses the whole turn, and the next attempt runs it again from the
previous commit: the requests it issued were never dispatched (§1). The next
commit's own lists replace the previous one's. An orphan batch, written by a
turn whose commit was lost, has the number the next turn writes over.

Why not SQL. Until 18 October 2026 a scope's records lived in SQL tables
(`checkpoints`, `state`, `effects`) through sqlite-kv, which made a turn's
writes atomic with its own log. Measured against Restate, that cost a turn
about 2 ms of CPU to open the database again and parse its schema, wrote
every changed B-tree page, put each log segment twice (once more on the next
open, to confirm it), and copied all pages into place at each checkpoint. A
20-wait run journaled ~150 state commands and wrote ~150 KiB. The tables were
key/value maps (the effects table even repeated the run's pending list), and a
single writer with ordered writes needs no general transaction: one commit
record, written last, is the atomic step. It also made the runtime
independent of turso. A program that wants SQL can still call a Restate
service that has it.

`MemoryStore` implements the same contract for tests and the benchmark, and
keeps its writes in order so a test can keep any prefix of them: the crash
tests start an attempt after every write of a run, and again after every write
of the attempt that recovers from it.

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

With count=41:

| Turn                                | Writes, in order                                                                 | Effect R                    |
| ----------------------------------- | -------------------------------------------------------------------------------- | --------------------------- |
| T1: execute until the RPC wait      | `batch/0`, `commit` (count=42, R pending), `kv/count`=42                         | Dispatched after the commit |
| T2: deliver R's result 84, continue | `commit` (completed, no pages; lastAnswer=84), `kv/lastAnswer`, delete `batch/0` | Done                        |

A crash before T1's commit leaves nothing: the next attempt starts the run
again, and R was never dispatched. After T1's commit but before `kv/count`,
the next attempt applies it. After dispatch, the next attempt waits on R again
under its stable ID: Restate attaches to the call already made. After T2's
commit, a repeated delivery is a no-op and another attempt returns the outcome.

### 8.2. Fan-out

For `Promise.all([rpcA(), rpcB()])`, the first commit has both pending.
Delivering B commits a turn that keeps A pending; `seen` keeps A from being
issued again. Delivering A then returns values in array order. For
`Promise.race`, the run completes in the turn the winner settles it; the loser
is dropped.

## 9. Restate binding and implementation acceptance

The runtime requires an embedding, not an SDK journal: a store per scope, one
writer per scope, effect dispatch after the commit that issued them, and
delivery of their results. Scope selection and RPC authorization belong to
the host.

### 9.1. The Restate binding

The `restate` module binds runs to a live Restate server, on the Rust SDK with
journal replay off:

- Each program is a virtual object of its name, with no registration: the
  binary embeds its programs and evaluates them when it starts (§5.5). An object key is a
  scope; its exclusive handlers serialize its runs, one writer at a time.
- A run is one invocation of the object's `run` handler, which drives its
  turns itself, on one live VM. Its store is the object's state, remembering
  every key it read or wrote for the attempt: a key is read from Restate at
  most once. After each turn commits, the handler waits on Restate for the
  first pending request: an RPC as a call whose idempotency key is the
  operation ID, a timer as a sleep until the deadline; Resume is delivered at
  once. The completion is the next turn's event, with every other completion
  already arrived (polled once, without waiting): what arrives together
  commits once. A run of `Promise.all` of 10 calls took 3-10 turns instead
  of 11 against a local server. Uncommitted IDs are never
  waited on, so their reuse is safe (§2.2).
- A retried attempt restores the last committed image and waits again on its
  pending requests under the same keys: Restate attaches a re-issued call to
  the original invocation, within the callee's idempotency retention. Nothing
  is replayed, and the host keeps no results.
- `run` returns once `main` settles, when the run ends (§2.2): requests still
  outstanding are dropped, and a call already made finishes at its callee
  unobserved.

An implementation can be built and accepted in this order:

1. Pin the reactor, bind imports and implement golden creation plus full-memory
   cold restore. Validate pointer/stack restoration, closures, cycles, maps,
   pending promises and queued jobs before optimizing capture.
2. Implement the JSON bridge and the turn over a store in memory. Add stable
   IDs, duplicate handling, and an unusable run after an error.
3. Add the batch codec, the chain of batches, growth, and new bases.
4. Add write barriers and host marking, retaining a full-scan control and a
   verification oracle: every actually changed page must have been marked.
5. Add the commit protocol, then crash tests at every write.

Required acceptance cases:

| Area         | Cases                                                                                                                                                                             |
| ------------ | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Snapshot ABI | Hidden mutable globals/table mutation/passive lifecycle rejected; stack limit catches recursive overflow before/after restore                                                     |
| Tracking     | Every scalar store width, offsets and cross-page writes; overlapping bulk writes; zero lengths; traps; growth; host writes; epoch reset; deliberate missed-write detection        |
| Images       | Golden isolation; sparse writes; zero growth; zero over nonzero golden; pages back to golden leave the live count; a new base bounds the chain; bounded decoding                  |
| Promises     | all/race/any/allSettled, rejection, fan-out delivery order, captured queued KV result, RPC before Resume and multiple Resume delivery orders                                      |
| Commits      | Read-your-writes in a turn; a crash after any write, and again after any write of the recovering attempt, ends as without one; a live run and a restored one store the same bytes |
| Identities   | Another program version or runtime rejected; canonical duplicate IDs are no-ops; unknown IDs fail                                                                                 |
| Completions  | Early/out-of-range timer result rejected; a late result after the run settled changes nothing                                                                                     |

`make test-qjs` runs the runtime's tests with `verify-dirty` (every changed page
must have been marked) and again with `full-scan`, then the programs against a
Restate server, including a crash between a call's commit and its answer.

## 10. Measurements

`cargo bench --bench checkpoints` (and `--features full-scan`, the control)
runs a few workloads over `MemoryStore`, once with the VM live and once
restoring it every turn, and writes its results beside it as JSON.

Against Restate, measured on 18 October 2026 (4 vCPUs, one restate-server
1.7.13), with the same workloads as regular virtual objects of the TypeScript
SDK 1.17.3 on Node 22:

| Workload                                      | TypeScript       | QuickJS          |
| --------------------------------------------- | ---------------- | ---------------- |
| KV read and write, a new key each, 32 at once | 484/s, p50 60 ms | 307/s, p50 98 ms |
| KV read and write, one key, one at a time     | p50 8 ms         | p50 14 ms        |
| 20 waits on a 1 ms timer, one run             | 82 ms            | 93 ms            |
| 5 waits, 32 runs at once                      | 220/s            | 155/s            |
| `Promise.all` of 10 calls, 8 at once          | 47/s             | 27/s             |
| Primes below 100,000                          | 53 ms            | 488 ms           |
| Written to Restate by a run of 20 waits       | ~21 KiB          | ~56 KiB          |

Rows vary by ~10% from run to run. Before the commit record and the live VM,
the same rows were 220/s, 64 ms, 1.36 s, 40/s, 8.3/s, 1.8 s and 1.1 MiB;
before the chain of batches, the run of 20 waits wrote 137 KiB, and ~70
KiB before compressing pages against their previous contents (§4) and
zeroing dead bytes; the throughput rows did not move with those (an A/B of
both binaries on one server). Most of what is left is Restate's: a commit
record and the journal per turn. The Rust
SDK's server now sets `TCP_NODELAY`, which saved ~40 ms per round trip. One
synchronous JavaScript job is still capped at about 10^8 Wasm instructions,
and the interpreter runs compute-bound code ~10x slower than V8.

## 11. Scope limits, provenance and references

There is no heap migration, no concurrent writers in a scope, no generic Wasm
snapshot and no dynamic native extension. A settled run keeps only its commit
record, with its outcome. Snapshot memory limits are not whole-process memory
limits. Nothing preempts a single long JavaScript job but fuel.

Applications are cooperative: the internal bridge and JavaScript built-ins are
not hardened against tampering. There is no guest filesystem/network/module-loader
API; RPC routing and authorization belong to the embedding. A non-idempotent remote
provider still needs a transport recovery/deduplication contract.

The pinned QuickJS binary's SHA-256 is
`d4c9375f2b1ca4dc95f72c8aa2982a7a9951ac8011490d79c6582df732b4bbd9`,
for provenance only; nothing checks it at runtime. It comes
from the published quickjs-wasi 3.6.2 package, not a claimed reproducible source
build; the package does not supply a complete toolchain/submodule manifest.
Retain `assets/quickjs-wasi-LICENSE`, `quickjs-ng-LICENSE`, `wasi-libc-LICENSE`
and component notices in `assets/licenses/`. Replacing the binary requires
ABI review, cold-restore validation, a new runtime version, and its heap's
first chunk as `heap::START` (§5.6).

Supporting sources for the already selected mechanisms:

- [Vercel driver at quickjs-wasi 3.6.2](https://github.com/vercel-labs/quickjs-wasi/blob/quickjs-wasi%403.6.2/src/index.ts), [C ABI](https://github.com/vercel-labs/quickjs-wasi/blob/quickjs-wasi%403.6.2/c/interface.c), [extensions](https://github.com/vercel-labs/quickjs-wasi/blob/quickjs-wasi%403.6.2/src/extensions.ts).
- [Zstd patching](https://github.com/facebook/zstd/wiki/Zstandard-as-a-patching-engine), [bsdiff](https://www.daemonology.net/bsdiff/), [Xdelta](https://jmacd.github.io/xdelta/), [HDiffPatch](https://github.com/sisong/HDiffPatch).
- [Wasmtime COW initialization](https://docs.wasmtime.dev/examples-fast-instantiation.html), [LinearMemory allocator](https://docs.wasmtime.dev/api/wasmtime/trait.LinearMemory.html), [Wizer](https://github.com/bytecodealliance/wizer).
- [Linux pagemap/PAGEMAP_SCAN](https://docs.kernel.org/admin-guide/mm/pagemap.html), [userfaultfd](https://docs.kernel.org/admin-guide/mm/userfaultfd.html), [soft-dirty](https://docs.kernel.org/admin-guide/mm/soft-dirty.html).
- [DFINITY memory tracker precedent](https://github.com/dfinity/ic/blob/3633026472367a413912bf797490c9336ba762f5/rs/memory_tracker/src/lib.rs).
