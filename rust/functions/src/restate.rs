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
//! | `ctx` request              | The handler waits on                                          |
//! | -------------------------- | ------------------------------------------------------------- |
//! | `ctx.rpc.call(target, x)`  | A call to `target`, under the request's idempotency key       |
//! | `ctx.call(name, x)`        | The host binding `name` ([`crate::Bindings`]), called here    |
//! | `ctx.sleep(ms)`            | A timer, until the request's deadline                         |
//! | `ctx.signal(name, x)`      | A wait in `function_signals`, which `signal` resolves         |
//! | `ctx.kv.*`, `ctx.db.*`     | Nothing: answered in the turn, from the object's database    |
//!
//! A call's input stays in the guest's heap until the call is answered: the
//! handler reads it when it makes the call, so the commit holds none, and a
//! binding's call is journaled nowhere. A request's idempotency key is the
//! guest's, if it set one, or else its operation ID: the same on every
//! attempt, since a request is made only once the turn that issued it has
//! committed.
//!
//! A retried attempt replays nothing: it restores the last committed image and
//! makes its requests again, under the same idempotency keys, so a call
//! made before the crash is attached to, not made again (a binding that
//! does not deduplicate is called again). The call ends when
//! its function settles: requests still outstanding (a race's loser) are
//! dropped, and their continuations never run; a signal no one gave is
//! forgotten.
//!
//! Besides its functions, each object has `pending`, the signals its runs
//! wait for, and `signal`, which gives one: by its wait's id, or else to the
//! oldest wait for its name ([`crate::Signal`]).
use crate::{
    Binding, Bindings, Budget, FunctionSignals, HostCall, Operation, Outcome, Programs, Run,
    Runtime, Scope, Signal, Waiting as Wait, signals::FunctionSignalsClient, signals_of, vm,
};
use futures::{StreamExt, future::BoxFuture, stream::FuturesUnordered};
use restate_sdk::{
    context::{DurableFuture, RequestTarget},
    endpoint::{ContextInternal, HandlerOptions},
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
    /// The memory live runs may hold.
    budget: Arc<Budget>,
    bindings: Bindings,
}

/// A request the handler waits on: its outcome, and the time to deliver it at.
type Waiting<'ctx> = Pin<Box<dyn DurableFuture<Output = (Outcome, u64)> + Send + 'ctx>>;
/// A binding's call, which the handler makes itself: its operation and outcome.
type Calling = BoxFuture<'static, (String, Outcome)>;

