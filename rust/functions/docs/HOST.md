# Host bindings, SQL and one commit per turn

A proposal, built but for the registry of in-turn Calls (§5). It covers
five things:

- how a turn's time is bounded, instead of fuel;
- how the host offers functions to the guest;
- `ctx.db.sql`, with SQLite outside the guest;
- one flush that makes a turn's SQL, pages and records durable together:
  the state's (v19) tail entry and head, the memory's pages a file beside
  the SQL's;
- host bindings: operations the host makes itself, such as a model call,
  whose input nothing but the guest's heap keeps (§6).

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
   binding (`restate.rs`; RPC and timers). `ctx.call(name, input)` is a
   host binding, made by the handler (§6).

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

Each scope (an object key) has a SQLite database: `core/sql` on the scope's
state (`core/state`),
running in the host over the object's Restate state. The guest never holds
pages of it. Only statements go in and only rows come out.

```js
export async function order({ sku, qty }, ctx) {
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
  SQL against the flushed database (the state's reader), without
  disturbing a running run.
- **`ctx.kv`** is already a table in this database, so one store holds a
  scope's data.

## 4. One root commits a turn

Built in #142 and #143. A turn is one flush of the scope's state: the run's
progress, the VM's page batch and the turn's SQL go into one mutation, flushed as
one root, in one round trip.
Before, a turn put `batch/<n>`, then `commit` (naming the batch), then the KV
writes the commit listed. That was correct, since Restate keeps writes in
issue order and a crash keeps a prefix, but it was three kinds of keys and
two commit points once SQL joined.

### The record

```text
v19/head = the commit point
  id, frame size, version, the checkpoint and the one before, the tail
v19/tail/<version>/<part>: one turn's mutation
    run:               record: the run's progress as JSON // run, program, pending
                                                         // (the outbox), status, outcome
    memory:            record: the memory's as JSON      // meta, memoryLen, file, pages
    memory/<golden>:   paged file over the golden image  // the pages that differ from it
    db:                paged file                        // SQL
    <name>/…:          each extension's records and files
v19/pack/<checkpoint>/<part>: a checkpoint: frames of 4 pages, values,
  and its manifest (each file's frames, every record)
```

- **A turn is one tail entry.** Its SQL pages and its memory's changed
  pages, from its mutation, each compressed against the page as the
  checkpoint holds it (a memory page against golden's), and the records it
  set. The entry is put first, then the head, in the same round trip.
- **`ctx.kv` is a table** in the database, so its writes commit with the
  turn.

### Flush, recover

- **Flush.** The tail entry, the head: queued in that order, then one sync.
  Once the tail is long enough, the flush first checkpoints: a pack of the
  frames the tail changed (patches of the pages, when few changed), and
  deletes the packs no checkpoint needs, over the object's state keys.
  Then the outbox: its Waits are made after the flush, and the next flush
  drops the ones already made.
- **Recover.** Open reads the head, then the manifest and the tail, and
  the VM is the golden image with the pages the memory record lists, read
  together by frame. The record's pending Waits are made again, under
  their IDs.

The API is `State::open`, whose `record(name)`, `file(name)` and
`file_with_base(name, base)` are handles each participant writes through
the turn's `Mutation` (`state.mutation()`) in its prepare (the SQL, `Sql::open(&state)`,
flushes into it), then `Mutation::flush`, one round trip.

## 5. Plan

On branches, a PR each:

1. **sqlite-kv (now `state`): the host's page chain in the record** (§4), with tests.
   Done in #142.
2. **The functions runtime on the state:** a turn is one commit of the
   scope's state (`commit_turn` then, `Mutation::flush` now); `batch/<n>`,
   `commit` and `kv/<key>` are gone, and `ctx.kv` is a table. Done in #143,
   measured in DESIGN.md §7.
3. **`ctx.db.sql` and `ctx.db.transaction`**, as Calls over `host_call`
   (§2), sharing the turn's session. Done: the Wasm runs async (#150), and
   one host function, `__host`, answers from `src/db.rs`.
4. **Epochs instead of fuel.** Done: async Wasm (#150), then a wall-clock
   `Limits` (10 min a turn, `FUNCTIONS_TURN_TIMEOUT`, declared to Restate as the
   inactivity timeout; commit after 1 s) checked at a 10 ms epoch tick,
   where a long job also yields its thread. A compute-bound job ran ~10%
   faster without fuel (116 ms against 127 ms), and the coding session of
   DESIGN.md §10.1 now runs 200 steps instead of failing at 45.
5. **A registry of the bindings the host makes itself** (§6). Done:
   a deployment offers `llm`, the model gateway ([LLM.md](LLM.md)), and
   `fetch` ([FETCH.md](FETCH.md)); the coding session stores 4.0 MiB over
   200 steps (2.5 MiB on the log), and Restate journals none of its
   requests (DESIGN.md §10.1).
   The in-turn Calls are still `src/db.rs`'s, and the Restate Waits (RPCs,
   timers) a `match` in `restate.rs`.

## Open questions

- **Sync or async `ctx.db.sql`.** Synchronous says honestly that a
  statement never ends a turn. Returning a promise would leave room for a
  remote database later. The proposal is synchronous.
- **Model calls.** `ctx.llm` (§6, [LLM.md](LLM.md)): the host's gateway,
  any provider's models in one format, with a conversation's messages in the
  scope's database. Whether the gateway belongs on the host, or in a service
  of its own, is open.
- **Size of a turn's writes.** A record past 1 KiB leaves the root for a
  blob of its own; how large a stored value Restate handles well is to be
  measured.

## 6. Bindings the host makes

A binding is an operation the host makes for a program, by name, outside
Restate, such as `ctx.llm` ([LLM.md](LLM.md)) and `fetch(url, init)`
([FETCH.md](FETCH.md)), or by name, `ctx.call(name, input)`. It is an
extension that keeps nothing ([`Extension`](../src/extension.rs) is the
general contract); a deployment offers it among its extensions, passed to
`bind`:

```rust
/// An operation the host makes for a program.
pub trait Binding: Send + Sync + 'static {
    /// Make `call`: its result, or the error its promise rejects with.
    fn call(&self, call: HostCall) -> BoxFuture<'static, Result<Value, GuestError>>;
    /// Whether a call made again under the same idempotency key is answered
    /// as the first was. Only then may the guest set a key of its own.
    fn deduplicates(&self) -> bool { false }
}

