# Durable functions

Plain JavaScript modules that survive anything: a crash, a restart, a wait
of a month. Each file `<name>.js` is a Restate virtual object `<name>`, and
each function it exports is a handler. Each key is an object of its own,
with its own copy of the module's variables, and they live on from call to
call: there is no state to load or save. While a call waits on a timer,
another service or a model, its memory is saved; when the result comes, or
after a crash, it carries on where it stopped. Nothing is replayed.

```js
// counter.js: the virtual object `counter`, with handlers `add` and `get`
let count = 0;

export async function add(n, ctx) {
  count += n;
  await ctx.sleep(1000); // a durable timer: a crash meanwhile loses nothing
  return count;
}

export function get() {
  return count;
}
```

## Run them

```sh
make qjs                                              # from the repository's root
ANTHROPIC_API_KEY=... target/release/quickjs-durable  # every function here, on port 9080
npx @restatedev/restate-server                        # Restate, in another terminal
curl localhost:9070/deployments --json '{"uri": "http://localhost:9080"}'
curl localhost:8080/counter/alice/add --json 20       # 20, a second later
curl localhost:8080/counter/alice/get                 # 20; bob has a counter of his own
```

Calls on the same key run one at a time. Each file's header shows how to
call it, and Restate's UI lists every handler, to call from its playground.

| File                                 | What                                                                                                     |
| ------------------------------------ | -------------------------------------------------------------------------------------------------------- |
| [`counter.js`](counter.js)           | **Start here.** A count per key, in a plain variable, and a durable one-second timer                     |
| [`memory.js`](memory.js)             | A class instance, a closure and a paused generator, all intact from call to call                         |
| [`notes.js`](notes.js)               | Notes in SQL, with `ctx.db.sql`                                                                          |
| [`checkout.js`](checkout.js)         | A saga: reserve stock ([`inventory`](inventory.js)), charge ([`payments`](payments.js)), undo on failure |
| [`subscription.js`](subscription.js) | Charge every period, on durable timers, until a charge is declined                                       |
| [`retry.js`](retry.js)               | Retries with exponential backoff, against [`flaky`](flaky.js)                                            |
| [`timeout.js`](timeout.js)           | A deadline: race a [`slow`](slow.js) call against a timer                                                |
| [`fanout.js`](fanout.js)             | Add to `counter` under several keys at once, and wait for all                                            |
| [`primes.js`](primes.js)             | A long computation, in one call                                                                          |
| [`assistant.js`](assistant.js)       | An AI agent (Claude) with the shop as its tools, its conversation an array                               |
| [`coder.js`](coder.js)               | A coding agent with a shell and files, in a workspace that outlives the call                             |

## Writing a function

Export a function `(input, ctx)`; it may be async. The input and the
result are JSON (no input is null). `ctx` is all it needs from outside:

| Call                                              | What                                                                                                                                                                    |
| ------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| ``ctx.db.sql`SELECT ... ${value}` ``              | The scope's SQL database: rows as objects. `${}` values are parameters, never SQL text. Synchronous.                                                                    |
| `ctx.db.transaction(fn)`                          | Run `fn`'s statements together, or undo them if it throws                                                                                                               |
| `ctx.kv.get/set/delete(key)`                      | The scope's JSON values                                                                                                                                                 |
| `ctx.rpc.call({ service, handler, key? }, input)` | Any Restate service, including the other functions here. Models too: `claude/messages` takes a [Messages API](https://platform.claude.com/docs/en/api/messages) request |
| `ctx.sleep(ms)`                                   | A durable timer, however long                                                                                                                                           |
| `ctx.key`                                         | The object's key                                                                                                                                                        |

- Keep state in variables: the module's memory is the object's. SQL
  (`ctx.db`) is there for what you want to query.
- It is standard JavaScript, without Node.js APIs or imports: all I/O goes
  through `ctx`, and values crossing it are JSON.
- SQL commits with the function's memory, together: no transaction is needed
  to stay consistent. `ctx.db.transaction(fn)` is for undoing; `fn` cannot
  await.
- A call ends when its function settles: unlike Node.js, promises still
  pending are dropped. Await what must happen.
- If a call runs out of time (a loop that never ends), it fails and its
  last turn is undone: memory and SQL are as they were at its last wait.
- Compute as long as you need. Work that awaits has no limit; one stretch
  without an await may run 10 minutes (`QJS_TURN_TIMEOUT`).
- An object's memory belongs to the source that made it: once a file
  changes, its existing objects refuse calls. Start new keys after a change,
  for now.

Another directory of functions: `QJS_PROGRAMS=path/to/dir make qjs`. How
they are made durable: [rust/quickjs-durable](../rust/quickjs-durable/README.md).
