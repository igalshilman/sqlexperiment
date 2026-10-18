# Host bindings, SQL and one commit per turn

A proposal, not yet built. It covers four things:

- how a turn's time is bounded, instead of fuel;
- how the host offers functions to the guest;
- `ctx.db.sql`, with SQLite outside the guest;
- one record that commits a turn's SQL, pages and effects together.

## 1. Fuel, and what replaces it

**What it is.** Wasmtime counts the Wasm instructions the guest executes,
and traps when a budget runs out. Each turn gets 200 million. The budget
resets every turn, so it bounds one turn, never a run. It does two jobs:

1. **Stop a runaway job.** A synchronous loop that never returns would hold
   the handler's thread forever, and would do the same again on every retry.
   Past the budget, the VM traps and the run fails.
2. **Save progress in long local work.** Past half the budget, a turn that
   is answering local requests (KV) stops at its next one and commits; the
   work goes on in the next turn.

**Why it keeps limiting us.** 200 million Wasm instructions is about
**30 ms** of compute. Measured on 18 October 2026: a counting loop trapped
after 31.6 ms, and the same job takes 445 ms without the limit. Anything
real in one synchronous job runs out: serializing a 145 KiB conversation,
a parser, a sort. The budget was a guess, never tuned.

**A larger budget would work.** It is only until the next commit, as you
say. But fuel costs on every instruction: the same job took ~520 ms with
fuel on and a budget it never reached, against ~433 ms with fuel off
(median of 5 each). That is a 15–20% cost for a check that should almost
never fire.

**Proposal: epoch interruption, by wall-clock time.** Wasmtime's epochs
are its cheap alternative. A ticker thread increments a counter every few
milliseconds, and compiled code checks it only at function entries and
loop back-edges. Nothing here is replayed, so a turn needs no determinism
and can be bounded by time:

- **Runaway jobs:** a turn's synchronous work is capped by wall-clock
  time, 10 s by default, configurable per program. Past it, the run fails
  with `Trap`, as it does today.
- **Long local work:** past one second, the turn commits at the next safe
  point, as it does today at half the fuel.
- **Async execution:** run the Wasm with Wasmtime's async support, on a
  fiber. A long job then yields to the executor at each epoch tick, instead
  of blocking a Tokio worker thread. The same mechanism lets host functions
  be `async` (§2).

The measured gain is the 15–20% fuel cost on compute, and the 30 ms
ceiling goes away. Instrumented stores (the barriers mode) are unaffected.

## 2. Host callbacks

### Today

The guest reaches the host one way only: it issues a request and gets a
promise. Adding an operation touches three places:

1. **Guest (`assets/guest.js`):** a `ctx` method that issues
   `{kind, ...fields}`.
2. **Model (`model.rs`):** a variant of `Operation`.
3. **Host:** either answered in the turn by the run loop (`run.rs`; KV
   works this way), or left pending and turned into a durable future by the
   binding (`restate.rs`; RPC and timers). `ctx.call(name, args)` exists as
   a generic kind, but the binding fails it with "No host operation".

Even a request answered in the turn round-trips through the job queue. The
guest reports it as JSON, the host answers, the guest's promise settles, and
the jobs drain again. That cost is paid per KV read.

### Proposal: two kinds of binding

The pinned reactor already supports synchronous host functions, unused so
far. `qjs_new_host_function(name)` makes a JavaScript function, and calling
it calls the import `env.host_call(name, this, argc, argv)`. The function
object in the heap holds only its name. A VM restored from a snapshot
therefore works as soon as the host registers the same names again, which
it does for every VM.

| Kind                | Examples                                     | When it runs              | What it costs a turn                          |
| ------------------- | -------------------------------------------- | ------------------------- | --------------------------------------------- |
| **Call** (in turn)  | `ctx.db.sql`, `ctx.kv.*`, randomness, clock  | Synchronously, in the job | Its effects join the turn's commit            |
| **Wait** (durable)  | `ctx.rpc.call`, `ctx.sleep`, signals         | After the commit          | A pending entry in the commit (the outbox)    |

