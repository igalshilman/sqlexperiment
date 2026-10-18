//! Durable JavaScript in Restate virtual objects. A program is a static
//! JavaScript file defining `main(input, ctx)`, shipped with its golden memory
//! dump (the `golden` binary of quickjs-durable computes it). Each program is
//! served as a virtual object of its name: a key is a scope, whose runs share
//! their `ctx.kv`, kept with the runs' images in one SQL database (sqlite-kv)
//! in the object's state.
//!
//! `<program>/<scope>/run` runs `main(input)` and returns its outcome. Behind
//! it, a turn is one invocation of the object's exclusive `start` or
//! `complete` handler, without journal replay: it restores the run's image,
//! delivers one event, runs the JavaScript until it waits, and saves the next
//! image, the KV writes and the new requests in one commit. A retried attempt
//! runs the turn again against the state as stored. After the commit, the turn
//! dispatches the new requests (the outbox) with their operation ID as
//! idempotency key, and each result comes back as a `complete` invocation:
//!
//! | `ctx` request             | Dispatched as                                                              |
//! | ------------------------- | -------------------------------------------------------------------------- |
//! | `ctx.rpc.call(target, x)` | A send to the object's `call`, which calls `target` and sends its outcome |
//! | `ctx.sleep(ms)`           | A delayed send to `complete`                                               |
//! | `ctx.kv.*`                | Nothing: answered in the turn, from the scope's database                   |
use quickjs_durable::{
    Engine, Operation, Outcome, Programs, RpcTarget, Runtime, View, namespace::NamespaceTransaction,
};
use restate_sdk::{context::RequestTarget, prelude::*};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlite_kv::StorageOptions;
use std::{
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

mod store;
use store::ObjectStore;

/// Fault injection, for tests.
#[doc(hidden)]
pub mod faults {
    use std::sync::atomic::{AtomicU32, Ordering};

    static AFTER_COMMIT: AtomicU32 = AtomicU32::new(0);

    /// Fail the next `n` turns once they have committed, before they dispatch
    /// anything: as if their attempt crashed there.
    pub fn crash_after_commit(n: u32) {
        AFTER_COMMIT.store(n, Ordering::SeqCst);
    }

    /// The crashes still to inject.
    pub fn pending() -> u32 {
        AFTER_COMMIT.load(Ordering::SeqCst)
    }

    pub(crate) fn after_commit() -> Result<(), std::io::Error> {
        let crash = AFTER_COMMIT
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok();
        if crash {
            return Err(std::io::Error::other("Injected crash after commit"));
        }
        Ok(())
    }
}

/// A run to start: `caller` is the invocation that waits for its outcome, a
/// signal named [`OUTCOME`].
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Start {
    pub run: String,
    #[serde(default)]
    pub input: Value,
    #[serde(default)]
    pub caller: Option<String>,
}

/// The result of a run's request. `at` is the earliest logical time to deliver
/// it at, a timer's deadline.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Completion {
    pub run: String,
    pub operation: String,
    pub outcome: Outcome,
    #[serde(default)]
    pub at: u64,
}

/// A run's RPC, for the object's `call` to make.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Call {
    pub run: String,
    pub operation: String,
    pub target: RpcTarget,
    pub input: Value,
}

/// The signal the run's outcome is sent to its caller as.
pub const OUTCOME: &str = "outcome";

/// A program, served as a virtual object of its name: a key is a scope.
pub struct JsObject {
    program: String,
    programs: Arc<Programs>,
}

/// What a turn does.
enum Step {
    Start(Start),
    Complete(Completion),
}

#[restate_sdk::object]
impl JsObject {
    /// Run the program's `main(input)` in this scope, and return its outcome:
    /// the main promise's value, or its error as a terminal error.
    #[handler]
    async fn run(
        &self,
        ctx: SharedObjectContext<'_>,
        input: Json<Value>,
    ) -> HandlerResult<Json<Value>> {
        let id = ctx.invocation_id().to_owned();
        ctx.request::<_, ()>(
            RequestTarget::object(&self.program, ctx.key(), "start"),
            Json(Start {
                run: id.clone(),
                input: input.into_inner(),
                caller: Some(id),
            }),
        )
        .send();
        Ok(ctx.signal::<Json<Value>>(OUTCOME).await?)
    }

    /// Start a run, unless it has started: then this is a retry, and does
    /// nothing more than dispatch what is still due.
    #[handler]
    async fn start(&self, ctx: ObjectContext<'_>, start: Json<Start>) -> HandlerResult<()> {
        let start = start.into_inner();
        let run = start.run.clone();
        self.turn(&ctx, &run, Step::Start(start)).await
    }

