//! Durable JavaScript in Restate virtual objects. A run is a JavaScript
//! `main(input, ctx)`; at each durable wait its QuickJS memory is saved as the
//! pages that changed, never a journal to replay. An object key is a scope: its
//! applications, its runs' images and their `ctx.kv` live in one SQL database
//! (sqlite-kv) in the object's state.
//!
//! A turn is one invocation of the object's exclusive handlers, without journal
//! replay: it restores the run's image, delivers one event, runs the
//! JavaScript until it waits, and saves the next image, the KV writes and the
//! new requests in one commit. A retried attempt runs the turn again against
//! the state as stored. After the commit, the turn dispatches the new requests
//! (the outbox) with their operation ID as idempotency key, and each result
//! comes back as a [`JsScope::complete`] invocation:
//!
//! | `ctx` request             | Dispatched as                                                      |
//! | ------------------------- | ------------------------------------------------------------------ |
//! | `ctx.rpc.call(target, x)` | A send to [`Js::call`], which calls `target` and sends its outcome |
//! | `ctx.sleep(ms)`           | A delayed send to `complete`                                       |
//! | `ctx.kv.*`                | Nothing: answered in the turn, from the scope's database           |
//!
//! [`Js::run`] starts a run and waits for its outcome, which the turn the
//! main promise settles in sends back as a signal.
use quickjs_durable::{
    Engine, Operation, Outcome, RpcTarget, Runtime, View, namespace::NamespaceTransaction,
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

/// An application: `source` defines `async function main(input, ctx)`. Its ID
/// names one immutable version.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct App {
    pub id: String,
    pub source: String,
}

/// A run of `app` in [`Js::run`]'s scope.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Run {
    pub scope: String,
    pub app: String,
    #[serde(default)]
    pub input: Value,
}

/// A run to start: `caller` is the invocation that waits for its outcome, a
/// signal named [`OUTCOME`].
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Start {
    pub run: String,
    pub app: String,
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

/// A run's RPC, for [`Js::call`] to make.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Call {
    pub scope: String,
    pub run: String,
    pub operation: String,
    pub target: RpcTarget,
    pub input: Value,
}

/// The signal the run's outcome is sent to its caller as.
pub const OUTCOME: &str = "outcome";

/// The scope: an object whose key is a KV scope, with its applications and runs.
pub struct JsScope {
    runtime: Arc<Runtime>,
}

/// Runs: [`Js::run`] for callers, [`Js::call`] for the scopes' RPCs.
pub struct Js;

/// What a turn does.
enum Step {
    Register(App),
    Start(Start),
    Complete(Completion),
}