let extensions = Extensions::new().with("fetch", Fetch::new(FetchPolicy::from_env()));
functions::bind(Endpoint::builder(), &programs, &cache, &budget, &extensions)?
```

A call is a pending request like an RPC: the handler makes it after the
commit that issued it, races it with Restate's futures, and delivers its
outcome in the next turn. Adding one is a registration in Rust; the bridges
need nothing new.

**One copy.** A call's input stays in the guest's heap until the call is
answered; the commit keeps the call's ID, name and key. The host reads the
input there (the bridge's `input(id)` hook, one `JSON.stringify` in C) when
it makes the call, and again on a retried attempt, from the restored heap.
The result is delivered to the guest and saved with its heap. So a request
lives once, in pages the program mostly holds anyway: the model's request is
the conversation, plus a few new messages. RPCs work the same way, but
Restate journals their input as well, since it delivers the call.

| A request's bytes        | RPC                 | Binding             |
| ------------------------ | ------------------- | ------------------- |
| The guest's heap (pages) | Kept until answered | Kept until answered |
| The turn's root          | ID, target, key     | ID, name, key       |
| Restate's journal        | The input           | Nothing             |

**Keys.** A call's idempotency key is the program's own, if it set one
(`{ idempotencyKey }`), or else its operation ID, `<invocation>/<n>`. The
host checks that the guest numbers its requests in order, and makes none
before the commit that issued it, so the key names the same request on every
attempt: a crash after a call is made makes it again under the same key and
with the same input. RPCs pass the key to Restate, which attaches a call
made again to the first. A binding gets it in `HostCall`; one that does not
deduplicate (the model) is called at least once, and refuses a key of the
program's own rather than seem to promise what it does not keep.

**What belongs in a binding.** A call whose input comes from the heap,
whose result is this run's alone, that may be made twice, and that finishes
well within an attempt (the turn limit, 10 minutes). What must happen once,
is shared, or runs for hours belongs in a Restate service: a payment, a
workspace, a human's approval. A binding's call is not in Restate's UI;
its key names it in the binding's own logs.