    /// Deliver a request's result. A result already delivered does nothing
    /// more than dispatch what is still due.
    #[handler]
    async fn complete(
        &self,
        ctx: ObjectContext<'_>,
        completion: Json<Completion>,
    ) -> HandlerResult<()> {
        let completion = completion.into_inner();
        let run = completion.run.clone();
        self.turn(&ctx, &run, Step::Complete(completion)).await
    }

    /// Make a run's RPC, and send its outcome to the run's `complete`.
    #[handler]
    async fn call(&self, ctx: SharedObjectContext<'_>, call: Json<Call>) -> HandlerResult<()> {
        let Call {
            run,
            operation,
            target,
            input,
        } = call.into_inner();
        let target = match target.key {
            Some(key) => RequestTarget::object(target.service, key, target.handler),
            None => RequestTarget::service(target.service, target.handler),
        };
        let outcome = match ctx
            .request::<_, bytes::Bytes>(target, Json(input))
            .call()
            .await
        {
            Ok(body) if body.is_empty() => Outcome::success(Value::Null),
            Ok(body) => match serde_json::from_slice::<Value>(&body) {
                Ok(value) => Outcome::success(value),
                Err(error) => {
                    Outcome::failure("TypeError", format!("Response is not JSON: {error}"))
                }
            },
            Err(error) => Outcome::failure("TerminalError", error.message()),
        };
        ctx.request::<_, ()>(
            RequestTarget::object(&self.program, ctx.key(), "complete"),
            Json(Completion {
                run,
                operation,
                outcome,
                at: 0,
            }),
        )
        .send();
        Ok(())
    }
}

impl JsObject {
    async fn turn(&self, ctx: &ObjectContext<'_>, run: &str, step: Step) -> HandlerResult<()> {
        let caller = match &step {
            Step::Start(start) => start.caller.clone(),
            Step::Complete(_) => None,
        };
        let store = Arc::new(ObjectStore::new(ctx.raw_state()));
        let (view, outbox) = match self.commit(store.clone(), run, step).await {
            Ok(turn) => turn,
            // A storage failure is the invocation's, and retried; anything
            // else would fail again, the same way.
            Err(error) if store.failed() => return Err(error.into()),
            Err(error) => {
                let error = TerminalError::new(format!("{error:#}"));
                answer(ctx, run, caller, Err(error.clone())).await?;
                return Err(error.into());
            }
        };
        faults::after_commit()?;
        dispatch(ctx, &self.program, run, &outbox).await?;
        match &view.state.outcome {
            Some(Outcome::Success { value, .. }) => {
                answer(ctx, run, caller, Ok(value.clone())).await
            }
            Some(Outcome::Failure { error, .. }) => {
                let error = TerminalError::new(format!("{}: {}", error.name, error.message));
                answer(ctx, run, caller, Err(error)).await
            }
            // Its caller waits for a later turn.
            None => {
                if let Some(caller) = caller {
                    ctx.set(&caller_key(run), caller);
                }
                Ok(())
            }
        }
    }

    /// Stage the step in one session of the scope's database and save it:
    /// the run's view and its outstanding requests, all committed.
    async fn commit(
        &self,
        store: Arc<ObjectStore>,
        run: &str,
        step: Step,
    ) -> anyhow::Result<(View, Vec<Operation>)> {
        let transaction =
            NamespaceTransaction::begin(store, StorageOptions::default(), run).await?;
        let mut engine = Engine::new(transaction, self.programs.clone());
        let view = match step {
            Step::Start(start) => engine.start(run, &self.program, start.input, now()).await?,
            Step::Complete(completion) => {
                engine
                    .complete(
                        run,
                        &completion.operation,
                        completion.outcome,
                        now().max(completion.at),
                    )
                    .await?
            }
        };
        let mut transaction = engine.into_storage()?;
        let outbox = transaction.effects().await?;
        transaction.commit().await?;
        Ok((view, outbox))
    }
}

/// The state key of the outbox's requests this run has dispatched.
fn dispatched_key(run: &str) -> String {
    format!("js/dispatched/{run}")
}

/// The state key of the invocation waiting for this run's outcome.
fn caller_key(run: &str) -> String {
    format!("js/caller/{run}")
}