#[restate_sdk::object]
impl JsScope {
    /// Register an application version in this scope. Registering the same ID
    /// with the same source again does nothing; with another, it fails.
    #[handler]
    async fn register(&self, ctx: ObjectContext<'_>, app: Json<App>) -> HandlerResult<()> {
        self.turn(&ctx, "", Step::Register(app.into_inner())).await
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
}

impl JsScope {
    /// The object, with its QuickJS runtime (compiled once).
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self {
            runtime: Arc::new(Runtime::new()?),
        })
    }

    async fn turn(&self, ctx: &ObjectContext<'_>, run: &str, step: Step) -> HandlerResult<()> {
        let caller = match &step {
            Step::Start(start) => start.caller.clone(),
            _ => None,
        };
        let store = Arc::new(ObjectStore::new(ctx.raw_state()));
        let (view, outbox) = match self.commit(store.clone(), run, step).await {
            Ok(Some(turn)) => turn,
            Ok(None) => return Ok(()),
            // A storage failure is the invocation's, and retried; anything
            // else would fail again, the same way.
            Err(error) if store.failed() => return Err(error.into()),
            Err(error) => {
                let error = TerminalError::new(format!("{error:#}"));
                if !run.is_empty() {
                    answer(ctx, run, caller, Err(error.clone())).await?;
                }
                return Err(error.into());
            }
        };
        faults::after_commit()?;
        dispatch(ctx, run, &outbox).await?;
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
    ) -> anyhow::Result<Option<(View, Vec<Operation>)>> {
        let transaction =
            NamespaceTransaction::begin(store, StorageOptions::default(), run).await?;
        let mut engine = Engine::new(transaction, self.runtime.clone());
        let view = match step {
            Step::Register(app) => {
                engine.register(&app.id, &app.source).await?;
                None
            }
            Step::Start(start) => Some(engine.start(run, &start.app, start.input, now()).await?),
            Step::Complete(completion) => Some(
                engine
                    .complete(
                        run,
                        &completion.operation,
                        completion.outcome,
                        now().max(completion.at),
                    )
                    .await?,
            ),
        };
        let mut transaction = engine.into_storage()?;
        let outbox = match view {
            Some(_) => transaction.effects().await?,
            None => Vec::new(),
        };
        transaction.commit().await?;
        Ok(view.map(|view| (view, outbox)))
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
async fn dispatch(ctx: &ObjectContext<'_>, run: &str, outbox: &[Operation]) -> HandlerResult<()> {
    let key = dispatched_key(run);
    let dispatched: Vec<String> = match ctx.get::<Json<Vec<String>>>(&key).await? {
        Some(ids) => ids.into_inner(),
        None => Vec::new(),
    };
    for operation in outbox {
        if !dispatched.iter().any(|id| id == operation.id()) {
            send(ctx, run, operation);
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

fn send(ctx: &ObjectContext<'_>, run: &str, operation: &Operation) {
    let id = operation.id().to_owned();
    let complete = |outcome: Outcome, at: u64| {
        ctx.request::<_, ()>(
            RequestTarget::object("JsScope", ctx.key(), "complete"),
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
                RequestTarget::service("Js", "call"),
                Json(Call {
                    scope: ctx.key().into(),
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

#[restate_sdk::service]
impl Js {
    /// Run `app` in `scope` with `input`, and return its outcome: the main
    /// promise's value, or its error as a terminal error.
    #[handler]
    async fn run(&self, ctx: Context<'_>, run: Json<Run>) -> HandlerResult<Json<Value>> {
        let Run { scope, app, input } = run.into_inner();
        let id = ctx.invocation_id().to_owned();
        ctx.request::<_, ()>(
            RequestTarget::object("JsScope", scope, "start"),
            Json(Start {
                run: id.clone(),
                app,
                input,
                caller: Some(id),
            }),
        )
        .send();
        Ok(ctx.signal::<Json<Value>>(OUTCOME).await?)
    }

    /// Make a run's RPC, and send its outcome to the run's scope.
    #[handler]
    async fn call(&self, ctx: Context<'_>, call: Json<Call>) -> HandlerResult<()> {
        let Call {
            scope,
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
            RequestTarget::object("JsScope", scope, "complete"),
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

/// Bind both services: the scope's handlers without journal replay, its
/// state loaded lazily (a turn reads only the pages it needs), and no journal
/// kept once a turn is done (it holds every page the turn read).
pub fn bind(
    builder: restate_sdk::endpoint::Builder,
) -> anyhow::Result<restate_sdk::endpoint::Builder> {
    let scope = JsScope::new()?.into_service_definition().options(
        ServiceOptions::new()
            .journal_replay(false)
            .enable_lazy_state(true)
            .journal_retention(Duration::ZERO)
            .handler("complete", HandlerOptions::new().ingress_private(true)),
    );
    let js = Js.into_service_definition().options(
        ServiceOptions::new().handler("call", HandlerOptions::new().ingress_private(true)),
    );
    Ok(builder.bind(scope).bind(js))
}

/// An endpoint with both services, and no others.
pub fn endpoint() -> anyhow::Result<Endpoint> {
    Ok(bind(Endpoint::builder())?.build())
}

/// Milliseconds since the epoch: a turn's logical time never runs backwards.
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}
