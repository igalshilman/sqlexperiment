# Durable JavaScript in Restate objects

JavaScript that waits on Restate (RPCs and timers) without a journal to
replay: [the durable QuickJS runtime](../../core/quickjs-durable/README.md)
in virtual objects, on [the patched Rust SDK](../sdk/README.md). At each wait,
a run's memory is saved as the pages that changed; a result restores it and
carries on.

A program is a static file, [`programs/counter.js`](programs/counter.js):

```js
async function main(input, ctx) {
  const n = ((await ctx.kv.get('count')) ?? 0) + input;
  await ctx.kv.set('count', n);
  await ctx.sleep(1000);
  return n;
}
```

`ctx.kv` is the scope's key/value state; `ctx.rpc.call({ service, handler, key }, input)`
calls any Restate service or object, and `ctx.sleep(ms)` waits on a Restate
timer.

## Run it

```sh
make restate-server qjs-restate              # from the repository's root
target/restate/restate-server &
target/release/quickjs-restate &             # serves each program, on PORT (9080)
curl localhost:9070/deployments --json '{"uri": "http://localhost:9080"}'

curl localhost:8080/counter/alice/run --json 20    # 20, a second later
curl localhost:8080/counter/alice/run --json 1     # 21
```

Each program is a virtual object of its name, and its key a scope: the runs
of `counter/alice` share their `ctx.kv`, and `counter/bob` has its own. `run`
takes the input as its body, and returns the run's outcome: what `main`
returns, or its error as a terminal error.

## Programs

The binary is built with its programs: each `<name>.js` in `QJS_PROGRAMS`
(by default [`programs/`](programs)), and its golden dump `<name>.golden`
beside it, the memory image of the evaluated source that every run starts
from and every checkpoint is relative to. `make goldens` computes the dumps
(quickjs-durable's `golden` binary), again when a source or the runtime
changes; `make qjs-restate` does it first. A dump is valid for one source and
one build of the runtime, and the binary refuses to start with a stale one.

A run keeps the version of its program it started on: once a program
changes, its runs from before fail to restore. Give a new version a new name
(`counter-v2.js`) while the old one's runs finish.

Bind programs into an endpoint of your own, beside the services they call,
with `quickjs_restate::bind(Endpoint::builder(), &programs)`: the embedded
ones (`quickjs_restate::embedded()`), or a `quickjs_durable::Programs` you
compile or load.

## How it works

`<program>/<scope>/run` is an exclusive handler without journal replay, and
it runs the program itself. The JavaScript runs until it waits on a request
the handler cannot answer at once; the handler commits the run's image, its
KV writes and its requests in one save of the scope's database
([sqlite-kv](../../core/sqlite-kv/README.md), in the object's state): a
**turn**. Then it waits on Restate for the first of those requests to
complete, an RPC (`ctx.rpc.call`, a call with the operation's ID as
idempotency key) or a timer (`ctx.sleep`, until the request's deadline), and
delivers it in the next turn. `ctx.kv` is answered within the turn.

A retried attempt replays nothing: it restores the last committed image and
waits again on its requests, under the same idempotency keys, so Restate
attaches it to a call made before the crash instead of making it again, and a
timer waits only for what is left.

The run ends when `main` settles, and `run` returns. Unlike a Node.js process,
it does not wait for its other promises: requests still outstanding (a race's
loser, a promise nobody awaited) are dropped, and their continuations never
run. A call already made finishes at its callee, unobserved. Await what must
happen.

Runs of one scope run one after the other, as invocations of an object's
exclusive handlers do: a run waiting on a timer holds its scope. Give runs
that must overlap keys of their own.

`make test-qjs-restate` runs it against a real Restate server: RPCs, timers,
KV shared by the runs of a scope, a failing program, a race whose loser is
dropped, and an attempt that crashes while its call runs, whose
retry attaches to the call (`quickjs_restate::faults`).

## Not yet

- `durable.call(name)`, the runtime's host operation, fails: call a service
  with `ctx.rpc.call`.
- A re-issued call attaches only within the callee's idempotency retention
  (24 hours by default): a run waiting longer on a call that its attempt lost
  would call again.
- A cancelled `run` ends its run where it is.
- The engine runs on the async runtime's threads: a long turn holds one.
