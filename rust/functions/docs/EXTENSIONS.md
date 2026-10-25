# Extensions

An extension gives programs something of the host's (a service, a store) and
keeps its own state in the run's. The built-ins are extensions too: `fs`,
`llm` and `fetch` ([`restate.rs`](../src/restate.rs)).

## A turn is one transaction

A program runs turn by turn, and each turn is one transaction, the seal:
one mutation that every participant prepares into, then flushed.

| Participant       | Where it lives in the state                                                                                  |
| ----------------- | ------------------------------------------------------------------------------------------------------------ |
| Run state         | Record `run`                                                                                                 |
| VM memory         | Record `memory`, a file over the golden image, and a log of the turns since it was last folded into the file |
| Tables (`ctx.db`) | `Sql` on the file `db`                                                                                       |
| Each extension    | Its own records and files, named under `<name>/`                                                             |
| Large bytes       | The scope's blob store, outside Restate; a record names each blob                                            |

```
turn n                     seal n                      after the seal
───────────────────────    ────────────────────────    ───────────────────────────
program runs               prepare(&mut mutation)      operations issued in turn n
extensions: call(),        on each participant,        are made (outbox); their
issued(), delivered()      then mutation.flush()       outcomes arrive in turn n+1
```

A crash before the seal loses the whole turn, which runs again from the last
seal. Whether a turn happened is whether its seal was stored: `committed()` and
`aborted()` are only notices a crash may skip, and every attempt recovers each
participant from the last seal, in `attach()`.

## A toy extension: notary

`notary` notarizes a document through an outside service and counts receipts in
its own record. It uses two of the three things an extension can offer
([`extension.rs`](../src/extension.rs)): an operation made
after the seal, and state kept in the seal. `rust/functions/tests/transactions.rs`
has a similar `Counter` toy you can run.

```rust
use functions::{Attached, Extension, GuestError, HostCall, Making, Operation, Outcome,
                Mutation, Participant, Record, ScopeBlobs, State, Tables};

/// The extension, as a deployment offers it.
#[derive(Clone)]
struct Notary;

/// Its part of one attempt at a run.
struct Notarized {
    record: Record, // stored as "notary/count", in the seal
    count: u64,     // as this turn sees it
    sealed: u64,    // as the last seal holds it
}

#[async_trait]
impl Extension for Notary {
    /// Every attempt, on any machine, starts here: from the last seal.
    async fn attach(&self, _blobs: ScopeBlobs, state: State) -> Result<Box<dyn Attached>> {
        let record = state.record("count");
        let count = std::str::from_utf8(&record.get())?.parse().unwrap_or(0);
        Ok(Box::new(Notarized { record, count, sealed: count }))
    }
}

#[async_trait]
impl Participant for Notarized {
    /// Before the seal: set what changed in the mutation. Its flush makes it
    /// durable with the run's memory, its SQL and every other participant,
    /// in one tail entry.
    async fn prepare(&mut self, mutation: &mut Mutation) -> Result<()> {
        if self.count != self.sealed {
            mutation.set(&self.record, self.count.to_string().into())?;
        }
        Ok(())
    }
    fn committed(&mut self) { self.sealed = self.count; }
    fn aborted(&mut self) { self.count = self.sealed; } // back to the seal
}

#[async_trait]
impl Attached for Notarized {
    /// Made again under the same key, the service answers as the first time.
    fn deduplicates(&self) -> bool { true }

    /// After the seal that issued it; again after a crash, under the same key.
    async fn operation(&mut self, call: HostCall, _tables: &mut Tables<'_>)
        -> Result<Making, GuestError> {
        Ok(Box::pin(async move {
            let receipt = notarize(&call.idempotency_key, &call.input).await?;
            Ok(serde_json::json!({ "receipt": receipt }))
        }))
    }

    /// In the turn the outcome arrives, before the program sees it.
    async fn delivered(&mut self, _tables: &mut Tables<'_>, _op: &Operation,
                       _outcome: &mut Outcome) -> Result<()> {
        self.count += 1; // sealed by this turn's prepare
        Ok(())
    }
}
```

Offer it beside the built-in extensions (`rust/functions/src/restate.rs`):

```rust
Extensions::new().with("fetch", fetch).with("notary", Notary)
```

A program calls it through the generic operation entry point:

```js
export async function main(input, ctx) {
  const { receipt } = await ctx.call('notary', { doc: input.doc });
  return receipt;
}
```

What each turn seals:

| Turn                       | What runs                                                                    | What the seal holds                                                    |
| -------------------------- | ---------------------------------------------------------------------------- | ---------------------------------------------------------------------- |
| 1                          | `ctx.call('notary', …)` issues the operation; the program waits              | Memory, run state with the pending operation; `notary/count` unchanged |
| Between turns              | The host makes `operation()`; a crash here repeats it under the same key     | Nothing new                                                            |
| 2                          | `delivered()` increments the count; the program gets the receipt and returns | New memory pages, run state, `notary/count = 1`                        |
| Crash before turn 2's seal | A new attempt calls `attach()`, reads `count = 0`, and runs turn 2 again     | Unchanged                                                              |

The count can never be off by one: the increment and the program seeing the
receipt are sealed together in turn 2.

Not covered by the toy:

- **Calls inside the turn** (`async fn call`, such as `ctx.notary.status()`)
  need a wrapper in the guest code (`rust/functions/assets/guest.js`). Only the
  built-ins have one today (`ctx.fs`, `ctx.llm`); a third-party extension is
  reachable only through `ctx.call`, as an operation.
- **SQL in an extension:** `Tables` lets an extension read and write the
  scope's SQL inside the turn's transaction, sealed in the same flush. `llm` keeps
  its conversations this way.
- **Large state:** an extension writes big bytes to the scope's blobs before the
  seal and names them in a record, as `fs` does with its manifest.
