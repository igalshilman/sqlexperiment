# Engines: what is QuickJS's, and what is not

The runtime needs one thing from a language's engine: a WebAssembly module
whose whole state lives in its linear memory. Saving that memory at each wait
saves the program. Most of the crate knows nothing more about the engine.
Each engine is one driver file, a bridge and the binary itself. Two run
today: QuickJS for JavaScript, and CPython for Python ([PYTHON.md](PYTHON.md)).

## The layers

| Layer                           | Files                                               | Knows about                                                                                                                                                                                                                                          |
| ------------------------------- | --------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Restate binding, turns, commits | `restate.rs`, `run.rs`, `store.rs`, `model.rs`      | Requests, outcomes and images (byte arrays). No engine.                                                                                                                                                                                              |
| Write tracking, encoding        | `tracking.rs`, `instrument.rs`, `codec.rs`          | One 32-bit Wasm linear memory. Instrumented stores need a restricted Wasm feature set (no SIMD, atomics, memory64).                                                                                                                                  |
| VMs                             | `vm.rs`                                             | Instances of a module, their memory and stack pointer: start, restore, track, capture. Which driver a program's language takes.                                                                                                                      |
| Bridges                         | `assets/guest.js`, `assets/guest.py`                | The language's own: `ctx`, promises or asyncio tasks, a seeded random. Both speak one protocol to the run loop (`handlers`, `init`, `start`, `deliver`, `inspect`, `pause`).                                                                         |
| Programs                        | `examples-functions/`, `examples-functions-python/` | Standard JavaScript, without Node.js APIs; Python with its standard library.                                                                                                                                                                         |
| **Engine drivers**              | `quickjs.rs`, `python.rs`                           | **Each reactor's ABI.** QuickJS's `qjs_*` exports evaluate and call, drain jobs and read strings, with its init flags, limits, runtime and context pointers. CPython's `py_*` exports start it, run the bridge and call it ([PYTHON.md](PYTHON.md)). |
| **System interface**            | `quickjs.rs`, `wasi.rs`                             | **What each engine imports.** QuickJS needs a clock, randomness and its `env` hooks. CPython also needs a filesystem to import its standard library from: a read-only tree in memory.                                                                |
| **Dead heap bytes**             | `heap.rs`                                           | **wasi-libc's dlmalloc**, which both binaries allocate with, and where each heap starts. Optional: without it, images still restore but also carry freed memory.                                                                                     |
| **The engines**                 | `assets/quickjs.wasm`, `target/pywasm/out`          | quickjs-wasi 3.6.2 (quickjs-ng inside); CPython 3.15 for WASI, built by `make python-engine`.                                                                                                                                                        |

## What a replacement engine must provide

1. **WebAssembly, with its state in one memory.** One 32-bit linear memory
   plus `__stack_pointer`, and nothing else that changes at runtime: no other
   mutable globals and no table writes. `validate_snapshot_abi` checks this.
2. **No JIT.** Code that Wasm generates at runtime would have to become new
   modules, outside the memory we save. The engine interprets, or the
   program is compiled ahead of time.
3. **Deterministic startup.** Every process must build the same golden image
   from a program's source, so the host controls the clock and randomness.
   After that, nothing needs to be deterministic. Memory is saved, not
   replayed, so GC timing or hash seeds do not matter.
4. **A small call surface:** evaluate a script, call a global function with
   a string, run pending jobs until there are none, and read a string back.
5. **Quiet between jobs.** The runtime saves memory only when no JavaScript
   frame is on the stack, so the engine must not keep state outside the job
   queue that a restore would miss: threads, native handles, host callbacks.

Adding an engine means a driver of a few hundred lines, a bridge in the
engine's language, the imports it needs, a Wasm binary and a bump of
`VERSION`. Tracking, encoding, the run loop and the binding stay as they are:
that is how Python came in.

## Candidates

| Engine                                                               | What it is                                                                                            | Fits                                                                              | Compute speed                                                                                                                                                                                                                                         |
| -------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| QuickJS (today)                                                      | Small C interpreter, built for Wasm by quickjs-wasi                                                   | Yes                                                                               | The baseline: ~11× slower than V8 on primes                                                                                                                                                                                                           |
| CPython (today, for Python)                                          | The reference Python interpreter, built for WASI by its own build scripts                             | Yes: [PYTHON.md](PYTHON.md)                                                       | Interpreted, like QuickJS; an 11 MB image to QuickJS's 1.4 MB, so restores cost more                                                                                                                                                                  |
| Our own QuickJS build                                                | The same engine, compiled by us                                                                       | Yes                                                                               | About the same. We would choose the allocator, the flags and the exports, and drop the pinned heap start                                                                                                                                              |
| [Javy](https://github.com/bytecodealliance/javy)                     | QuickJS in Wasm, from Shopify, now with the Bytecode Alliance                                         | Yes, with a new driver                                                            | About the same: the same interpreter                                                                                                                                                                                                                  |
| [LLRT](https://github.com/awslabs/llrt)                              | Amazon's native runtime for Lambda cold starts: QuickJS (via rquickjs) plus Node-like modules in Rust | Not as it stands: a native process on Tokio, not one Wasm memory                  | About the same: the same interpreter, no JIT                                                                                                                                                                                                          |
| [StarlingMonkey](https://github.com/bytecodealliance/StarlingMonkey) | SpiderMonkey (Firefox's engine) built for Wasm, as Fastly Compute uses it                             | Likely, with work: a Wasm component, not a reactor                                | Interpreted, like QuickJS. With [weval](https://github.com/bytecodealliance/weval), which compiles the program's bytecode ahead of time, its authors report 3–5× over that interpreter. A larger heap means bigger golden images and slower VM starts |
| [Porffor](https://github.com/CanadaHonk/porffor)                     | Compiles JavaScript ahead of time to Wasm                                                             | Not yet: a research project, with partial language support and known Promise bugs | Potentially the fastest here, once complete                                                                                                                                                                                                           |
| V8 / Node.js                                                         | What the TypeScript SDK runs on                                                                       | No: a JIT's machine code cannot live in a Wasm memory                             | The reference. Durability there means journal replay, which is what the Restate SDKs do                                                                                                                                                               |

LLRT is the wrong lever for speed. It runs the same interpreter we do, and
it is fast to _start_ as a native process, which we already avoid by
starting VMs from a shared golden image. What it adds is its Node-compatible
modules (`Buffer`, `crypto`, `fetch`, ...). Those would need a Wasm build,
and here all I/O goes through `ctx` anyway.

## Where the gaps to TypeScript come from

**Compute (primes: 519 ms against 47 ms).** This gap is the interpreter: V8
compiles hot loops to optimized machine code, and QuickJS interprets them
inside Wasm. Only ahead-of-time compilation narrows it: StarlingMonkey with
weval today, Porffor some day. Neither reaches V8. For a heavy step, call a
service written in anything with `ctx.rpc.call`. The program waits on it
like on any call, and its result is saved like any other.

**Throughput (KV: 362/s against 473/s; `Promise.all`: 28/s against 38/s).**
This gap is not the interpreter: the JavaScript in these runs takes
microseconds. Our runtime's whole cost for a one-turn run is ~0.8 ms of CPU
(start the VM, run, capture, encode; DESIGN §3.2). On the benchmark's 4 vCPUs,
which the server shares, 473 runs/s is ~8.5 ms of CPU per run and 362/s is
~11 ms. Most of the ~2.6 ms difference is the server's: besides the KV write
itself, each turn stores a batch of pages and a commit record. A faster
engine would not move these rows. Writing less per turn, or fewer records,
would.
