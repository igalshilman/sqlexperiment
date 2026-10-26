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
make functions                                   # from the repository's root
OPENAI_API_KEY=... target/release/functions      # every function here, on port 9080
npx @restatedev/restate-server                   # Restate, in another terminal
curl localhost:9070/deployments --json '{"uri": "http://localhost:9080"}'
curl localhost:8080/counter/alice/add --json 20  # 20, a second later
curl localhost:8080/counter/alice/get            # 20; bob has a count of his own
```

The object's key (`alice`) names a scope: calls under the same key share its
SQL database and KV, and run one at a time. Each call starts from the module
as evaluated: what lasts between calls is in `ctx.db` and `ctx.kv`. Each
file's header shows how to call it, and Restate's UI lists every handler.

| File                                 | What                                                                                                          |
| ------------------------------------ | ------------------------------------------------------------------------------------------------------------- |
| [`counter.js`](counter.js)           | **Start here.** A count per key, and a durable one-second timer                                               |
| [`notes.js`](notes.js)               | Notes in SQL, with `ctx.db.sql`                                                                               |
| [`memory.js`](memory.js)             | A class instance, a closure and a paused generator, all intact after a wait                                   |
| [`checkout.js`](checkout.js)         | A saga: reserve stock ([`inventory`](inventory.js)), charge ([`payments`](payments.js)), undo on failure      |
| [`refunds.js`](refunds.js)           | A refund that waits for a manager's approval (`ctx.signal`), then pays back through [`payments`](payments.js) |
| [`subscription.js`](subscription.js) | Charge every period, on durable timers, until a charge is declined                                            |
| [`retry.js`](retry.js)               | Retries with exponential backoff, against [`flaky`](flaky.js)                                                 |
| [`timeout.js`](timeout.js)           | A deadline: race a [`slow`](slow.js) call against a timer                                                     |
| [`fanout.js`](fanout.js)             | Add to `counter` under several keys at once, and wait for all                                                 |
| [`primes.js`](primes.js)             | A long computation, carried across turns                                                                      |
| [`assistant.js`](assistant.js)       | An AI agent (GPT) with the shop as its tools                                                                  |
| [`coder.js`](coder.js)               | A coding agent with a shell and files, in a workspace that outlives the run                                   |

## Writing a function

Export a function `(input, ctx)`; it may be async. The input and the result
are JSON (no input is null). `ctx` is all it needs from outside:

| Call                                                      | What                                                                                                               |
| --------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------ |
| ``ctx.db.sql`SELECT ... ${value}` ``                      | The scope's SQL database: rows as objects. `${}` values are parameters, never SQL text. Synchronous.               |
| `ctx.db.transaction(fn)`                                  | Run `fn`'s statements together, or undo them if it throws                                                          |
| `ctx.kv.get/set/delete(key)`                              | The scope's JSON values                                                                                            |
| `ctx.rpc.call({ service, handler, key? }, input)`         | Any Restate service, including the other functions here                                                            |
| `ctx.llm.conversation(name, defaults).send({ messages })` | A model's answer, any provider's (`anthropic/...`, `openai/...`, `xai/...`): the conversation is kept in the scope |
| `ctx.llm.complete(request)`                               | A model's answer to a whole request                                                                                |
| `fetch(url, init)`                                        | An HTTP request, sent by the host: only where it allows, with the API keys it adds. `fetch.json(data)` is a body   |
| `ctx.sleep(ms)`                                           | A durable timer, however long                                                                                      |
| `ctx.signal(name, input)`                                 | A value from outside, such as a person's approval: the object's `signal` handler gives it, however long it takes   |
| `ctx.key`                                                 | The object's key                                                                                                   |

- It is standard JavaScript, without Node.js APIs or imports: all I/O goes
  through `ctx`, and values crossing it are JSON.
- A call's memory lasts for the call, across all its waits. Keep what the
  next call needs in `ctx.db` or `ctx.kv`.
- SQL commits with the function's memory, together: no transaction is needed
  to stay consistent. `ctx.db.transaction(fn)` is for undoing; `fn` cannot
  await.
- A call ends when its function settles: unlike Node.js, promises still
  pending are dropped. Await what must happen.
- The input of `ctx.rpc.call` or `ctx.call` stays in the function's memory
  until the call is answered, and is read from there when the call is made,
  and again after a crash: nothing else stores it. Do not change it until
  then.
- Each call has an idempotency key, the same after a crash. To set your own,
  pass `{ idempotencyKey }` as the last argument: the service called runs
  once for every call with that key.
- Every object also has `pending`, the signals its calls wait for (each
  with its `id`, `name`, `input` and `since`), and `signal`, which gives one:
  `{"id": ..., "value": ...}` for one wait, or `{"name": ..., "value": ...}`
  for the oldest waiting for that name. A signal no one waits for is
  dropped, unless `"keep": true` keeps it for the next wait for its name.
  While a call waits, nothing runs; a crash or a restart changes nothing.
  Exports cannot be named `pending` or `signal`.
- Compute as long as you need. Work that awaits has no limit; one stretch
  without an await may run 10 minutes (`FUNCTIONS_TURN_TIMEOUT`).
- `FUNCTIONS_MEMORY=1g` caps the memory calls hold. Past it, the calls that
  have waited longest let their memory go, and restore it when their wait
  ends; a call that does not fit waits its turn.
- A changed file is a new function: a call still waiting when its file
  changes cannot carry on, since its memory belongs to the old source.

Another directory of functions: `FUNCTIONS_PROGRAMS=path/to/dir make functions`. How
they are made durable: [rust/functions](../rust/functions/README.md).