- **Calls** go through `host_call`, dispatched by name to a registry in
  the host:

  ```rust
  /// A function the guest calls in a turn, as `name` (baked into heaps).
  trait Call: Send + Sync {
      fn name(&self) -> &'static str;
      /// JSON in, JSON out; an `Err` throws in the guest.
      async fn call(&self, turn: &mut Turn<'_>, args: Value) -> Result<Value, GuestError>;
  }
  ```

  `Turn` gives a binding the scope's SQL session, the run's identity, the
  turn's time and its buffered effects. A call may await host I/O, such as
  reading a page from Restate state. With async Wasm (§1), the guest's stack
  is suspended on its fiber meanwhile, and JavaScript sees a plain function
  call. Arguments and results cross as one JSON string each way, serialized
  and parsed in C.
- **Waits** keep today's protocol: a request, a promise, an entry in the
  commit, a durable future after it. A registry replaces the hard-coded
  `match`:

  ```rust
  /// A request the run waits on across turns.
  trait Wait: Send + Sync {
      fn kind(&self) -> &'static str;
      /// After the commit: the durable future that completes it. On a retried
      /// attempt it is made again, from the same entry, under the same ID.
      fn wait<'ctx>(&self, ctx: &'ctx ObjectContext<'ctx>, id: &str, request: &Value)
          -> Waiting<'ctx>;
  }
  ```

**Adding a binding** then means one registration in Rust and one line of
bridge code (`ctx.x = __host('x')`). Renaming a binding, or changing what
it returns, is a new runtime version: names live in heaps.

## 3. `ctx.db.sql`

Each scope (an object key) has a SQLite database: `core/sqlite-kv`,
running in the host over the object's Restate state. The guest never holds
pages of it. Only statements go in and only rows come out.

```js
async function main({ sku, qty }, ctx) {
  ctx.db.sql`CREATE TABLE IF NOT EXISTS stock (sku TEXT PRIMARY KEY, n INTEGER)`;
  const [row] = ctx.db.sql`SELECT n FROM stock WHERE sku = ${sku}`;
  if (!row || row.n < qty) throw new Error(`Out of ${sku}`);
  ctx.db.transaction(() => {
    ctx.db.sql`UPDATE stock SET n = n - ${qty} WHERE sku = ${sku}`;
    ctx.db.sql`INSERT INTO orders (sku, qty) VALUES (${sku}, ${qty})`;
  });
  await ctx.rpc.call({ service: 'shipping', handler: 'ship' }, { sku, qty });
  return row.n - qty;
}
```

- **Synchronous.** A statement is a Call (§2), not a Wait, so it never ends
  a turn. A tagged template binds every `${}` as a parameter, never as SQL
  text. Rows come back as objects. `ctx.db.query(sql, params)` is the
  untagged form.
- **Durable at the turn's commit,** together with the heap and the turn's
  effects (§4). A crash resumes from the last commit, with the database and
  the heap as they were together at that point. The program needs no
  transaction to stay consistent with its own memory.
- **`ctx.db.transaction(fn)`** is for rollback: `BEGIN`, `fn()`, then
  `COMMIT`, or `ROLLBACK` if `fn` throws. `fn` is synchronous, so a
  transaction never spans a wait, and a turn never commits half of one.
  That is the rule that keeps an open SQL transaction out of a commit.
- **Errors.** A SQL error (syntax, a constraint) throws in the guest, and
  undoes that statement, as in SQLite. A storage error fails the attempt,
  which is retried from the last commit, as today.
