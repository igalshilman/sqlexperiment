//! Durable JavaScript in Restate virtual objects. A program is a static
//! JavaScript file defining `main(input, ctx)`, embedded in the binary. Each
//! program is served as a virtual object of its name, whose key is a scope:
//! its runs share their `ctx.kv`, kept with the run's image in the object's
//! state.
//!
//! `<program>/<key>/run` runs `main(input)` in its handler, exclusive and
//! without journal replay, and returns its outcome. The JavaScript runs until
//! it waits; the handler commits the run's changed pages, its KV writes and
//! its requests (a turn), then waits on Restate itself for the first request
//! to complete, and delivers it in the next turn, to the same live VM:
//!
//! | `ctx` request             | The handler waits on                                        |
//! | ------------------------- | ----------------------------------------------------------- |
//! | `ctx.rpc.call(target, x)` | A call to `target`, its idempotency key the operation's ID |
//! | `ctx.sleep(ms)`           | A timer, until the request's deadline                       |
//! | `ctx.kv.*`                | Nothing: answered in the turn, from the object's state     |
//!
//! A retried attempt replays nothing: it restores the last committed image and
//! waits again on its requests, under the same idempotency keys, so a call
//! made before the crash is attached to, not made again. The run ends when
//! `main` settles: requests still outstanding (a race's loser) are dropped,
//! and their continuations never run.
use crate::{Operation, Outcome, Programs, Run, Runtime};
use restate_sdk::{
    context::{DurableFuture, RequestTarget},
    prelude::*,
};
use serde_json::Value;
use std::{
    collections::HashSet,
    pin::Pin,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

mod store;
use store::ObjectStore;

/// Fault injection, for tests.
#[doc(hidden)]
pub mod faults {
    use std::sync::atomic::{AtomicU32, Ordering};

    static SKIP: AtomicU32 = AtomicU32::new(0);
    static CRASH: AtomicU32 = AtomicU32::new(0);

    /// Let `skip` turns pass, then fail the next `crash` once they have
    /// committed, before the handler waits for anything: as if their attempt
    /// crashed there.
    pub fn crash_after_commit(skip: u32, crash: u32) {
        SKIP.store(skip, Ordering::SeqCst);
        CRASH.store(crash, Ordering::SeqCst);
    }

    /// The crashes still to inject.
    pub fn pending() -> u32 {
        CRASH.load(Ordering::SeqCst)
    }

    pub(crate) fn after_commit() -> Result<(), std::io::Error> {
        let take = |n: &AtomicU32| {
            n.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                .is_ok()
        };
        if !take(&SKIP) && take(&CRASH) {
            return Err(std::io::Error::other("Injected crash after commit"));
        }
        Ok(())
    }
}

/// A program, served as a virtual object of its name: a key is a scope.
pub struct JsObject {
    program: String,
    programs: Arc<Programs>,
}

/// A request the handler waits on: its outcome, and the time to deliver it at.
type Waiting<'ctx> = Pin<Box<dyn DurableFuture<Output = (Outcome, u64)> + Send + 'ctx>>;

#[restate_sdk::object]
impl JsObject {
    /// Run the program's `main(input)` in this scope, and return its outcome:
    /// the main promise's value, or its error as a terminal error.
    #[handler]
    async fn run(&self, ctx: ObjectContext<'_>, input: Json<Value>) -> HandlerResult<Json<Value>> {
        let run = ctx.invocation_id().to_owned();
        match self.drive(&ctx, &run, input.into_inner()).await? {
            Outcome::Success { value, .. } => Ok(Json(value)),
            Outcome::Failure { error, .. } => {
                Err(TerminalError::new(format!("{}: {}", error.name, error.message)).into())
            }
        }
    }
}

impl JsObject {
    /// Start the run with `input`, then run turns until `main` settles, each
    /// waiting for the first of the run's requests to complete.
    async fn drive<'ctx>(
        &self,
        ctx: &'ctx ObjectContext<'ctx>,
        run: &str,
        input: Value,
    ) -> HandlerResult<Outcome> {
        // One store for the whole attempt: its handler is the object's only
        // writer meanwhile, so what it read or wrote stays true.
        let store = ObjectStore::new(ctx.raw_state());
        let started = Run::start(&self.programs, &store, run, &self.program, input, now()).await;
        let mut run = committed(&store, started)?;
        let mut waiting = DurableFuturesUnordered::<Waiting<'ctx>>::new();
        let mut operations: Vec<String> = Vec::new();
        let mut awaited: HashSet<String> = HashSet::new();
        loop {
            let state = run.view().state;
            if let Some(outcome) = state.outcome {
                return Ok(outcome);
            }
            // Answer the requests the host can answer now, one per turn; wait
            // on the others.
            let mut now_ready = None;
            for operation in &state.pending {
                let id = operation.id();
                if awaited.contains(id) {
                    continue;
                }
                match operation {
                    Operation::Resume { .. } => {
                        now_ready.get_or_insert((id.to_owned(), Outcome::success(Value::Null), 0));
                    }
                    Operation::Call { name, .. } => {
                        let message =
                            format!("No host operation {name:?}: call a service with ctx.rpc.call");
                        now_ready.get_or_insert((
                            id.to_owned(),
                            Outcome::failure("Error", message),
                            0,
                        ));
                    }
                    _ => {
                        if let Some(future) = wait(ctx, operation) {
                            waiting.push(future);
                            operations.push(id.to_owned());
                            awaited.insert(id.to_owned());
                        }
                    }
                }
            }
            let (operation, outcome, at) = match now_ready {
                Some(ready) => ready,
                None => {
                    let Some((index, (outcome, at))) = waiting.next().await? else {
                        return Err(TerminalError::new("The run waits on nothing").into());
                    };
                    (operations[index].clone(), outcome, at)
                }
            };
            let delivered = run
                .deliver(&store, &operation, outcome, now().max(at))
                .await;
            committed(&store, delivered)?;
        }
    }
}

