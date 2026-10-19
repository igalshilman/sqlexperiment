# Durable functions

Plain JavaScript modules that survive anything: a crash, a restart, a wait
of a month. Each file `<name>.js` is a Restate virtual object `<name>`, and
each function it exports is a handler. While a call waits on a timer,
another service or a model, its memory (variables, closures, loops in
progress) is saved; when the result comes, or after a crash, it carries on
where it stopped. Nothing is replayed.

```js
// counter.js: the virtual object `counter`, with handlers `add` and `get`
export async function add(n, ctx) {
  const count = ((await ctx.kv.get('count')) ?? 0) + n;
  await ctx.kv.set('count', count);
  await ctx.sleep(1000); // a durable timer: a crash meanwhile loses nothing
  return count;
}

export async function get(_, ctx) {
  return (await ctx.kv.get('count')) ?? 0;
}
```

## Run them

```sh
make qjs                                                                     # from the repository's root
ANTHROPIC_API_KEY=... target/release/quickjs-durable                         # every function here, on port 9080
npx @restatedev/restate-server  # Restate, in another terminal
curl localhost:9070/deployments --json '{"uri": "http://localhost:9080"}'
curl localhost:8080/counter/alice/add --json 20                              # 20, a second later
curl localhost:8080/counter/alice/get                                        # 20; bob has a count of his own
```

The object's key (`alice`) names a scope: calls under the same key share its
SQL database and KV, and run one at a time. Each call starts from the module
as evaluated: what lasts between calls is in `ctx.db` and `ctx.kv`. Each
file's header shows how to call it, and Restate's UI lists every handler.

| File                                 | What                                                                                                     |
| ------------------------------------ | -------------------------------------------------------------------------------------------------------- |
| [`counter.js`](counter.js)           | **Start here.** A count per key, and a durable one-second timer                                          |
| [`notes.js`](notes.js)               | Notes in SQL, with `ctx.db.sql`                                                                          |
| [`memory.js`](memory.js)             | A class instance, a closure and a paused generator, all intact after a wait                              |
| [`checkout.js`](checkout.js)         | A saga: reserve stock ([`inventory`](inventory.js)), charge ([`payments`](payments.js)), undo on failure |
| [`subscription.js`](subscription.js) | Charge every period, on durable timers, until a charge is declined                                       |
| [`retry.js`](retry.js)               | Retries with exponential backoff, against [`flaky`](flaky.js)                                            |
| [`timeout.js`](timeout.js)           | A deadline: race a [`slow`](slow.js) call against a timer                                                |
| [`fanout.js`](fanout.js)             | Add to `counter` under several keys at once, and wait for all                                            |
| [`primes.js`](primes.js)             | A long computation, carried across turns                                                                 |
| [`assistant.js`](assistant.js)       | An AI agent (Claude) with the shop as its tools                                                          |
| [`coder.js`](coder.js)               | A coding agent with a shell and files, in a workspace that outlives the run                              |

## Writing a function

Export a function `(input, ctx)`; it may be async. The input and the result
are JSON (no input is null). `ctx` is all it needs from outside:

| Call                                              | What                                                                                                                                                                    |
| ------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| ``ctx.db.sql`SELECT ... ${value}` ``              | The scope's SQL database: rows as objects. `${}` values are parameters, never SQL text. Synchronous.                                                                    |
| `ctx.db.transaction(fn)`                          | Run `fn`'s statements together, or undo them if it throws                                                                                                               |
| `ctx.kv.get/set/delete(key)`                      | The scope's JSON values                                                                                                                                                 |
| `ctx.rpc.call({ service, handler, key? }, input)` | Any Restate service, including the other functions here. Models too: `claude/messages` takes a [Messages API](https://platform.claude.com/docs/en/api/messages) request |
| `ctx.sleep(ms)`                                   | A durable timer, however long                                                                                                                                           |
| `ctx.key`                                         | The object's key                                                                                                                                                        |

- It is standard JavaScript, without Node.js APIs or imports: all I/O goes
  through `ctx`, and values crossing it are JSON.
- A call's memory lasts for the call, across all its waits. Keep what the
  next call needs in `ctx.db` or `ctx.kv`.
- SQL commits with the function's memory, together: no transaction is needed
  to stay consistent. `ctx.db.transaction(fn)` is for undoing; `fn` cannot
  await.
- A call ends when its function settles: unlike Node.js, promises still
  pending are dropped. Await what must happen.
- Compute as long as you need. Work that awaits has no limit; one stretch
  without an await may run 10 minutes (`QJS_TURN_TIMEOUT`).
- A changed file is a new function: a call still waiting when its file
  changes cannot carry on, since its memory belongs to the old source.

Another directory of functions: `QJS_PROGRAMS=path/to/dir make qjs`. How
they are made durable: [rust/quickjs-durable](../rust/quickjs-durable/README.md).