- **Readers.** A shared handler (`<program>/<key>/query`) can run read-only
  SQL against the committed database (sqlite-kv's reader), without
  disturbing a running run.
- **`ctx.kv`** becomes a table in this database, so one store holds a
  scope's data.

## 4. One record commits a turn

Today a turn writes:

- `batch/<n>`, the pages it changed;
- `commit`, the run's state, with the chain of batches and the pending
  effects;
- then the KV writes, applied after the commit.

sqlite-kv commits on its own, through a log segment with a commit flag.
Two commit points cannot be atomic together. So there is one: a log of
turns, in the shape sqlite-kv already proves out (its log, `meta`,
checkpoint invariants, and TLA+ spec).

### Keys

| Key           | Holds                                                                                                                            |
| ------------- | -------------------------------------------------------------------------------------------------------------------------------- |
| `turn/<seq>`  | **The turn record**, written once. Its presence commits turn `seq`.                                                              |
| `meta`        | `log_start`: the first turn record still needed. The run's identity. The base below.                                            |
| `base/vm/<k>` | The VM pages that differ from golden, as of `log_start`, in QPD3 (as a first batch is today).                                    |
| `db/page/<n>` | SQL pages as of `log_start` (sqlite-kv's `page/<n>`).                                                                            |
| `turn/<seq>/<part>` | A large turn's sections, written before its record (as sqlite-kv's non-final segments).                                     |

### The turn record

```text
TurnRecord = "QTR1"
  state: JSON {                       // what today's commit holds, less the chain
      run, program, version, runtime, turn: seq,
      meta: {stack, runtime, context}, memoryLen,
      pending: Operation[],           // the outbox: Waits to make after the commit
      nextOperation, now, status, outcome?
  }
  vm:  QPD3 batch | part reference    // the pages the turn changed, against their previous contents
  sql: sqlite-kv transaction | part reference | none
                                      // the changed SQL pages, against their previous images
```

Each section is length-prefixed and binary, so nothing is base64 inside
JSON. A typical turn is a few KB of VM pages plus a few KB of SQL pages:
**one put commits it all.** Today that is one put for the batch, one for
the commit, and one per KV write. A large turn puts its sections as parts
first, then the record, which names them.

### Commit, recover, checkpoint

- **Commit.** Put the record, then issue the outbox. Restate journals a
  handler's writes in order and a crash loses only a suffix. So a call
  issued after the record exists only if the record does, and nothing waits
  for an acknowledgement in between.
- **Recover** (another attempt):
  1. Read `meta`.
  2. Read `turn/<log_start..>` concurrently, probing until the first
     missing one.
  3. Apply each record's VM and SQL sections in order, over `base/` and
     `db/page/`.
  4. Take the last record's `state`.

  Its pending Waits are made again, under their IDs. A record past the
  first gap is a failed turn's leftover; the next turn overwrites it.
- **Checkpoint** (when the log passes a count or twice the base's bytes,
  as the chain policy does now). Steps, in order, each one after the
  previous step is acknowledged:
  1. Write the new `base/vm/<k+1>` and the SQL pages to `db/page/<n>`.
  2. Move `meta.log_start` past the folded turns.
  3. Delete what `meta` no longer names.

  These are sqlite-kv's invariants 3–5 applied to both page spaces. Any
  prefix of a checkpoint is a valid state.

### What a crash leaves

| Interrupted          | Result                                                                   |
| -------------------- | ------------------------------------------------------------------------ |
| A turn, before its record | The turn did not happen: the attempt reruns it from the last record. |
| A turn, after its record  | It happened. Its Waits are made again, under the same IDs (attached). |
| A checkpoint         | `meta` names the old or the new base. The log covers both.               |
| A deletion           | Repeated by the next checkpoint.                                         |

### What sqlite-kv needs

An external-commit mode, beside `Durability::Save` and `Durability::Commit`:

- `Session::prepare()`: the changed pages, encoded as one transaction (the
  bytes a commit segment would hold), with no put. The host puts them
  inside its turn record.
- `Session::committed(seq)`: the host's record is durable. The view moves
  forward, as after a save.
- `open_with_log(records)`: open over pages, with the log's transactions
  supplied by the host from the turn records, not read from `log/<n>`.
- `checkpoint_pages()`: the latest images to write at `db/page/<n>`, for
  the host's checkpoint.

The page codec, the replay and the checkpoint rules stay sqlite-kv's. Only
the commit point moves to the host.

## 5. Plan

On a branch, in this order. Each step can be measured on its own:

1. **Epochs instead of fuel**, with async Wasm. Measure primes and the
   agent benchmark.
2. **A registry of Calls over `host_call`**, with `ctx.kv` ported to it.
   Measure the KV benchmark, where each read stops round-tripping through
   the job queue.
3. **sqlite-kv's external-commit mode**, with its simulation tests.
4. **`ctx.db.sql` and `ctx.db.transaction`**, at first committed beside
   today's records.
5. **The turn record:** `turn/<seq>`, `meta`, `base/`, and checkpoints over
   both page spaces. `ctx.kv` moves into SQL.

## Open questions

- **Sync or async `ctx.db.sql`.** Synchronous says honestly that a
  statement never ends a turn. Returning a promise would leave room for a
  remote database later. The proposal is synchronous.
- **Model calls.** Not here. With Calls and Waits in place, a model call is
  a Wait whose request is read from the heap when it is made. That was the
  experiment on `quickjs-host-model`, but here it would be a registered
  binding, not a special case.
- **Size of a turn record.** How large a state value Restate handles well
  is to be measured. Past that size, a record's sections go to parts.
