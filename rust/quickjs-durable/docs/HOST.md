# Host bindings, SQL and one commit per turn

A proposal, not yet built. It covers four things:

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
- **`ctx.kv`** becomes a table in this database, so one store holds a
  scope's data.

## 4. One record commits a turn

Today a turn puts `batch/<n>` (the pages it changed), then `commit` (the
run's state, with the chain of batches), then applies the KV writes the
commit lists. That relies on Restate losing only a suffix of the writes. With
the journal off, writes land in any order: a `commit` can be durable while
its batch is lost. And SQL would be a second commit point.

sqlite-kv v14 already has the one commit point this needs: a record that
every commit rewrites in one put, holding the log since the last checkpoint,
with stored values for what outgrows it, and the host's **effects** in the
same put (the SDKs keep their outbox there). A turn becomes one sqlite-kv
commit, and the record gains one section for the VM.

### The record

```text
v14/commit = SC14
  version, epoch, size, next_segment, garbage   // as now
  effects: the run's state as JSON             // today's `commit`, less the chain:
                                                // run, program, meta, memoryLen,
                                                // pending (the outbox), status, outcome
  log:   the SQL transactions since the checkpoint, inline or stored
  pages: the VM's chain of QPD3 batches,         // new: the host's, opaque to sqlite-kv
         each inline or stored
```

- **A turn is one put.** Its SQL transaction joins `log`, its batch joins
  `pages`, its state replaces `effects`. A few KB of each, inline.
- **Sealing covers both.** Past `inline_log_bytes`, the inline SQL segments
  and VM batches are sealed together into one stored value, as now; the
  owner's next commit names it.
- **Each chain keeps its own checkpoint.** A SQL checkpoint writes pages and
  empties `log`; `pages` is untouched. The VM's chain restarts when the host
  says so (today's rule: 256 batches, or more than twice the first): the
  commit's batch is then a new first batch (every page that differs from
  golden), and the old chain's stored values become garbage.
- **KV moves into SQL** (a table), so its writes commit with the turn too,
  instead of being applied after.

### Commit, recover

- **Commit.** One put of the record (plus a seal, or garbage deletes, in the
  same round trip). Then the outbox: its Waits are made after the commit,
  and the next commit drops the ones that are durable, as the SDKs do.
- **Recover.** Open reads the record and the stored values it names: the
  SQL log replays over `db/page/<n>`, and the VM is the golden image with the
  chain applied in order. The record's pending Waits are made again, under
  their IDs.

### What sqlite-kv needs

- `Commit.pages: Vec<Part>` in the record, sealed and named like the log.
- `Session::commit_turn(effects, pages)`, with `pages` one of: unchanged,
  append a batch, or restart the chain with a batch. SQL changes may be empty.
- `Session::pages()`: the chain as opened, each batch's bytes.

The spec gains the chain as a second sequence of parts; its invariants do
not change: a record names only durable values, and no value leaks.

## 5. Plan

On branches, a PR each:

1. **sqlite-kv: the host's page chain in the record** (above), with tests.
   Done in #142; the TLA+ spec does not model the chain yet.
2. **quickjs-durable on sqlite-kv:** a turn commits through
   `commit_turn`; `batch/<n>`, `commit` and `kv/<key>` go away, and `ctx.kv`
   is already a table. Done in #143, measured in DESIGN.md §7.
3. **`ctx.db.sql` and `ctx.db.transaction`**, as Calls over `host_call`
   (§2), sharing the turn's session. `ctx.kv` becomes a table.
4. **Epochs instead of fuel**, with async Wasm.
5. **A registry of Calls and Waits**, for the remaining bindings.

## Open questions

- **Sync or async `ctx.db.sql`.** Synchronous says honestly that a
  statement never ends a turn. Returning a promise would leave room for a
  remote database later. The proposal is synchronous.
- **Model calls.** Not here. With Calls and Waits in place, a model call is
  a Wait whose request is read from the heap when it is made. That was the
  experiment on `quickjs-host-model`, but here it would be a registered
  binding, not a special case.
- **Size of a turn record.** Sealing bounds the inline part at 4 KiB of
  log; how large a stored value Restate handles well is to be measured.
