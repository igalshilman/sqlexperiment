//! Durable JavaScript in Restate virtual objects. A program is a static
//! JavaScript module, embedded in the binary, served as a virtual object of
//! its name: each function it exports is a handler. Each key is a scope,
//! whose calls share its SQL database (`ctx.db`, and `ctx.kv` in it); each
//! call starts from the module as evaluated, and its memory lasts for the call.
//!
//! `<program>/<key>/<export>` calls `export(input, ctx)` in its handler,
//! exclusive and without journal replay, and returns its outcome. The
//! JavaScript runs until it waits; the handler commits the changed pages,
//! the SQL and the call's requests (a turn), in one write of the object's
//! database, then waits on Restate itself for the first request to complete,
//! and delivers it in the next turn, to the same live VM:
//!
//! | `ctx` request             | The handler waits on                                        |
//! | ------------------------- | ----------------------------------------------------------- |
//! | `ctx.rpc.call(target, x)` | A call to `target`, its idempotency key the operation's ID |
//! | `ctx.sleep(ms)`           | A timer, until the request's deadline                       |
//! | `ctx.kv.*`, `ctx.db.*`    | Nothing: answered in the turn, from the object's database  |
//!
//! A retried attempt replays nothing: it restores the last committed image and
//! waits again on its requests, under the same idempotency keys, so a call
//! made before the crash is attached to, not made again. The call ends when
//! its function settles: requests still outstanding (a race's loser) are
//! dropped, and their continuations never run.
use crate::{Operation, Outcome, Programs, Run, Runtime, Scope, vm};
use restate_sdk::{
    context::{DurableFuture, RequestTarget},
    endpoint::ContextInternal,
    prelude::*,
    service::{ServiceDefinition, macro_support},
};
use serde_json::Value;
use std::{
    collections::HashSet,
    pin::Pin,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

mod store;
use sqlite_kv::{PageCache, PageCacheConfig};
use store::StateQueue;

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

/// A program, served as a virtual object of its name: a key is an object.
pub struct JsObject {
    program: String,
    programs: Arc<Programs>,
    /// The objects' databases this process owned, kept between attempts.
    cache: PageCache,
}

/// A request the handler waits on: its outcome, and the time to deliver it at.
type Waiting<'ctx> = Pin<Box<dyn DurableFuture<Output = (Outcome, u64)> + Send + 'ctx>>;

impl JsObject {
    /// The object as Restate sees it: one exclusive handler per exported
    /// function, taking and returning JSON.
    fn definition(self) -> anyhow::Result<ServiceDefinition> {
        let handlers = self.programs.get(&self.program)?.handlers();
        let handlers: Vec<Value> = (handlers.iter())
            .map(|name| {
                serde_json::json!({
                    "name": name,
                    "input": {"contentType": "application/json", "required": false},
                    "output": {"contentType": "application/json", "setContentTypeIfEmpty": false},
                })
            })
            .collect();
        let manifest = serde_json::json!({
            "name": self.program,
            "ty": "VIRTUAL_OBJECT",
            "handlers": handlers,
        });
        let discovery = serde_json::from_value(manifest)?;
        Ok(macro_support::service_definition(
            Dispatch(Arc::new(self)),
            discovery,
        ))
    }

    /// Call `handler(input)` on this object, and return its outcome: the
    /// promise's value, or its error as a terminal error. No input is null.
    async fn call(
        &self,
        ctx: ObjectContext<'_>,
        handler: &str,
        input: bytes::Bytes,
    ) -> HandlerResult<Json<Value>> {
        let input = match input.is_empty() {
            true => Value::Null,
            false => serde_json::from_slice(&input)
                .map_err(|error| TerminalError::new(format!("The input is not JSON: {error}")))?,
        };
        let run = ctx.invocation_id().to_owned();
        match self.drive(&ctx, &run, handler, input).await? {
            Outcome::Success { value, .. } => Ok(Json(value)),
            Outcome::Failure { error, .. } => {
                Err(TerminalError::new(format!("{}: {}", error.name, error.message)).into())
            }
        }
    }
}

/// Routes each of the object's handlers to [`JsObject::call`].
struct Dispatch(Arc<JsObject>);

impl restate_sdk::service::Service for Dispatch {
    type Future = macro_support::ServiceBoxFuture;

    fn handle(&self, ctx: ContextInternal) -> Self::Future {
        let object = self.0.clone();
        Box::pin(async move {
            let handler = ctx.handler_name().to_owned();
            let program = object.programs.get(&object.program);
            if !program.is_ok_and(|program| program.handlers().contains(&handler)) {
                return Err(restate_sdk::endpoint::Error::unknown_handler(
                    ctx.service_name(),
                    &handler,
                ));
            }
            let (input, metadata) = ctx.input::<bytes::Bytes>().await;
            let result = object.call((&ctx, metadata).into(), &handler, input).await;
            ctx.handle_handler_result(result);
            ctx.end();
            Ok(())
        })
    }
}

impl JsObject {
    /// Start the call with `input`, then run turns until `handler` settles,
    /// each waiting for the first of the call's requests to complete.
    async fn drive<'ctx>(
        &self,
        ctx: &'ctx ObjectContext<'ctx>,
        run: &str,
        handler: &str,
        input: Value,
    ) -> HandlerResult<Outcome> {
        // One store for the whole attempt: its handler is the object's only
        // writer meanwhile, so what it read or wrote stays true.
        let store = Arc::new(StateQueue::new(ctx.raw_state()));
        let namespace = format!("{}/{}", self.program, ctx.key());
        let scope = Scope::new(store.clone()).cached(self.cache.namespace(&namespace));
        let started = Run::start(
            &self.programs,
            &scope,
            ctx.key(),
            run,
            &self.program,
            handler,
            input,
            now(),
        )
        .await;
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
            let (outcomes, at) = match now_ready {
                Some((operation, outcome, at)) => (vec![(operation, outcome)], at),
                None => {
                    let Some((index, (outcome, mut at))) = waiting.next().await? else {
                        return Err(TerminalError::new("The run waits on nothing").into());
                    };
                    // What else has arrived goes in the same turn, and commits
                    // with it.
                    let mut outcomes = vec![(operations[index].clone(), outcome)];
                    while let Some(ready) = futures::FutureExt::now_or_never(waiting.next()) {
                        let Some((index, (outcome, ready_at))) = ready? else {
                            break;
                        };
                        outcomes.push((operations[index].clone(), outcome));
                        at = at.max(ready_at);
                    }
                    (outcomes, at)
                }
            };
            let delivered = run.deliver_all(outcomes, now().max(at)).await;
            committed(&store, delivered)?;
        }
    }
}