impl JsObject {
    /// The object as Restate sees it: one exclusive handler per exported
    /// function, taking and returning JSON.
    fn definition(self) -> anyhow::Result<ServiceDefinition> {
        let handlers = self.programs.get(&self.program)?.handlers();
        let handler = |name: &str, ty: &str| {
            serde_json::json!({
                "name": name,
                "ty": ty,
                "input": {"contentType": "application/json", "required": false},
                "output": {"contentType": "application/json", "setContentTypeIfEmpty": false},
            })
        };
        let mut handlers: Vec<Value> = (handlers.iter())
            .map(|name| handler(name, "EXCLUSIVE"))
            .collect();
        // Every object's: what its runs wait for, and a signal to them.
        handlers.extend(BUILT_IN.iter().map(|name| handler(name, "SHARED")));
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

/// The handlers every object has, besides its program's functions.
pub const BUILT_IN: [&str; 2] = ["pending", "signal"];

impl JsObject {
    /// `pending` (the waits of this object's runs, as [`Wait`]) or `signal`
    /// (give one, [`Signal`]), through `function_signals`.
    async fn signals(
        &self,
        ctx: SharedObjectContext<'_>,
        handler: &str,
        input: bytes::Bytes,
    ) -> HandlerResult<Json<Value>> {
        let signals =
            ctx.object_client::<FunctionSignalsClient>(signals_of(&self.program, ctx.key()));
        if handler == "pending" {
            return Ok(Json(serde_json::to_value(
                signals.pending().call().await?.into_inner(),
            )?));
        }
        let signal: Signal = match input.is_empty() {
            true => Signal::default(),
            false => serde_json::from_slice(&input).map_err(|error| {
                TerminalError::new(format!(
                    "Expected a signal {{id?, name?, value, keep?}}: {error}"
                ))
            })?,
        };
        let resolved = signals.resolve(Json(signal)).call().await?.into_inner();
        Ok(Json(serde_json::to_value(resolved)?))
    }
}

/// Routes each of the object's handlers to [`JsObject::call`], or a built-in one to [`JsObject::signals`].
struct Dispatch(Arc<JsObject>);

impl restate_sdk::service::Service for Dispatch {
    type Future = macro_support::ServiceBoxFuture;

    fn handle(&self, ctx: ContextInternal) -> Self::Future {
        let object = self.0.clone();
        Box::pin(async move {
            let handler = ctx.handler_name().to_owned();
            if BUILT_IN.contains(&handler.as_str()) {
                let (input, metadata) = ctx.input::<bytes::Bytes>().await;
                let result = object
                    .signals((&ctx, metadata).into(), &handler, input)
                    .await;
                ctx.handle_handler_result(result);
                ctx.end();
                return Ok(());
            }
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
        // Each turn first reserves what it may take; then the run holds its
        // memory until it parks.
        let mut lease = self.budget.lease();
        lease.reserve(self.budget.peak(&self.program)).await;
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
        self.budget.record(&self.program, run.turn_peak());
        lease.hold(run.resident());
        let mut waiting = DurableFuturesUnordered::<Waiting<'ctx>>::new();
        let mut operations: Vec<String> = Vec::new();
        let mut calls = FuturesUnordered::<Calling>::new();
        let mut awaited: HashSet<String> = HashSet::new();
        // The signals this attempt waits for, until delivered: those left
        // when the run settles are forgotten.
        let mut signals: HashSet<String> = HashSet::new();
        loop {
            let state = run.view().state;
            if let Some(outcome) = state.outcome {
                if !signals.is_empty() {
                    let ids: Vec<String> = signals.into_iter().collect();
                    ctx.object_client::<FunctionSignalsClient>(signals_of(
                        &self.program,
                        ctx.key(),
                    ))
                    .forget(Json(ids))
                    .send();
                }
                return Ok(outcome);
            }
            // Making a call reads its input from the heap: a run that is not
            // live restores first, within the budget, as for a delivery.
            let reads = (state.pending.iter())
                .filter(|operation| !awaited.contains(operation.id()))
                .any(|operation| {
                    matches!(operation, Operation::Rpc { .. } | Operation::Call { .. })
                });
            if reads && run.resident() == 0 {
                lease
                    .reserve(self.budget.peak(&self.program).max(run.footprint()))
                    .await;
            }
            // Answer the requests the host can answer now, one per turn; make
            // the others, and wait on them.
            let mut now_ready = None;
            for operation in &state.pending {
                let id = operation.id();
                if awaited.contains(id) {
                    continue;
                }
                let made = match operation {
                    Operation::Resume { .. } => Err(Outcome::success(Value::Null)),
                    Operation::Sleep { deadline, .. } => Ok(Made::Waiting(sleep(ctx, *deadline))),
                    Operation::Signal { name, input, .. } => {
                        signals.insert(id.to_owned());
                        let key = signals_of(&self.program, ctx.key());
                        Ok(Made::Waiting(signal(ctx, key, operation, name, input)))
                    }
                    Operation::Rpc { target, .. } => (run.input(id).await)
                        .map_err(|error| failed(&store, error))?
                        .map(|input| Made::Waiting(rpc(ctx, operation, target, input)))
                        .map_err(|error| Outcome::failure(error.name, error.message)),
                    Operation::Call { name, .. } => match self.bindings.get(name) {
                        None => Err(Outcome::failure(
                            "Error",
                            format!("No host binding {name:?}"),
                        )),
                        Some(binding)
                            if operation.idempotency_key() != id && !binding.deduplicates() =>
                        {
                            let message =
                                format!("{name} does not deduplicate: it takes no idempotency key");
                            Err(Outcome::failure("TypeError", message))
                        }
                        Some(binding) => (run.input(id).await)
                            .map_err(|error| failed(&store, error))?
                            .map(|input| Made::Calling(call(&**binding, operation, input)))
                            .map_err(|error| Outcome::failure(error.name, error.message)),
                    },
                    // Answered in the turn: never pending.
                    Operation::KvGet { .. }
                    | Operation::KvSet { .. }
                    | Operation::KvDelete { .. } => {
                        continue;
                    }
                };
                match made {
                    Ok(Made::Waiting(future)) => {
                        waiting.push(future);
                        operations.push(id.to_owned());
                        awaited.insert(id.to_owned());
                    }
                    Ok(Made::Calling(call)) => {
                        calls.push(call);
                        awaited.insert(id.to_owned());
                    }
                    Err(outcome) => {
                        now_ready.get_or_insert((id.to_owned(), outcome, 0));
                    }
                }
            }
            lease.hold(run.resident());
            let (outcomes, at) = match now_ready {
                Some((operation, outcome, at)) => (vec![(operation, outcome)], at),
                None => {
                    if waiting.is_empty() && calls.is_empty() {
                        return Err(TerminalError::new("The run waits on nothing").into());
                    }
                    // While it waits, the run may park to free its memory.
                    let park = lease.wait();
                    let (first, mut at) = loop {
                        tokio::select! {
                            ready = waiting.next(), if !waiting.is_empty() => {
                                let (index, (outcome, at)) = ready?.expect("a request to wait on");
                                break ((operations[index].clone(), outcome), at);
                            }
                            Some(called) = calls.next() => break (called, 0),
                            () = park.notified(), if run.resident() > 0 => {
                                run.park();
                                lease.hold(0);
                            }
                        }
                    };
                    lease.woken();
                    // What else has arrived goes in the same turn, and commits
                    // with it.
                    let mut outcomes = vec![first];
                    while let Some(ready) = futures::FutureExt::now_or_never(waiting.next()) {
                        let Some((index, (outcome, ready_at))) = ready? else {
                            break;
                        };
                        outcomes.push((operations[index].clone(), outcome));
                        at = at.max(ready_at);
                    }
                    while let Some(Some(called)) = futures::FutureExt::now_or_never(calls.next()) {
                        outcomes.push(called);
                    }
                    (outcomes, at)
                }
            };
            let peak = self.budget.peak(&self.program).max(run.footprint());
            if !lease.try_reserve(peak) {
                // Wait parked: a run that waits to fit holding its memory
                // could keep the turns before it from ever fitting.
                run.park();
                lease.hold(0);
                lease.reserve(peak).await;
            }
            for (operation, _) in &outcomes {
                signals.remove(operation);
            }
            let delivered = run.deliver_all(outcomes, now().max(at)).await;
            self.budget.record(&self.program, run.turn_peak());
            lease.hold(run.resident());
            committed(&store, delivered)?;
        }
    }
}

/// A request, made: a future of Restate's, or a binding's call.
enum Made<'ctx> {
    Waiting(Waiting<'ctx>),
    Calling(Calling),
}

/// A turn's result. Once it committed, the crash a test injects, if any.
fn committed<T>(store: &StateQueue, result: anyhow::Result<T>) -> HandlerResult<T> {
    let value = result.map_err(|error| failed(store, error))?;
    faults::after_commit()?;
    Ok(value)
}

/// A storage failure, or a full pool, is the attempt's, and retried; anything
/// else would fail again, the same way.
fn failed(store: &StateQueue, error: anyhow::Error) -> HandlerError {
    if store.failed() || vm::exhausted(&error) {
        error.into()
    } else {
        TerminalError::new(format!("{error:#}")).into()
    }
}

/// A call to `target` with `input`, the guest's `operation`: attached to on a
/// retried attempt, by its idempotency key.
fn rpc<'ctx>(
    ctx: &'ctx ObjectContext<'ctx>,
    operation: &Operation,
    target: &crate::RpcTarget,
    input: Value,
) -> Waiting<'ctx> {
    let target = match &target.key {
        Some(key) => RequestTarget::object(&target.service, key, &target.handler),
        None => RequestTarget::service(&target.service, &target.handler),
    };
    let call = ctx
        .request::<_, bytes::Bytes>(target, Json(input))
        .idempotency_key(operation.idempotency_key().to_owned())
        .call()
        .map(|result| {
            let outcome = match result {
                Ok(body) if body.is_empty() => Outcome::success(Value::Null),
                Ok(body) => match serde_json::from_slice::<Value>(&body) {
                    Ok(value) => Outcome::success(value),
                    Err(error) => {
                        Outcome::failure("TypeError", format!("Response is not JSON: {error}"))
                    }
                },
                Err(error) => Outcome::failure("TerminalError", error.message()),
            };
            (outcome, 0)
        });
    Box::pin(call)
}

/// A call of `binding` with `input`, the guest's `operation`: made here, under
/// the operation's idempotency key.
fn call(binding: &dyn Binding, operation: &Operation, input: Value) -> Calling {
    let id = operation.id().to_owned();
    let call = binding.call(HostCall {
        id: id.clone(),
        idempotency_key: operation.idempotency_key().to_owned(),
        input,
    });
    Box::pin(async move {
        let outcome = match call.await {
            Ok(value) => Outcome::success(value),
            Err(error) => Outcome::failure(error.name, error.message),
        };
        (id, outcome)
    })
}

/// A wait for the signal `name`, in `function_signals` under `key`: the
/// guest's `operation`, attached to on a retried attempt, by its idempotency
/// key.
fn signal<'ctx>(
    ctx: &'ctx ObjectContext<'ctx>,
    key: String,
    operation: &Operation,
    name: &str,
    input: &Value,
) -> Waiting<'ctx> {
    let waiting = Wait {
        id: operation.id().to_owned(),
        name: name.to_owned(),
        input: input.clone(),
        since: 0,
    };
    let wait = ctx
        .object_client::<FunctionSignalsClient>(key)
        .wait(Json(waiting))
        .idempotency_key(operation.idempotency_key().to_owned())
        .call()
        .map(|result| {
            let outcome = match result {
                Ok(Json(value)) => Outcome::success(value),
                Err(error) => Outcome::failure("TerminalError", error.message()),
            };
            (outcome, 0)
        });
    Box::pin(wait)
}

