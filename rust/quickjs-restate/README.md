# Durable JavaScript in Restate objects

JavaScript that waits on Restate (RPCs and timers) without a journal to
replay: [the durable QuickJS runtime](../../core/quickjs-durable/README.md)
in virtual objects, on [the patched Rust SDK](../sdk/README.md). At each wait,
a run's memory is saved as the pages that changed; a result restores it and
carries on.

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
make restate-server qjs-restate             # from the repository's root
target/restate/restate-server &
target/release/quickjs-restate &             # the services, on PORT (9080)
curl localhost:9070/deployments --json '{"uri": "http://localhost:9080"}'

# app.js: the application above
curl localhost:8080/JsScope/alice/register --json "$(jq -n --rawfile source app.js '{id: "counter", source: $source}')"
curl localhost:8080/Js/run --json '{"scope": "alice", "app": "counter", "input": 20}'
```

`Js/run` returns the run's outcome: what `main` returns, or its error as a
terminal error. Bind the services into an endpoint of your own, beside the
services the JavaScript calls, with `quickjs_restate::bind(Endpoint::builder())`.

## How it works

| Service            | Handler                         | What                                                                                                     |
| ------------------ | ------------------------------- | -------------------------------------------------------------------------------------------------------- |
| `JsScope` (object) | `register({id, source})`        | An application version, in this scope (the object's key). Immutable: the same ID with other source fails |
|                    | `start({run, app, input})`      | A turn: start a run                                                                                      |
|                    | `complete({run, operation, …})` | A turn: deliver a request's result (private)                                                             |
| `Js` (service)     | `run({scope, app, input})`      | Start a run, its ID this invocation's, and wait for its outcome, a signal                                |
|                    | `call({…})`                     | A run's RPC: call the target, and send its outcome to the scope's `complete` (private)                   |

A scope's applications, its runs' images and their `ctx.kv` are one SQL
database ([sqlite-kv](../../core/sqlite-kv/README.md)) in the object's state.
A **turn** is one invocation of `start` or `complete`, without journal replay:
it restores the run's image, delivers the event, runs the JavaScript until it
waits, and commits the next image, the KV writes and the new requests in one
save. A retried attempt runs the turn again against the state as stored, and
a result already delivered does nothing more.

After the commit, the turn dispatches the run's new requests, the **outbox**,
each with its operation ID as idempotency key: an RPC through `Js/call`, a
timer as a delayed send to `complete`. It then records what it dispatched, in
the object's state; Restate stores state writes in order, so once the record
is stored, so are the sends. A turn whose attempt crashed between its commit
and that record dispatches the rest on the retry. Once `main` settles, the
turn signals the outcome to `Js/run`; requests still outstanding (a race's
loser) still complete, and run their continuations, before the run ends.

`make test-qjs-restate` runs both against a real Restate server: RPCs,
timers, KV shared by the runs of a scope, a failing application, and turns
that crash after their commit (`quickjs_restate::faults`).

## Not yet

- `durable.call(name)`, the runtime's host operation, fails: call a service
  with `ctx.rpc.call`.
- A run is not cancelled, and a `Js/run` that gives up leaves it running.
- The engine runs on the async runtime's threads: a long turn holds one.