/// A turn's result. Once it committed, the crash a test injects, if any. A
/// storage failure is the attempt's, and retried; anything else would fail
/// again, the same way.
fn committed<T>(store: &StateQueue, result: anyhow::Result<T>) -> HandlerResult<T> {
    match result {
        Ok(value) => {
            faults::after_commit()?;
            Ok(value)
        }
        Err(error) if store.failed() || vm::exhausted(&error) => Err(error.into()),
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

/// The programs built into this binary (`examples-functions/`, or `FUNCTIONS_PROGRAMS` at
/// build time: `examples-functions-python/` for `make functions-python`), each evaluated now into its golden image: in milliseconds
/// (JavaScript) or a tenth of a second (Python), and the same bytes on every
/// start, as a golden image depends on nothing but the program and the
/// runtime. Python programs need the engine `make python-engine` builds. `FUNCTIONS_TURN_TIMEOUT` (`90s`, `10m`, `1h`)
/// changes how long one turn may run, 10 minutes by default.
pub fn embedded() -> anyhow::Result<Arc<Programs>> {
    use anyhow::Context as _;
    let mut runtime = Runtime::new()?;
    if let Ok(timeout) = std::env::var("FUNCTIONS_TURN_TIMEOUT") {
        let turn = duration(&timeout).with_context(|| {
            format!("FUNCTIONS_TURN_TIMEOUT={timeout}: expected e.g. 90s, 10m, 1h")
        })?;
        let limits = crate::Limits {
            turn,
            ..runtime.limits()
        };
        runtime = runtime.limited(limits);
    }
    let mut programs = Programs::new(Arc::new(runtime));
    for (name, language, source) in shipped::PROGRAMS {
        let program = crate::Program::compile_in(programs.runtime(), *language, name, source)
            .with_context(|| format!("Program {name}"))?;
        programs = programs.with(program)?;
    }
    Ok(Arc::new(programs))
}

/// `90s`, `10m` or `1h`.
fn duration(text: &str) -> Option<Duration> {
    let split = text.find(|c: char| !c.is_ascii_digit())?;
    let n: u64 = text[..split].parse().ok()?;
    let unit = match &text[split..] {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        _ => return None,
    };
    Some(Duration::from_secs(n * unit))
}

mod shipped {
    include!(concat!(env!("OUT_DIR"), "/programs.rs"));
}

/// Bind each program as a virtual object of its name: its handlers without journal
/// replay (a retried attempt restores the last image instead), its state
/// loaded lazily (an attempt reads only what its database needs), no
/// journal kept once an invocation is done (it holds every page it wrote),
/// and Restate's inactivity timeout as long as a turn may run.
/// The objects share `cache`, which keeps the scopes this process owns.
pub fn bind(
    mut builder: restate_sdk::endpoint::Builder,
    programs: &Arc<Programs>,
    cache: &PageCache,
) -> anyhow::Result<restate_sdk::endpoint::Builder> {
    for name in programs.names() {
        let object = JsObject {
            program: name.to_owned(),
            programs: programs.clone(),
            cache: cache.clone(),
        };
        builder = builder.bind(
            object.definition()?.options(
                ServiceOptions::new()
                    .journal_replay(false)
                    .enable_lazy_state(true)
                    .journal_retention(Duration::ZERO)
                    .inactivity_timeout(programs.runtime().limits().turn),
            ),
        );
    }
    Ok(builder)
}

/// An endpoint serving `programs`, and [`crate::Claude`] and
/// [`crate::Workspace`] for them to call.
pub async fn endpoint(programs: &Arc<Programs>) -> anyhow::Result<Endpoint> {
    Ok(bind(Endpoint::builder(), programs, &page_cache().await?)?
        .bind(crate::Claude::from_env())
        .bind(crate::Workspace::from_env())
        .build())
}

/// A cache of the scopes' databases for this process: 64 MiB in memory, and
/// 256 MiB in a file that goes away with the process.
pub async fn page_cache() -> anyhow::Result<PageCache> {
    let file = std::env::temp_dir().join(format!("functions-{}.cache", std::process::id()));
    let cache = PageCache::open(PageCacheConfig::new(&file, 64 << 20, 256 << 20)).await?;
    // The cache keeps the file open: unlinked now, it goes away with the
    // process however it ends.
    let _ = std::fs::remove_file(&file);
    Ok(cache)
}

/// Milliseconds since the epoch: a turn's logical time never runs backwards.
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    #[test]
    fn durations_read_as_written() {
        use std::time::Duration;
        assert_eq!(super::duration("90s"), Some(Duration::from_secs(90)));
        assert_eq!(super::duration("10m"), Some(Duration::from_secs(600)));
        assert_eq!(super::duration("1h"), Some(Duration::from_secs(3600)));
        for bad in ["", "10", "m", "1.5m", "10min"] {
            assert_eq!(super::duration(bad), None, "{bad}");
        }
    }
}