/// A timer until `deadline`.
fn sleep<'ctx>(ctx: &'ctx ObjectContext<'ctx>, deadline: u64) -> Waiting<'ctx> {
    let timer = ctx
        .sleep(Duration::from_millis(deadline.saturating_sub(now())))
        .map(move |result| {
            let outcome = match result {
                Ok(()) => Outcome::success(Value::Null),
                Err(error) => Outcome::failure("TerminalError", error.message()),
            };
            (outcome, deadline)
        });
    Box::pin(timer)
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
/// and Restate's inactivity timeout as long as a turn may run; and
/// `function_signals`, which keeps their waits for signals.
/// The objects share `cache`, which keeps the scopes this process owns, and
/// `budget`, the memory their live runs may hold; they offer their programs
/// `bindings`.
pub fn bind(
    mut builder: restate_sdk::endpoint::Builder,
    programs: &Arc<Programs>,
    cache: &PageCache,
    budget: &Arc<Budget>,
    bindings: &Bindings,
) -> anyhow::Result<restate_sdk::endpoint::Builder> {
    for name in programs.names() {
        let object = JsObject {
            program: name.to_owned(),
            programs: programs.clone(),
            cache: cache.clone(),
            budget: budget.clone(),
            bindings: bindings.clone(),
        };
        let mut options = ServiceOptions::new()
            .journal_replay(false)
            .enable_lazy_state(true)
            .journal_retention(Duration::ZERO)
            .inactivity_timeout(programs.runtime().limits().turn);
        // They call `function_signals`: a retried attempt replays its call.
        for name in BUILT_IN {
            options = options.handler(name, HandlerOptions::new().journal_replay(true));
        }
        builder = builder.bind(object.definition()?.options(options));
    }
    Ok(builder.bind(FunctionSignals))
}

/// An endpoint serving `programs`, with `fetch` under
/// [`crate::FetchPolicy::from_env`], and [`crate::Workspace`] for them to
/// call.
pub async fn endpoint(programs: &Arc<Programs>) -> anyhow::Result<Endpoint> {
    let budget = Budget::from_env()?;
    let fetch = crate::Fetch::new(crate::FetchPolicy::from_env());
    let bindings = (Bindings::new().with("fetch", fetch)).with("llm", crate::Llm::from_env());
    let cache = page_cache().await?;
    Ok(
        bind(Endpoint::builder(), programs, &cache, &budget, &bindings)?
            .bind(crate::Workspace::from_env())
            .build(),
    )
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