/// Dispatch the committed outbox's requests not yet dispatched, then record
/// them: the record is stored after the sends, so once it is, they are.
async fn dispatch(
    ctx: &ObjectContext<'_>,
    program: &str,
    run: &str,
    outbox: &[Operation],
) -> HandlerResult<()> {
    let key = dispatched_key(run);
    let dispatched: Vec<String> = match ctx.get::<Json<Vec<String>>>(&key).await? {
        Some(ids) => ids.into_inner(),
        None => Vec::new(),
    };
    for operation in outbox {
        if !dispatched.iter().any(|id| id == operation.id()) {
            send(ctx, program, run, operation);
        }
    }
    if outbox.is_empty() {
        if !dispatched.is_empty() {
            ctx.clear(&key);
        }
    } else {
        let ids: Vec<String> = outbox.iter().map(|op| op.id().to_owned()).collect();
        if ids != dispatched {
            ctx.set(&key, Json(ids));
        }
    }
    Ok(())
}

fn send(ctx: &ObjectContext<'_>, program: &str, run: &str, operation: &Operation) {
    let id = operation.id().to_owned();
    let complete = |outcome: Outcome, at: u64| {
        ctx.request::<_, ()>(
            RequestTarget::object(program, ctx.key(), "complete"),
            Json(Completion {
                run: run.into(),
                operation: id.clone(),
                outcome,
                at,
            }),
        )
        .idempotency_key(id.clone())
    };
    match operation {
        Operation::Rpc { target, input, .. } => {
            ctx.request::<_, ()>(
                RequestTarget::object(program, ctx.key(), "call"),
                Json(Call {
                    run: run.into(),
                    operation: id.clone(),
                    target: target.clone(),
                    input: input.clone(),
                }),
            )
            .idempotency_key(id)
            .send();
        }
        Operation::Sleep { deadline, .. } => {
            let delay = Duration::from_millis(deadline.saturating_sub(now()));
            complete(Outcome::success(Value::Null), *deadline).send_after(delay);
        }
        Operation::Resume { .. } => {
            complete(Outcome::success(Value::Null), 0).send();
        }
        Operation::Call { name, .. } => {
            let message = format!("No host operation {name:?}: call a service with ctx.rpc.call");
            complete(Outcome::failure("Error", message), 0).send();
        }
        // Answered in the turn, never in the outbox.
        Operation::KvGet { .. } | Operation::KvSet { .. } | Operation::KvDelete { .. } => {}
    }
}

/// Send the run's outcome to the invocation waiting for it, if one still is:
/// `caller` when the turn has it, else the one recorded.
async fn answer(
    ctx: &ObjectContext<'_>,
    run: &str,
    caller: Option<String>,
    outcome: Result<Value, TerminalError>,
) -> HandlerResult<()> {
    let key = caller_key(run);
    let caller = match caller {
        Some(caller) => Some(caller),
        None => ctx.get::<String>(&key).await?,
    };
    if let Some(caller) = caller {
        let signal = ctx.invocation_handle(caller).signal(OUTCOME);
        match outcome {
            Ok(value) => signal.resolve(Json(value)),
            Err(error) => signal.reject(error),
        }
        ctx.clear(&key);
    }
    Ok(())
}

/// The programs built into this binary, from `QJS_PROGRAMS` (by default
/// `programs/`) at build time: each `<name>.js` with its `<name>.golden`.
pub fn embedded() -> anyhow::Result<Arc<Programs>> {
    let mut programs = Programs::new(Arc::new(Runtime::new()?));
    for (name, source, dump) in built::PROGRAMS {
        programs = programs.load(name, source, dump)?;
    }
    Ok(Arc::new(programs))
}

mod built {
    include!(concat!(env!("OUT_DIR"), "/programs.rs"));
}

/// Bind each program as a virtual object of its name: its turns (`start`,
/// `complete`) without journal replay, its state loaded lazily (a turn reads
/// only the pages it needs), and no journal kept once a turn is done (it holds
/// every page the turn read). `run` and `call` are ordinary shared handlers.
pub fn bind(
    mut builder: restate_sdk::endpoint::Builder,
    programs: &Arc<Programs>,
) -> restate_sdk::endpoint::Builder {
    let replayed = || HandlerOptions::new().journal_replay(true);
    for name in programs.names() {
        let object = JsObject {
            program: name.to_owned(),
            programs: programs.clone(),
        };
        builder = builder.bind(
            object.into_service_definition().options(
                ServiceOptions::new()
                    .name(name)
                    .journal_replay(false)
                    .enable_lazy_state(true)
                    .journal_retention(Duration::ZERO)
                    .handler("run", replayed())
                    .handler("call", replayed().ingress_private(true))
                    .handler("complete", HandlerOptions::new().ingress_private(true)),
            ),
        );
    }
    builder
}

/// An endpoint serving `programs`, and nothing else.
pub fn endpoint(programs: &Arc<Programs>) -> Endpoint {
    bind(Endpoint::builder(), programs).build()
}

/// Milliseconds since the epoch: a turn's logical time never runs backwards.
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}
