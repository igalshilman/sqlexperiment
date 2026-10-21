# QuickJS/Wasm durable execution

This is the implementation specification for a single-writer durable JavaScript
runtime embedded in Restate. It defines the guest bridge, snapshot ABI, record
formats, execution algorithms, the commit protocol and recovery rules. It is
self-contained: source links and references are supporting material, not
substitutes for protocol steps. Last consolidated: 18 October 2026.

The implementation in this crate uses QuickJS compiled to WebAssembly, Wasmtime,
4 KiB page write tracking (by the kernel where it can, else by instrumented
stores), and Zstd batches of the changed 64-byte blocks, each compressed
against the blocks it replaces, over a shared golden image. A turn ends in one commit record per scope (§7), in a Restate object's
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
after evaluating it as a module, before any run's input is bound. A **scope** is the unit
of storage and ownership: in Restate, one virtual object key. A **run** is one
call of a function the program exports, in a scope, identified by a run ID
(the invocation's). Each run starts from the golden image. A **turn** starts or
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
3. Writes are kept in issue order: a sync makes every write before it
   durable, and a crash keeps a prefix of those not yet synced. Reads within
   an attempt see its own writes.
4. A turn happened once its commit, one put of the scope's database record,
   is stored, with its pages and its KV writes (§7). Effects are dispatched
   only after the commit that issued them, in issue order, so a crash that
   loses the commit loses them too.
5. Snapshot only after returning completely from Wasm. The image contains
   QuickJS promises and queued jobs, never a native Wasm stack or a Rust future.
6. Recovery reconstructs the committed image. It does not replay JavaScript,
   issue historical KV reads, or repeat callbacks.

## 2. Guest and host interfaces

### 2.1. JavaScript API and wire values

Application source is an ES module: each function it exports is a handler,
called as `handler(input, ctx)`. Other exports, and functions it does not
export, are not handlers; it imports nothing. `ctx` and global `durable` refer to
one frozen API. Its nested KV and RPC objects are frozen too.

| Call                                 | Request fields, in addition to `id` | Result                          |
| ------------------------------------ | ----------------------------------- | ------------------------------- |
| `ctx.kv.get(key)`                    | `kind:"kv.get", key`                | JSON value, or null when absent |
| `ctx.kv.set(key, value)`             | `kind:"kv.set", key, value`         | null, the write in the turn     |
| `ctx.kv.delete(key)`                 | `kind:"kv.delete", key`             | null, the delete in the turn    |
| `ctx.rpc.call(target, input = null)` | `kind:"rpc", target, input`         | JSON response                   |
| `ctx.call(name, input = null)`       | `kind:"call", name`                 | The host binding's response     |
| `fetch(url, init)`                   | `kind:"call", name:"fetch"`         | A `Response`                    |
| `ctx.sleep(ms)`                      | `kind:"sleep", deadline: now + ms`  | null at/after deadline          |
| `ctx.signal(name, input = null)`     | `kind:"signal", name, input`        | The signal's value              |

`ctx.key` is the scope: the key of the object the program is served as.

A `signal` waits in `function_signals`, a journaled object keyed
`<program>:<key>` (`src/signals.rs`): the handler calls its `wait` under the
request's idempotency key, so a retried attempt joins the same wait, and the
run parks as for any call. The object's built-in handlers, `pending` and
`signal`, list the waits and resolve one (by its id, or the oldest for a
name; a signal no one waits for is dropped, or kept with `keep`). A run that
settles first has its waits forgotten.

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
guest running out of time inside one job, or an error QuickJS cannot hand to a
script. The same image and delivery would trap again on every retry, so the
run ends instead: it commits a failed outcome named `Trap`, drops the trapped
VM (it is mid-mutation) and the run's outstanding requests. An infrastructure
error ends the attempt: the turn is not committed, and the next attempt
starts from the last commit.

### 2.2. Snapshot-resident promise bridge

Keep these values inside the QuickJS heap:

- `runId`, initially unset; `nextOperation`, initially zero; logical `now`.
- PRNG state and host-supplied maximum time.
- An insertion-ordered map `pending[id] = {resolve, reject, input?}`.
- An optional `rootOutcome`.

Request creation validates/copies arguments before assigning an ID, then:

```text
request(fields):
    require runId is initialized
    id = runId + "/" + decimal(nextOperation)
    nextOperation += 1
    issued.append({id, ...fields})
    return new Promise((resolve, reject) => pending.set(id, {resolve, reject, input}))
```

The guest keeps each request's resolvers and, until it is answered, a call's
input (an RPC's, or a host binding's). The rest of the request goes to the
host at the next `inspect()` and is forgotten: the host keeps it pending, in
the commit, as its ID, target and idempotency key. The host reads a call's
input from the heap when it makes the call (the `input(id)` hook), and again
on another attempt; nothing else keeps a copy of it, and a binding's call is
journaled nowhere. An input crosses as `JSON.stringify` makes it, in one pass
in C: a primitive is checked when the call is issued, an object when it is
read, and one JSON cannot hold fails the call. A turn sending a 39 KiB request
saves ~22 KiB of pages and none of it in the commit; an agent's request,
mostly the conversation it keeps anyway, costs only the pages its last
messages take (§10.1). A program must not change an input until its call is answered:
it is read when the call is made. A request that loses a race may
reject later with no handler: no rejection tracker is installed, so that is
not an error, and no handler is attached to every request (one cost ~230 B a
turn). Creating a request performs no host I/O.
The operation counter covers local KV requests and internal wakeups as well as
external effects. IDs are stable across restore. Uncommitted IDs may be reused
because uncommitted requests must never have dispatched.

The internal bridge exposes six host hooks, not application API methods:

| Hook                 | Behavior                                                                                                             |
| -------------------- | -------------------------------------------------------------------------------------------------------------------- |
| `bind(namespace)`    | At golden creation: keep the module's namespace promise; `handlers()` then lists its exported functions              |
| `init(configJson)`   | Once only: set run ID, time, PRNG seed and time limit                                                                |
| `start(callJson)`    | Queue `handler(input, durable)` through `Promise.resolve().then(...)`; capture its normalized root outcome           |
| `deliver(eventJson)` | Advance logical time monotonically; find and remove the pending entry; resolve/reject its promise; do not drain jobs |
| `pause()`            | Create an internal `{kind:"resume"}` request through the same ID/map mechanism                                       |
| `input(id)`          | The pending call's input, as `JSON.stringify` makes it, or why it has none                                           |
| `inspect()`          | Return the requests issued since the last `inspect()`, then forget them; and where the run stands                    |

`inspect()` returns:

```text
Report = {
    issued: Operation[],        // issue order, since the last inspect()
    nextOperation: unsigned integer,
    now: milliseconds,
    status: "waiting" | "completed" | "failed",
    outcome?: Outcome
}
```

After draining jobs, a root still unsettled with no pending request is a failed
"Execution stalled: no durable wakeup" outcome. Status is waiting until the root
settles, then follows its success or failure: **the run ends when its
handler settles**, as a request handler does, not when every promise has (as a Node.js
process would). Requests still pending then, such as a losing `Promise.race`
branch or an ignored promise, are dropped with the pending map: their results
are never delivered and their continuations never run; the terminal report
issues nothing, and the terminal commit has no pending request. A result that arrives later changes nothing.
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
Run::start(&programs, &scope, key, run, program, handler, input, now).await -> Result<Run>
run.deliver(&store, operation_id, outcome, now).await -> Result<View>
run.deliver_all(&store, vec![(operation_id, outcome), ...], now).await -> Result<View>
run.view() -> View
```

`Run::start` begins a run with its first turn, or, when the scope's commit is
already that run's, continues it as committed: another attempt. `deliver`
answers one pending request and runs a turn; `deliver_all` answers several,
in order, in one turn. A run takes its `Scope`: the store its database lives
in, and, for a process that keeps owning it, an owner cache. Every store call
is awaited outside any Wasm call, so a remote store may suspend in each. An
error leaves the `Run` unusable: the attempt ends, and the next one starts
from the store. Tests run over sqlite-kv's `MemoryKv`, and its `FaultKv`
fails any one write, as a crash may.

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

Compile each engine's module once, without its data segments, and share it:
every VM maps a golden image over its memory (below). A fresh VM maps the
memory the segments lay out, as instantiating the module as built would; the
module must have no start function, which would run before it. The compiled
module is cached on disk (`FUNCTIONS_CACHE`, by default `~/.cache/functions`),
keyed by the engine's configuration, the tracking mode and the Wasm: a restart
loads it in milliseconds instead of compiling QuickJS (~1 s) and CPython
(~8 s) again. A run's Wasmtime
Store/instance lives for its attempt: it is restored once, from golden for a
new run or from the commit for another attempt, and then runs turn after turn.

Golden creation, once per program version, when the process starts (§5.5):

1. Instantiate with logical time zero and host identity `program/<name>`, and
   map the module's data over its memory.
2. Call `_initialize()`, then `qjs_init2(~((1<<9)|(1<<10)))`; require return zero.
   These flags disable weak references/finalization and the performance intrinsic.
3. Apply QuickJS allocator and stack limits. Evaluate bridge code, then application
   source as a module (`qjs_eval` flag 1), hand its namespace promise to
   `bind`, and drain all QuickJS jobs.
4. Require at least one exported function (`handlers()`), each a Restate
   handler name. Durable requests during evaluation throw because the bridge
   has no run ID yet.
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

1. Reconstruct memory from storage as specified in section 5. Instantiate the
   engine's module without data segments (its memory starts as zeros), with
   fresh host imports, logical time and memory limits.
2. Require image length at least initial module memory, a multiple of 64 KiB and
   no more than the configured maximum; grow memory to that exact length.
3. Map the program's golden image copy-on-write over the start of linear memory
   zero, then write only the pages that differ from golden. Clear the transient
   tracker.
4. Restore `__stack_pointer`; call `qjs_set_runtime_and_context` with saved offsets.
5. Reapply allocator/stack limits. These rebinding writes belong to the new dirty
   epoch. Do not call `_initialize`, `qjs_init2`, bridge initialization or top-level
   application evaluation over the restored heap.

Starting a VM is most of what a short run costs, so the runtime keeps it cheap:

- **Imports resolved once.** The runtime links the module's imports once
  (`InstancePre`); each VM only instantiates it.
- **A pool of instance slots.** Wasmtime's pooling allocator reuses memory
  slots from VM to VM. The pool holds 1,000 VMs at once; past that, starting
  one fails, and the binding retries the attempt, as for a storage failure.
  Its 2,000 memory slots reserve ~8 TiB of address space (4 GiB each, so that
  Wasm needs no bounds checks), not memory: the process ran at ~150 MiB
  resident under load.
- **Golden mapped, not copied** (`cow.rs`). A program's golden image is a
  sealed memory file, made on first use. A VM maps it privately over the start
  of its memory: the kernel shares its pages between every VM of the program
  and copies a page only when a VM writes it, so a VM costs the pages it
  touches, not the image's size. A restore then writes only the pages that
  differ from golden. Wasmtime could map an image itself, but only from a
  module's data segments, which would mean compiling a module per program
  (seconds for CPython); the runtime maps it instead, into a module without
  data segments, one per engine, which every VM of the engine shares, golden
  evaluation's included. The memory is Wasmtime's: when a VM ends it
  maps fresh zeros back before Wasmtime has the memory again, as its slot
  would hold after any VM (a unit test checks that nothing a VM wrote, over
  golden or past it, reaches the next).

Measured on 18 October 2026, a run that settles in its first turn (a KV read
and write) took 1,755 µs in `MemoryStore`, of which 1,112 µs restoring golden
(mostly page faults) and 253 µs copying it; the pool made it 1,073 µs, the
linked imports 924 µs, the shared golden 771 µs. Against a server, one-turn
runs went from ~298/s to ~340/s and `Promise.all` of 10 calls from ~24/s to
~28/s; runs of 5 waits stayed at ~130/s.

Measured on 19 October 2026 (`benches/languages.rs`, medians of 15 runs),
mapping golden instead of writing it: starting a VM and restoring it took
29 + 23 µs instead of 56 + 140 µs for QuickJS, and 30 + 26 µs instead of
1,700 + 3,900 µs for CPython's 11 MB image; a turn on a restored VM 1.0 ms
instead of 1.6-2.2 ms (JavaScript), 2.3 ms instead of 7.8-8.8 ms (Python, with
§5.6's partial walk).

6. Bind run state on first start, or deliver one completion, then run jobs.
   Each later turn reuses the instance: it restarts the turn's clock, sets logical time,
   delivers, runs, captures, and clears the tracker once the commit is stored.

Before capture, call the runtime/context getters and read the stack global, then
select/copy pages. If metadata getters touch memory, those writes must be included.

### 3.3. Host imports, time, randomness and bounds

The host's `now` stays fixed during a turn. On completion use
`max(delivery_now, previous_state.now)`. All times/deadlines are milliseconds in
`0..=18_446_744_073_709`, allowing conversion to unsigned 64-bit nanoseconds.
Sleep requires a nonnegative safe-integer duration and an in-range sum.

| Import                                                                                | Binding                                                                                                                                                      |
| ------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `clock_time_get(i32 clock, i64 precision, i32 out) -> i32`                            | Clock 0 or 1: write `now * 1_000_000` as little-endian u64; unsupported clocks return errno 52                                                               |
| `random_get(i32 ptr, i32 len) -> i32`                                                 | Write repeated bytes of the 64-bit FNV-1a fingerprint of the host identity; deterministic bootstrap input, not randomness                                    |
| `fd_write(i32 fd, i32 iov, i32 count, i32 written) -> i32`                            | For fd 1/2, validate at most 1,024 vectors and bounds, discard output and write total byte count; other fds return errno 8                                   |
| `fd_fdstat_get(i32 fd, i32 out) -> i32`                                               | For fd 1/2, write the 24-byte zeroed character-device descriptor with byte zero set to 2; other fds return errno 8                                           |
| `fd_close(i32) -> i32`, `fd_seek(i32, i64, i32, i32) -> i32`                          | Unsupported, errno 52                                                                                                                                        |
| `env.host_get_timezone_offset(i32 hi, i32 lo) -> i32`                                 | Return zero: UTC                                                                                                                                             |
| `env.host_interrupt() -> i32`                                                         | Return zero; Wasmtime's epoch deadline bounds execution                                                                                                      |
| `env.host_module_normalize(i32, i32) -> i32`, `env.host_module_load(i32, i32) -> i32` | Return null/zero: no external module loader                                                                                                                  |
| `env.host_call(i32 name, i32 name_len, i32 this, i32 argc, i32 argv) -> i32`          | The guest's `__host(operation, request)`: two strings in, a JSON string handle out, from the scope's database ([HOST.md](HOST.md)); awaits on the Wasm fiber |
| `env.host_promise_rejection(i32, i32, i32) -> ()`                                     | Unexpected use traps                                                                                                                                         |

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
| A turn's JavaScript, by the wall clock (`Limits`)         |       10 min; commits after 1 s |
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
Two mechanisms discover writes; the runtime picks one per process at startup.

**Native, where the kernel has it (Linux 6.7 or later).** Each VM's memory is
registered with a process-wide `userfaultfd` in asynchronous write-protect mode
(`UFFD_FEATURE_WP_ASYNC`, with `UFFD_USER_MODE_ONLY`, so no privilege is
needed): a write to a protected page faults into the kernel, which unprotects
it and returns, with no signal and no thread to wake. `PAGEMAP_SCAN` on
`/proc/self/pagemap` lists the pages written since (`PAGE_IS_WRITTEN`), and
with `PM_SCAN_WP_MATCHING` protects them again in the same walk. The module
runs as compiled, without barriers.

- Capture scans the memory for written pages; after the commit, and after a
  restore, a scan protects those pages again. Protecting the whole memory
  instead costs ~24 µs for 1.3 MiB and ~222 µs for 16 MiB, against ~3 and
  ~13 µs for scanning and protecting what was written.
- Host writes into the memory are user-mode writes: the kernel sees them too.
- Each VM registers its pool slot's first 64 MiB, the most memory can grow
  to, after mapping golden over it (a new mapping drops a registration);
  registering a slot again is harmless. Write protection works on the private
  file mapping as on anonymous memory.
- The startup probe creates a VM and scans it: a kernel or sandbox without
  either feature (`ENOSYS`, `EPERM`, a seccomp filter) falls back to barriers.
  `FUNCTIONS_TRACKING=barriers` forces them.

The cost moves from every store to the first write of each page in a turn: a
fault of ~1.5 µs. Measured on 18 October 2026, with the block encoding (§5.2)
in both:

| Workload                           | Barriers     | Native     |
| ---------------------------------- | ------------ | ---------- |
| 8,000 objects, one synchronous job | 12.8-13.4 ms | 11.0 ms    |
| 800 JSON round trips, one job      | 18.4-18.7 ms | 16.0 ms    |
| A turn of a wait loop (~23 pages)  | 205 µs       | 265 µs     |
| A turn of a KV update              | 713-738 µs   | 717-740 µs |
| A turn building 2,000 objects      | 9.5-9.7 ms   | 9.3 ms     |

Barriers cost compute-bound code 15-20%. Native tracking costs a light turn ~60 µs in faults and scans, a small
part of what Restate spends on a turn. It is the default where available.
Changing the barriers' shape (one 16-bit mark of a page and the next instead
of two marks) saved ~5%; even one unsound mark kept most of their cost.

**Barriers, everywhere else.** Add a second fixed memory, exported as `__durable_dirty_pages`, with one byte per
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

At a safe boundary compare only written pages (marked, or listed by the
kernel) with the parent image (the memory
as of the last commit, which the run keeps beside its live VM, as golden and
the pages that differ from it), treating pages beyond parent length as zeros. Dirty means written; changed means different at
capture. A page written and restored to its original value needs no new
payload. Reset the tracker on restore before rebinding pointers/limits, and
after each commit is stored. A failed commit ends the attempt with its VM.

Since 21 October 2026 the runtime keeps no encoding of its own: a turn writes
its changed pages into the memory file (§5.1), and sqlite-kv compresses each
against what it replaces (its previous image, or golden's), whole pages with
Zstd. The block encoding below, and its measurements, are the history that
design came from.

Selected encoding: **the changed 64-byte blocks of the changed pages,
compressed as one Zstd frame (level 2, long-distance matching) with the
blocks they replace as its prefix** (§5.2), in a chain over a base encoded
against golden (§5.1). Zstd matches each block against what it replaces,
wherever the data now sits: a block a turn changed by a few pointers costs a
few bytes, and so does data that moved, which an XOR against the same
offsets cannot see. Blocks, found by comparing words (eight 8-byte XORs a
block, which the compiler vectorizes), cut what Zstd reads from ~190 KB a
typical turn (its 23 pages and their old contents) to a few KB; with one
compression context per thread, reused, encoding such a turn went from ~370
to ~55 µs, and a wait-loop turn from ~530 to ~205-265 µs. Batches grew 3-7%
(the block lists); a page with 16 or more changed blocks goes whole, so
scattered writes cost no more than whole pages did. Before
comparing, capture zeroes the dead bytes of the pages it compares (§5.3): the
stack below its pointer, where between calls no frame is live, and the free
chunks of the heap. A turn's dead frames, and what it allocated and freed,
would otherwise count as changes: a turn that churns ~100 KiB of strings
writes ~1.5 KiB, against ~70 KiB without.

Measured on 18 October 2026 over traces of seven workloads (40 turns each, 80
for the agent; batch KiB summed over the run's turns, and Zstd time over all
of them):

| Encoding of a turn's changed pages           | steps | kv  | agent | shift | typed | churn | map | Total  | Encode |
| -------------------------------------------- | ----- | --- | ----- | ----- | ----- | ----- | --- | ------ | ------ |
| XOR with previous, Zstd 3 (before)           | 47    | 161 | 198   | 80    | 234   | 3,543 | 740 | 5,006  | 132 ms |
| XOR with golden, Zstd 3 (before the chain)   |       |     |       |       |       |       |     | 12,921 | 279 ms |
| Prefix of previous, Zstd 3                   | 38    | 111 | 138   | 62    | 226   | 2,672 | 862 | 4,111  | 251 ms |
| Prefix of previous, Zstd 3, long matching    | 37    | 109 | 135   | 41    | 66    | 2,692 | 769 | 3,851  | 443 ms |
| Same, stack zeroed                           | 32    | 100 | 116   | 35    | 59    | 2,671 | 762 | 3,777  | 422 ms |
| Same, Zstd 2                                 | 32    | 102 | 115   | 37    | 62    | 2,762 | 827 | 3,939  | 300 ms |
| Same, and free heap chunks zeroed (selected) | 33    | 94  | 109   | 36    | 62    | 2,455 | 524 | 3,315  | 303 ms |

The workloads: a sleep loop (steps), KV reads and writes with an RPC each
(kv), a growing chat history (agent), arrays and strings shifted at the front
(shift), scattered writes to a typed array (typed), a fresh array of 2,000
objects each turn (churn), and a Map of 5,000 entries updated in place (map).
Zstd 19 on the XORs reached 4,015 KiB in 14.6 s; Zstd 1 with a prefix loses
most matches without long matching. Encoding a typical 20-page turn takes
~0.24 ms against ~0.06 ms for the XOR. On a run's bytes in `MemoryStore`, the
steps, kv, agent, typed and churn runs wrote 31%, 38%, 45%, 53% and 35% less.

Reasons and alternatives from the experiment's earlier measurements:

| Candidate                 | Trade-off                                                                                                                                                                                                                  |
| ------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| bsdiff4                   | Smaller heap-churn patches, expensive whole-image matching; historical scalar median ~143 ms, sparse-8-MiB ~1,084 ms among successful samples, with timeouts on larger pairs                                               |
| Page XOR + Zstd           | Exploits stable memory offsets; historical scalar encode ~0.26 ms; ~73 KiB heap-churn patch versus ~25 KiB for bsdiff. Blind to data that moves within or between pages; replaced, above                                   |
| Replacing changed pages   | Simpler, but wastes space on unchanged bytes inside a dirty page                                                                                                                                                           |
| Zstd patch-from, image    | The whole previous image as prefix: barely smaller than the changed pages' previous contents, and indexes the whole memory each turn (~2 ms per MiB)                                                                       |
| Xdelta/VCDIFF, HDiffPatch | General alternatives; no special knowledge of Wasm heaps. The tested Xdelta Wasm port ran out of memory; its bundled native source was measured separately. HDiffPatch was researched, not measured                        |
| Kernel write tracking     | Selected where available (above): asynchronous userfaultfd write protection and PAGEMAP_SCAN need no trap handling; mprotect with a fault handler or soft-dirty bits would need signal coordination or process-wide resets |

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

| Workload | Default   | Collect every turn | Threshold 1 MiB | Threshold 4 MiB or more |
| -------- | --------- | ------------------ | --------------- | ----------------------- |
| steps    | 61 KiB    | 66 KiB             | 61 KiB          | 61 KiB                  |
| kv       | 179 KiB   | 194 KiB            | 179 KiB         | 179 KiB                 |
| agent    | 501 KiB   | 498 KiB            | 487 KiB         | 487 KiB                 |
| map      | 811 KiB   | 812 KiB            | 814 KiB         | 809 KiB                 |
| churn    | 2,473 KiB | 2,475 KiB          | 2,433 KiB       | 2,433 KiB               |
| cycles   | 653 KiB   | 726 KiB            | 822 KiB         | 1,997 KiB (5 MiB heap)  |
| fan      | 626 KiB   | 652 KiB            | 626 KiB         | 626 KiB                 |

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

A scope's store holds one journal, a sqlite-kv namespace
([its format](../../../core/sqlite-kv/docs/FORMAT.md)): the commit record at
`v15/commit`, the values it outgrew at `v15/log/<n>`, and pages in place at
`v15/page/<file>/<n>`. Its files:

| File                | Kind                      | Holds                                                                    |
| ------------------- | ------------------------- | ------------------------------------------------------------------------ |
| `run`               | Record                    | The scope's current run as of its last turn: JSON, below                 |
| `memory`            | Record                    | Its memory: JSON, below                                                  |
| `memory/<golden>`   | Paged file over golden    | The pages of the run's memory that differ from its program's golden one |
| `db`                | Paged file                | SQL: the scope's KV, a table `kv(key TEXT PRIMARY KEY, value TEXT)`      |
| `<extension>`       | Record                    | Each extension's (§9)                                                    |

Programs and their golden images are not stored: the process supplies them
(§5.5). The records, in camelCase JSON:

```text
VmMeta = {stack: i32, runtime: i32, context: i32}
Run = {
    run: run_id, handler: name, program: name, version: u64, runtime: u64,
    turns: count, state: State
}
Memory = {
    meta: VmMeta, memoryLen: byte_count,
    file: "memory/<golden fingerprint, 16 hex digits>",
    pages: [[first, count], ...]
}
```

The image is golden (zeros beyond its length) with the pages `pages` lists
read from `file`. The file is over golden (a sqlite-kv base): a page the run
never changed, or changed back, is a hole that reads as golden's, and the
journal compresses each page against what it replaces (golden's, the first
time it is logged, and in place), so a page costs about what changed in it.
`pages` lists the pages that differ from golden, run-length encoded,
because the store cannot list a file's pages: a restore reads exactly
those. A settled run's list is empty, and its pages are holes. `runtime` is
the runtime's version, a constant bumped by hand with anything an image
depends on: the Wasm, the bridge, the limits, the imports' behavior. A run
restores only on the program version and runtime it started on: a changed
program needs a new name while the old one's runs finish. There is no heap
migration.

The returned View holds the run, program, turns, State, memory length, live
pages, and what the last turn wrote: changed and examined pages, the bytes
its seal logged (memory and tables) and the commit record's.

### 5.2. One file per golden image

The memory file is named after its golden image, and opens only with it
(sqlite-kv refuses another base): its log since the last checkpoint, and its
pages in place, decode only against that image. A new run in the scope
writes holes for the previous run's pages in its first seal, in the
previous run's file (without its golden image: a hole needs none), then its
own pages in its own file. A scope that runs one program reuses one file; a
new program version adds a file, which the previous one's emptied file stays
beside, at no cost but its directory entry.

### 5.3. Capture algorithm

Inputs are golden image G, the parent P (the memory as of the last commit,
kept beside the live VM as G and the pages that differ from it), the live VM
and its tracker. A new run has P=G.

```text
capture(G, P, VM):
    require no active Wasm call or pending host operation future
    read VM metadata first
    require VM.length >= P.length, aligned to 64 KiB, within limit
    changed = []
    for each written page i in ascending order:
        previous = P.page(i), or zeros beyond P.length
        zero VM's dead bytes in page i: below the stack pointer, and
            free heap chunks past their links (§5.6)
        current = VM.page(i)
        if current == previous: continue
        changed.append(i)
        P.page(i) = current
    return changed

prepare(changed):                                 // the memory's part of the seal
    if the scope's previous run left pages: write each as a hole, in its file
    if the run settled: write every page of P that differs from G as a hole
                        pages = []                // no image is kept
    else: write P.page(i) for each i in changed   // the journal makes one equal
                                                  // to G's a hole
          pages = the pages of P that differ from G
    set the record "memory"
```

Count compared pages as examined, actual changes as changed. Zeroing dead
bytes in the VM itself keeps the VM equal to the image it restores from;
golden is captured with all its dead bytes zeroed. Dead bytes in pages not
compared stay as they were, in the VM and in P alike. Updating P with each
changed page keeps it equal to the VM's memory as of the commit: the
`verify-dirty` build checks that every page that differs from it was
written (marked or listed), in either mechanism. A turn writes the pages it
changed, once into the log and once in place at a checkpoint: it never
rewrites the whole image.

### 5.4. Reconstruction algorithm

```text
reconstruct(run, memory):
    require the program's version and the runtime's match the run's
    require golden_length <= memoryLen <= limit, 64-KiB aligned
    memory = golden bytes, extended with zeros to memoryLen
    read the pages memory.pages lists from memory.file, together
        (each from the log, the process's page cache, or its page key)
    write them into memory
    return Image(memory, memory.meta)
```

The process keeps each golden image decompressed in its catalog, shared by
every image made from it: an image holds only the pages that differ. An
attempt reconstructs once, when its first delivery needs the VM; reading a
settled run's commit needs no image.

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

Both engines allocate with wasi-libc's dlmalloc (32-bit). Its heap is
chunks back to back from the first, after the binary's static data
(`heap::QUICKJS`, or found from CPython's `__heap_base`), to the end of
memory: each a 4-byte footer slot
and a 4-byte head (the size, with bit 1 set when the chunk is in use and bit 0
when the one before it is), the last the allocator's top foot. A free chunk
holds its links in its first 32 bytes (footer slot, head, list or tree
links); the rest is dead until an allocation writes it, and `calloc` clears
what it returns.

```text
dead(memory):
    at = first chunk; previous_in_use = true
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

In CPython, without pymalloc, every object is a chunk: a whole walk reads
~100,000 of them, 0.5-1.5 ms a turn. So a VM keeps the chunks its last walk
found (`heap::Chunks`), and the next walk takes them as they were wherever no
page was written: a chunk whose head is on a page not written has its size
still, so the chunk after it starts where it did. The walk reads the heap
only from the last chunk that starts before each run of written pages to the
first one after it whose head was not written, which must have been a chunk
before. Golden's chunks are walked once per program; a restore updates them
with its pages. Memory that grew, or a heap that does not walk as before, is
walked whole. With `verify-dirty`, every partial walk is checked against a
whole one.

## 6. Run algorithms and turn boundaries

### 6.1. Start, delivery and another attempt

`Run::start(key, run, program, handler, input, now)` validates the run ID, input and time,
then reads the scope's commit and applies what follows it (§7). If the commit
is this run's, it is another attempt: require the same program and handler, check its
version and the runtime's, and return the run as committed, without guest
execution and without a VM. Otherwise a new run takes the scope: restore
golden into a new VM, bind run ID, time and seed, queue the handler and run its
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
settled. Foreign-run, noncanonical and never-issued IDs fail. The host also
checks that the guest numbers what it issues `run/<n>`, in order: an ID, and
the idempotency key made from it, names one request on every attempt.

### 6.2. Turn

All native calls below return before the run awaits the store:

```text
begin: start the turn's clock, set logical time
pending = the last commit's pending requests, less those delivered
loop:
    drain all QuickJS jobs
    report = inspect bridge
    if the run settled: break with no pending request
    answered = false
    for op in report.issued, in issue order:
        if op is a KV request:
            get/set/delete: SQL on the kv table, in the turn's transaction
                (begun at its first KV request), so a get sees its writes
            deliver the result, with this turn's logical time
            answered = true
        else: append op to pending              // an effect for the host
    if not answered: break
    if the turn has run past the checkpoint limit (1 s):
        invoke internal pause(), then inspect without draining jobs
        require it issued exactly one Resume; append it to pending
        break
capture (§5.3) and commit (§7)
```

As the guest reports each request once, effects already issued are never
issued again when a sibling finishes. A turn continues through local requests until only
effects remain, the run settles, or it has run past the checkpoint limit:
thousands of sequential local awaits would otherwise run into the turn's
limit, trap, and trap again on every retry. The committed Resume makes the queued continuations schedulable
after a crash.

| Situation                                   | Action                                                               |
| ------------------------------------------- | -------------------------------------------------------------------- |
| KV request                                  | Answer in the turn and continue                                      |
| Store read awaiting I/O                     | Keep the VM and the tracker; no Wasm call is live; do not capture    |
| Effects only, no runnable jobs              | Capture and commit the pending requests                              |
| Past the checkpoint limit                   | Finish the batch, add a Resume, capture before queued jobs run       |
| Root settled                                | Commit the outcome with no image and no pending request; drop the VM |
| Infinite synchronous job or microtask chain | The epoch deadline traps; the run ends failed                        |

Any pending effect can be delivered next, not just a Resume. More than one
Resume can coexist. Once the root settles, the run is final.

## 7. Storage: one commit per turn

A turn is one commit of the scope's database (`Session::seal`), one put of
its record:

1. its KV writes, the SQL transaction its first KV request began;
2. its memory's pages that changed, into the file over golden (§5.3);
3. the run's state, as the record `run`, with the pending requests; the
   memory's, as the record `memory`; and each extension's record that
   changed.

Writes land in any order (§1), so nothing may be its own put: a turn's pages
and a commit that names them could not be ordered. The record carries the
turn's log segments (SQL and memory pages alike) inline, or names them as a
stored value it waits for first (a transaction larger than the inline
budget, 1 KiB here). Past the budget, the record's earlier inline parts are
sealed into one stored value, which the next commit names. A checkpoint
writes every file's logged pages in place, the memory's with the SQL's. A
crash before the record is stored loses the whole turn, which the next
attempt runs again from the previous commit: the requests it issued were
never dispatched (§1). Nothing is applied after a commit.

**SQL again.** Until 18 October 2026 the scope's records lived in SQL tables
through an earlier sqlite-kv; it cost a turn about 2 ms of CPU and wrote
every log segment twice, so it gave way to one commit record of its own
(with `batch/<n>` and `kv/<key>` keys) that relied on ordered writes. With
the journal off, writes are not ordered, and sqlite-kv's one record per
commit, with the host's effects and chain in it, is that commit. Measured on
18 October 2026 (`cargo bench --bench checkpoints`, medians), against that
record of its own:

| Workload    | Live turn, before | Live turn | Restored turn, before | Restored turn | Bytes a turn, before | Bytes a turn |
| ----------- | ----------------: | --------: | --------------------: | ------------: | -------------------: | -----------: |
| scalar      |           0.58 ms |   0.71 ms |               1.38 ms |       2.24 ms |                1,259 |        1,708 |
| sparse-8m   |           2.33 ms |   2.33 ms |              11.37 ms |      11.84 ms |                1,401 |        1,353 |
| growth-3m   |           1.28 ms |   1.68 ms |               4.57 ms |       5.23 ms |                2,140 |        2,096 |
| heap-churn  |           8.03 ms |   7.58 ms |               9.81 ms |      11.37 ms |               53,358 |       53,319 |
| write-heavy |           5.52 ms |   5.31 ms |               6.92 ms |       7.47 ms |                2,342 |        2,301 |

A turn is one or two writes instead of two to four. A restored turn opens
the SQL engine (0.4-1.5 ms; an owner cache keeps it between attempts of one
process), and a small batch is written twice, inline and then sealed. A turn
without KV runs no SQL.

## 8. Worked execution and failure examples

### 8.1. KV mutation, RPC and completion

```js
export async function main(input, ctx) {
  const n = await ctx.kv.get('count');
  await ctx.kv.set('count', n + 1);
  const answer = await ctx.rpc.call({ service: 'Worker', handler: 'double' }, n + 1);
  await ctx.kv.set('lastAnswer', answer);
  return answer;
}
```

With count=41:

| Turn                                | The record's put                                             | Effect R                    |
| ----------------------------------- | ------------------------------------------------------------ | --------------------------- |
| T1: execute until the RPC wait      | count=42 in SQL, the base batch, the run (R pending)         | Dispatched after the commit |
| T2: deliver R's result 84, continue | lastAnswer=84 in SQL, the chain cleared, the run (completed) | Done                        |

A crash before T1's record is stored leaves nothing: the next attempt starts
the run again, and R was never dispatched. After it, count=42 is stored with
it. After dispatch, the next attempt waits on R again under its stable ID:
Restate attaches to the call already made. After T2's record, a repeated
delivery is a no-op and another attempt returns the outcome; the base batch,
stored or not, is garbage the next commit deletes.

### 8.2. Fan-out

For `Promise.all([rpcA(), rpcB()])`, the first commit has both pending.
Delivering B commits a turn that keeps A pending, and A is not issued again:
the guest reported it once. Delivering A then returns values in array order. For
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
- Each exported function is an exclusive handler of the object, registered
  from the program at startup (a manifest of its exports, not a macro). A run
  is one invocation of a handler, which drives its
  turns itself, on one live VM. Its store is the object's state, remembering
  every key it read or wrote for the attempt: a key is read from Restate at
  most once. After each turn commits, the handler makes the pending
  requests and waits for the first to complete: an RPC as a call whose
  idempotency key is the program's own or else the operation ID, a timer as a
  sleep until the deadline, and a host binding's call (`ctx.call(name,
  input)`, [HOST.md](HOST.md) §6) made by the handler itself, raced with
  Restate's futures; Resume is delivered at once. A call's input is read from
  the heap as the call is made. The completion is the next turn's event, with every other completion
  already arrived (polled once, without waiting): what arrives together
  commits once. A run of `Promise.all` of 10 calls took 3-10 turns instead
  of 11 against a local server. Uncommitted IDs are never
  waited on, so their reuse is safe (§2.2).
- A retried attempt restores the last committed image and makes its pending
  requests again under the same keys, reading their inputs from the restored
  heap: Restate attaches a re-issued call to the original invocation, within
  the callee's idempotency retention, and a binding that does not deduplicate
  is called again. Nothing is replayed, and the host keeps no results.
- A handler returns once its function settles, when the run ends (§2.2): requests still
  outstanding are dropped, and a call already made finishes at its callee
  unobserved.

An implementation can be built and accepted in this order:

1. Pin the reactor, bind imports and implement golden creation plus full-memory
   cold restore. Validate pointer/stack restoration, closures, cycles, maps,
   pending promises and queued jobs before optimizing capture.
2. Implement the JSON bridge and the turn over a store in memory. Add stable
   IDs, duplicate handling, and an unusable run after an error.
3. Add the memory's pages to the journal (a file over golden), and growth.
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

`make test-functions` runs the runtime's tests with `verify-dirty` (every changed page
must have been marked) and again with `full-scan`, then the programs against a
Restate server, including a crash between a call's commit and its answer.

## 10. Measurements

`cargo bench --bench checkpoints` (and `--features full-scan`, the control)
runs a few workloads over an in-memory store, once with the VM live and once
restoring it every turn, and writes its results beside it as JSON.

Against Restate, measured on 18 October 2026 (4 vCPUs, one restate-server
1.7.13), with the same workloads as regular virtual objects of the TypeScript
SDK 1.17.3 on Node 22:

| Workload                                      | TypeScript       | QuickJS          |
| --------------------------------------------- | ---------------- | ---------------- |
| KV read and write, a new key each, 32 at once | 473/s, p50 62 ms | 362/s, p50 83 ms |
| KV read and write, one key, one at a time     | p50 10 ms        | p50 13 ms        |
| 20 waits on a 1 ms timer, one run             | 115 ms           | 95 ms            |
| 5 waits, 32 runs at once                      | 175/s            | 134/s            |
| `Promise.all` of 10 calls, 8 at once          | 38/s             | 28/s             |
| Primes below 100,000                          | 47 ms            | 519 ms           |
| Written to Restate by a run of 20 waits       | ~20 KiB          | ~53 KiB          |

Rows vary by ~10% from run to run, and both columns from day to day: these
were re-measured after delivering what arrives together in one turn,
starting VMs from a pool (§3.2) and the leaner bridge (§2.2), when the same
rows had read 484/s and 307/s, 8 and 14 ms, 82 and 93 ms, 220/s and 155/s,
47/s and 27/s, 53 and 488 ms, ~21 and ~70 KiB. Before the commit record and the live VM,
the same rows were 220/s, 64 ms, 1.36 s, 40/s, 8.3/s, 1.8 s and 1.1 MiB;
before the chain of batches, the run of 20 waits wrote 137 KiB, and ~70
KiB before compressing pages against their previous contents (§4) and
zeroing dead bytes; the throughput rows did not move with those (an A/B of
both binaries on one server). Most of what is left is Restate's: a commit
record and the journal per turn. The Rust
SDK's server now sets `TCP_NODELAY`, which saved ~40 ms per round trip. One
synchronous JavaScript job is capped by time (10 minutes by default), and the
interpreter runs compute-bound code ~10x slower than V8.

### 10.1. A long coding session

`cargo bench --bench agent` runs the shipped `coder` program through a
scripted session over an in-memory store, to the program's own limit of 200
steps. Each step is a few hundred bytes of model text and one tool call: a
file read (~6 KiB), a command (~1.5 KiB of output), an edit, or a written file
(~3 KiB). The model is a conversation of the host's gateway
([LLM.md](LLM.md)): the program sends each step's new messages, which go
into the scope's database, and the host reads the whole conversation from
there for each request. Measured on 20 October 2026 (4 vCPUs):

| Step | Request to the model | Reading it | Stored by the step | Model turn | Tool turn |
| ---: | -------------------: | ---------: | -----------------: | ---------: | --------: |
|    1 |              1.7 KiB |    0.57 ms |             9.3 KB |    1.59 ms |   1.33 ms |
|   10 |               31 KiB |    0.53 ms |              16 KB |    1.50 ms |   1.22 ms |
|   41 |              136 KiB |    1.02 ms |             9.7 KB |    1.02 ms |   1.37 ms |
|   98 |              327 KiB |    1.91 ms |              16 KB |    1.61 ms |   1.03 ms |
|  190 |              636 KiB |    3.56 ms |              16 KB |    1.76 ms |   1.09 ms |

- **Per step:** 10 KB at the median, 9–33 KB for nine steps in ten, whatever
  the conversation's length: the rows of its new messages, a heap batch of
  2–4 KB (the loop, the latest response) and a commit record of ~365 B.
- **Checkpoints:** every 16 steps or so, the database writes its pages in
  place: 31–41 KB, flat. The heap, 1.5 MiB, needs no new base.
- **The session:** 2.5 MiB stored over 200 steps. Neither the heap nor
  Restate's journal holds the requests.
- **Restores:** a new attempt, every 25 steps, restores the run in 1–19 ms,
  reopening the database.

## 11. Scope limits, provenance and references

There is no heap migration, no concurrent writers in a scope, no generic Wasm
snapshot and no dynamic native extension. A settled run keeps only its commit
record, with its outcome. Snapshot memory limits are not whole-process memory
limits. Nothing preempts a single long JavaScript job but the turn's time
limit; meanwhile it yields its thread to the executor at every 10 ms tick.

Applications are cooperative: the internal bridge and JavaScript built-ins are
not hardened against tampering. There is no guest filesystem or module-loader
API, and the network is `fetch`, under the deployment's policy
([FETCH.md](FETCH.md)); RPC routing and authorization belong to the embedding. A non-idempotent remote
provider still needs a transport recovery/deduplication contract.

The pinned QuickJS binary's SHA-256 is
`d4c9375f2b1ca4dc95f72c8aa2982a7a9951ac8011490d79c6582df732b4bbd9`,
for provenance only; nothing checks it at runtime. It comes
from the published quickjs-wasi 3.6.2 package, not a claimed reproducible source
build; the package does not supply a complete toolchain/submodule manifest.
Retain `assets/quickjs-wasi-LICENSE`, `quickjs-ng-LICENSE`, `wasi-libc-LICENSE`
and component notices in `assets/licenses/`. Replacing the binary requires
ABI review, cold-restore validation, a new runtime version, and its heap's
first chunk as `heap::QUICKJS` (§5.6).
Which parts are QuickJS's, what another engine must provide, and the
candidates: [ENGINES.md](ENGINES.md).

Supporting sources for the already selected mechanisms:

- [Vercel driver at quickjs-wasi 3.6.2](https://github.com/vercel-labs/quickjs-wasi/blob/quickjs-wasi%403.6.2/src/index.ts), [C ABI](https://github.com/vercel-labs/quickjs-wasi/blob/quickjs-wasi%403.6.2/c/interface.c), [extensions](https://github.com/vercel-labs/quickjs-wasi/blob/quickjs-wasi%403.6.2/src/extensions.ts).
- [Zstd patching](https://github.com/facebook/zstd/wiki/Zstandard-as-a-patching-engine), [bsdiff](https://www.daemonology.net/bsdiff/), [Xdelta](https://jmacd.github.io/xdelta/), [HDiffPatch](https://github.com/sisong/HDiffPatch).
- [Wasmtime COW initialization](https://docs.wasmtime.dev/examples-fast-instantiation.html), [LinearMemory allocator](https://docs.wasmtime.dev/api/wasmtime/trait.LinearMemory.html), [Wizer](https://github.com/bytecodealliance/wizer).
- [Linux pagemap/PAGEMAP_SCAN](https://docs.kernel.org/admin-guide/mm/pagemap.html), [userfaultfd](https://docs.kernel.org/admin-guide/mm/userfaultfd.html), [soft-dirty](https://docs.kernel.org/admin-guide/mm/soft-dirty.html).
- [DFINITY memory tracker precedent](https://github.com/dfinity/ic/blob/3633026472367a413912bf797490c9336ba762f5/rs/memory_tracker/src/lib.rs).
