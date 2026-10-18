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

| Handler of `<program>/<scope>`      | What                                                                       |
| ----------------------------------- | -------------------------------------------------------------------------- |
| `run(input)`, shared                | Start a run, its ID this invocation's, and wait for its outcome, a signal  |
| `start({run, input})`               | A turn: start a run                                                        |
| `complete({run, operation, …})`     | A turn: deliver a request's result (private)                               |
| `call({run, operation, …})`, shared | A run's RPC: call the target, and send its outcome to `complete` (private) |

A scope's runs' images and their `ctx.kv` are one SQL database
([sqlite-kv](../../core/sqlite-kv/README.md)) in the object's state. A
**turn** is one invocation of `start` or `complete`, without journal replay:
it restores the run's image, delivers the event, runs the JavaScript until it
waits, and commits the next image, the KV writes and the new requests in one
save. A retried attempt runs the turn again against the state as stored, and
a result already delivered does nothing more.

After the commit, the turn dispatches the run's new requests, the **outbox**,
each with its operation ID as idempotency key: an RPC through `call`, a timer
as a delayed send to `complete`. It then records what it dispatched, in the
object's state; Restate stores state writes in order, so once the record is
stored, so are the sends. A turn whose attempt crashed between its commit and
that record dispatches the rest on the retry. Once `main` settles, the turn
signals the outcome to `run`; requests still outstanding (a race's loser)
still complete, and run their continuations, before the run ends.

`make test-qjs-restate` runs both against a real Restate server: RPCs,
timers, KV shared by the runs of a scope, a failing program, and turns that
crash after their commit (`quickjs_restate::faults`).

## Not yet

- `durable.call(name)`, the runtime's host operation, fails: call a service
  with `ctx.rpc.call`.
- A run is not cancelled, and a `run` that gives up leaves it running.
- The engine runs on the async runtime's threads: a long turn holds one.
