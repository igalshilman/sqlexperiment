# Durable JavaScript in Restate objects

JavaScript that waits on Restate (RPCs, timers) for as long as it takes, and
survives crashes without a journal to replay. At each wait the JavaScript
VM's memory is saved; a crash or a result restores it, and the program
carries on where it was.

```js
// programs/counter.js
async function main(input, ctx) {
  const n = ((await ctx.kv.get('count')) ?? 0) + input;
  await ctx.kv.set('count', n);
  await ctx.sleep(1000);
  return n;
}
```

```sh
curl localhost:8080/counter/alice/run --json 20    # 20, a second later
curl localhost:8080/counter/alice/run --json 1     # 21
```

## Run it

```sh
make restate-server qjs-restate              # from the repository's root
target/restate/restate-server &
target/release/quickjs-restate &             # serves programs/, on PORT (9080)
curl localhost:9070/deployments --json '{"uri": "http://localhost:9080"}'
```

A program is a file in [`programs/`](programs): `<name>.js`, defining
`async function main(input, ctx)`. `cargo build` ships it in the binary, and
the binary serves it as the virtual object `<name>`. The object's key is a
**scope**: the runs of `counter/alice` share their `ctx.kv`; `counter/bob`
has its own.

| Program                          | Shows                                                                       |
| -------------------------------- | --------------------------------------------------------------------------- |
| [`counter`](programs/counter.js) | KV state and a timer                                                        |
| [`memory`](programs/memory.js)   | A class instance, a closure and a paused generator surviving waits          |
| [`fanout`](programs/fanout.js)   | Programs calling programs, in parallel: `Promise.all` over RPCs             |
| [`timeout`](programs/timeout.js) | A deadline: `Promise.race` of a [`slow`](programs/slow.js) call and a timer |
| [`primes`](programs/primes.js)   | A long computation, carried over many turns                                 |

## Writing a program

| `ctx`                                                 | What                                        |
| ----------------------------------------------------- | ------------------------------------------- |
| `ctx.kv.get(key)`, `.set(key, value)`, `.delete(key)` | The scope's state: JSON values              |
| `ctx.rpc.call({ service, handler, key? }, input)`     | Call any Restate service, object or program |
| `ctx.sleep(ms)`                                       | A durable timer                             |

- **Values are JSON**: inputs, outputs, KV values and RPC payloads.
- **I/O goes through `ctx`**: no `fetch`, timers or modules. Call a Restate
  service for the rest of the world (a model, an API).
- **`Date.now()` and `Math.random()` are the run's**: time is the host's at
  each turn, and the random generator is seeded per run and saved with it.
- **The run ends when `main` settles.** Unlike in Node.js, promises still
  pending then (a race's loser, an RPC nobody awaited) are dropped, and their
  callbacks never run. Await what must happen.
- **Runs of one scope run one at a time**, as an object's exclusive handlers
  do: a run waiting on a timer holds its scope. Runs that must overlap need
  scopes of their own.
- **A changed program is a new program.** A run restores only with the source
  it started on: ship the new version under a new name while old runs finish.

## How it works

The JavaScript engine is [QuickJS](https://bellard.org/quickjs/), compiled to
WebAssembly and run by Wasmtime. A WebAssembly instance's whole state is one
linear memory and a stack pointer, so the VM, heap, closures, pending
promises and all, is a byte array. [The durable QuickJS
runtime](../../core/quickjs-durable/README.md) saves that array; this crate
binds it to Restate.

**A run is one invocation** of `<program>/<scope>/run`, an exclusive handler
without journal replay. It is a loop of **turns**:

```
restore the image ─▶ deliver one result ─▶ run the JavaScript until it waits
     ▲                                                       │
     │                                                       ▼
 wait on Restate for the first          commit: the image's changed pages, the KV
 pending request to complete    ◀────── writes and the new requests, in one save
```

Within a turn, `ctx.kv` is answered at once from the scope's SQL database;
an RPC or a timer is a request the handler waits on, on Restate itself: a
call (with the request's ID as idempotency key) or a sleep until its deadline.

### Snapshots: the key choices

- **Snapshot only at a wait.** A turn ends when the JavaScript has run all it
  can and waits on a promise: no JavaScript or native frame is on the stack,
  so memory and the stack pointer are the whole state. There is no
  mid-function capture, and no OS-level process snapshot.
- **Every run starts from a golden image.** At build time, each program is
  evaluated once (QuickJS initialized, the bridge and the program loaded) and
  its memory dumped into the binary. A run starts from a copy of it, so the
  startup work is never repeated, and every snapshot is relative to it.
- **Save only the pages that changed.** The Wasm is rewritten once, at
  startup, so that every store also marks its 4 KiB page in a side table. At
  the end of a turn, only marked pages are compared, and only those that
  changed are saved: a turn that touches a few objects writes a few pages, not
  the whole heap.
- **Pages relative to golden, not to the last turn.** A saved page is XORed
  against its golden page, and a turn's pages are compressed together (Zstd)
  into one batch. Each snapshot's manifest maps every page that differs from
  golden to the batch holding it, so a restore is golden plus one lookup per
  page, never a chain of diffs. Past 16 referenced batches, a turn rewrites
  every live page into its own, so restores stay bounded however long a run
  lives.
- **The snapshot commits with what it did.** The image, the KV writes and the
  new requests are one transaction of the scope's database, in the object's
  state ([sqlite-kv](../../core/sqlite-kv/README.md)): a turn happened
  entirely or not at all.
- **Nothing is replayed.** A retried attempt restores the last committed
  image and waits again on its pending requests, under the same idempotency
  keys, so Restate attaches it to a call already made instead of making it
  again, and a timer waits only for what is left.
- **A turn has a CPU budget.** Execution is metered (Wasmtime fuel); past half
  the budget, the run is saved at its next wait and continues in a new turn,
  so a long computation is never cut off. A single job that exceeds the whole
  budget (an endless loop) ends the run as failed.

With the runtime's SQLite host, a turn (cold restore, execution, capture and
commit) takes about 3 ms for a program of a 1.3 MiB image, and about 18 ms
for one of 9 MiB ([measurements](../../core/quickjs-durable/docs/DESIGN.md#11-measurements-and-reproduction)).

## Tests

`make test-qjs-restate` runs the binding against a real Restate server: RPCs,
timers, scopes, a failing program, a race whose loser is dropped, an attempt
that crashes while its call runs (its retry attaches to the call, and the
callee runs once), and the shipped programs.

## Not yet

- A re-issued call attaches only within the callee's idempotency retention
  (24 hours by default): a run waiting longer on a call its attempt lost
  would make it again.
- A cancelled run ends where it is.
- A turn runs on the async runtime's threads: a long one holds a thread.
