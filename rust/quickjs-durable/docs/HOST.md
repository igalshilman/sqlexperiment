# Host bindings, SQL and one commit per turn

A proposal; §4 and the first two steps of the plan (§5) are built. It
covers four things:

- how a turn's time is bounded, instead of fuel;
- how the host offers functions to the guest;
- `ctx.db.sql`, with SQLite outside the guest;
- one record that commits a turn's SQL, pages and effects together:
  sqlite-kv v14's commit record, with the VM's chain beside the SQL log.

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

| Kind               | Examples                                    | When it runs              | What it costs a turn                       |
| ------------------ | ------------------------------------------- | ------------------------- | ------------------------------------------ |
| **Call** (in turn) | `ctx.db.sql`, `ctx.kv.*`, randomness, clock | Synchronously, in the job | Its effects join the turn's commit         |
| **Wait** (durable) | `ctx.rpc.call`, `ctx.sleep`, signals        | After the commit          | A pending entry in the commit (the outbox) |

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
- **`ctx.kv`** is already a table in this database, so one store holds a
  scope's data.

## 4. One record commits a turn

Built in #142 and #143. A turn is one sqlite-kv commit: the run's state, the
VM's page batch and the turn's SQL go into one record, written in one put.
Before, a turn put `batch/<n>`, then `commit` (naming the batch), then the KV
writes the commit listed. That was correct, since Restate keeps writes in
issue order and a crash keeps a prefix, but it was three kinds of keys and
two commit points once SQL joined.

### The record

```text
v14/commit = SC14
  version, epoch, size, next_segment, garbage
  effects: the run's state as JSON           // run, program, meta, memoryLen,
                                             // pending (the outbox), status, outcome
  log:     the SQL transactions since the checkpoint, inline or stored
  chain:   the VM's QPD3 batches, in order   // the host's, opaque to sqlite-kv
```

- **A turn is one put.** Its SQL transaction joins `log`, its batch joins
  `chain`, its state replaces `effects`.
- **Sealing covers both.** Past `inline_log_bytes` (1 KiB), the inline SQL
  segments and VM batches move into one stored value, written before the
  record that names it, in the same round trip.
- **Each sequence keeps its own checkpoint.** A SQL checkpoint writes pages
  and empties `log`; `chain` is kept. The VM's chain restarts when the host
  says so (256 batches, or more than twice the first): the turn's batch is
  then a new first batch (every page that differs from golden), and the old
  chain's stored values become garbage.
- **`ctx.kv` is a table** in the database, so its writes commit with the
  turn.

### Commit, recover

- **Commit.** Garbage deletes, a seal if one is due, the record, and a SQL
  checkpoint if one is due: queued in that order, then one sync. Then the
  outbox: its Waits are made after the commit, and the next commit drops
  the ones already made.
- **Recover.** Open reads the record and the stored values it names: the
  SQL log replays over `page/<n>`, and the VM is the golden image with the
  chain applied in order. The record's pending Waits are made again, under
  their IDs.

The API is `Session::commit_turn(effects, chain)`, where `chain` is
unchanged, an appended batch, or a restart with a batch, and
`Session::chain()`, the chain as opened.

## 5. Plan

On branches, a PR each:

1. **sqlite-kv: the host's page chain in the record** (§4), with tests.
   Done in #142.
2. **quickjs-durable on sqlite-kv:** a turn commits through
   `commit_turn`; `batch/<n>`, `commit` and `kv/<key>` are gone, and
   `ctx.kv` is a table. Done in #143, measured in DESIGN.md §7.
3. **`ctx.db.sql` and `ctx.db.transaction`**, as Calls over `host_call`
   (§2), sharing the turn's session. Done: the Wasm runs async (#150), and
   one host function, `__host`, answers from `src/db.rs`.
4. **Epochs instead of fuel.** Done: async Wasm (#150), then a wall-clock
   `Limits` (10 min a turn, `QJS_TURN_TIMEOUT`, declared to Restate as the
   inactivity timeout; commit after 1 s) checked at a 10 ms epoch tick,
   where a long job also yields its thread. A compute-bound job ran ~10%
   faster without fuel (116 ms against 127 ms), and the coding session of
   DESIGN.md §10.1 now runs 200 steps instead of failing at 45.
5. **A registry of Calls and Waits**, for the remaining bindings.

## Open questions

- **Sync or async `ctx.db.sql`.** Synchronous says honestly that a
  statement never ends a turn. Returning a promise would leave room for a
  remote database later. The proposal is synchronous.
- **Model calls.** Not here. With Calls and Waits in place, a model call is
  a Wait whose request is read from the heap when it is made. That was the
  experiment on `quickjs-host-model`, but here it would be a registered
  binding, not a special case.
- **Size of a turn record.** Sealing bounds the inline part at about 1 KiB;
  how large a stored value Restate handles well is to be measured.