/// A turn's result. Once it committed, the crash a test injects, if any. A
/// storage failure is the attempt's, and retried; anything else would fail
/// again, the same way.
fn committed<T>(store: &ObjectStore, result: anyhow::Result<T>) -> HandlerResult<T> {
    match result {
        Ok(value) => {
            faults::after_commit()?;
            Ok(value)
        }
        Err(error) if store.failed() => Err(error.into()),
        Err(error) => Err(TerminalError::new(format!("{error:#}")).into()),
    }
}

/// Wait on Restate for `operation`: an RPC (attached to on a retried attempt,
/// by its idempotency key), or a timer.
fn wait<'ctx>(ctx: &'ctx ObjectContext<'ctx>, operation: &Operation) -> Option<Waiting<'ctx>> {
    match operation {
        Operation::Rpc { id, target, input } => {
            let target = match &target.key {
                Some(key) => RequestTarget::object(&target.service, key, &target.handler),
                None => RequestTarget::service(&target.service, &target.handler),
            };
            let call = ctx
                .request::<_, bytes::Bytes>(target, Json(input.clone()))
                .idempotency_key(id.clone())
                .call()
                .map(|result| {
                    let outcome = match result {
                        Ok(body) if body.is_empty() => Outcome::success(Value::Null),
                        Ok(body) => match serde_json::from_slice::<Value>(&body) {
                            Ok(value) => Outcome::success(value),
                            Err(error) => Outcome::failure(
                                "TypeError",
                                format!("Response is not JSON: {error}"),
                            ),
                        },
                        Err(error) => Outcome::failure("TerminalError", error.message()),
                    };
                    (outcome, 0)
                });
            Some(Box::pin(call))
        }
        Operation::Sleep { deadline, .. } => {
            let deadline = *deadline;
            let timer = ctx
                .sleep(Duration::from_millis(deadline.saturating_sub(now())))
                .map(move |result| {
                    let outcome = match result {
                        Ok(()) => Outcome::success(Value::Null),
                        Err(error) => Outcome::failure("TerminalError", error.message()),
                    };
                    (outcome, deadline)
                });
            Some(Box::pin(timer))
        }
        _ => None,
    }
}

/// The programs built into this binary (`programs/`, or `QJS_PROGRAMS` at
/// build time), each evaluated now into its golden image: in milliseconds,
/// and the same bytes on every start, as a golden image depends on nothing
/// but the program and the runtime.
pub fn embedded() -> anyhow::Result<Arc<Programs>> {
    let mut programs = Programs::new(Arc::new(Runtime::new()?));
    for (name, source) in shipped::PROGRAMS {
        programs = programs.compile(name, source)?;
    }
    Ok(Arc::new(programs))
}

mod shipped {
    include!(concat!(env!("OUT_DIR"), "/programs.rs"));
}

/// Bind each program as a virtual object of its name: `run` without journal
/// replay (a retried attempt restores the last image instead), its state
/// loaded lazily (an attempt reads only the batches it needs), and no journal
/// kept once an invocation is done (it holds every page it wrote).
pub fn bind(
    mut builder: restate_sdk::endpoint::Builder,
    programs: &Arc<Programs>,
) -> restate_sdk::endpoint::Builder {
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
                    .journal_retention(Duration::ZERO),
            ),
        );
    }
    builder
}

/// An endpoint serving `programs`, and [`crate::Claude`] for them to call.
pub fn endpoint(programs: &Arc<Programs>) -> Endpoint {
    bind(Endpoint::builder(), programs)
        .bind(crate::Claude::from_env())
        .build()
}

/// Milliseconds since the epoch: a turn's logical time never runs backwards.
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}
