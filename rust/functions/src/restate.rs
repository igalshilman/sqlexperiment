//! Programs as Restate virtual objects. A program is a static JavaScript or
//! Python module, embedded in the binary, served as a virtual object of its
//! name: each function it exports is a handler. Each key is a scope, whose
//! calls share its SQL database (`ctx.db`, and `ctx.kv` in it); each call
//! starts from the module as evaluated, and its memory lasts for the call.
//!
//! `<program>/<key>/<export>` calls `export(input, ctx)` in its handler,
//! exclusive and without journal replay, and returns its outcome. The
//! program runs until it waits; the handler flushes the changed pages, the
//! SQL and the call's requests (a turn) to the object's state, in one flush,
//! then waits on Restate itself for the first request to complete,
//! and delivers it in the next turn, to the same live VM:
//!
//! | `ctx` request              | The handler waits on                                          |
//! | -------------------------- | ------------------------------------------------------------- |
//! | `ctx.rpc.call(target, x)`  | A call to `target`, under the request's idempotency key       |
//! | `ctx.call(name, x)`, ...   | The extension `name`'s operation ([`crate::Extensions`])      |
//! | `ctx.sleep(ms)`            | A timer, until the request's deadline                         |
//! | `ctx.signal(name, x)`      | A wait in `function_signals`, which `signal` resolves         |
//! | `ctx.kv.*`, `ctx.db.*`     | Nothing: answered in the turn, from the object's database    |
//!
//! A call's input stays in the guest's heap until the call is answered: the
//! handler reads it when it makes the call, so the commit holds none, and an
//! extension's operation is journaled nowhere. A request's idempotency key is
//! the guest's, if it set one, or else its operation ID: the same on every
//! attempt, since a request is made only once the turn that issued it has
//! committed.
//!
//! A retried attempt replays nothing: it restores the last committed image and
//! makes its requests again, under the same idempotency keys, so a call
//! made before the crash is attached to, not made again (an extension's
//! operation that does not deduplicate is made again). The call ends when
//! its function settles: requests still outstanding (a race's loser) are
//! dropped, and their continuations never run; a signal no one gave is
//! forgotten.
//!
//! Besides its functions, each object has `pending`, the signals its runs
//! wait for, and `signal`, which gives one: by its wait's id, or else to the
//! oldest wait for its name ([`Signal`]).
use crate::{
    Budget, Extensions, FunctionSignals, Making, Operation, Outcome, Programs, Run, Runtime, Scope,
    budget::Lease,
    signals::{FunctionSignalsClient, Signal, Waiting, signals_of},
    vm,
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
use state::{PageCache, PageCacheConfig};
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

/// The handlers every object has, besides its program's functions.
pub const BUILT_IN: [&str; 2] = ["pending", "signal"];

/// A program, served as a virtual object of its name: a key is an object.
struct ProgramObject {
    program: String,
    programs: Arc<Programs>,
    /// The objects' databases this process owned, kept between attempts.
    cache: PageCache,
    /// The memory live runs may hold.
    budget: Arc<Budget>,
    extensions: Extensions,
}

impl ProgramObject {
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

    /// `pending` (the waits of this object's runs, as [`Waiting`]) or
    /// `signal` (give one, [`Signal`]), through `function_signals`.
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

    /// Start the run `run_id` with `input`, then run turns until `handler`
    /// settles, each delivering what arrived while the run waited.
    ///
    /// Each turn first reserves its memory in the budget; the run then holds
    /// what it keeps live until it parks, which it may while it waits.
    async fn drive<'ctx>(
        &self,
        ctx: &'ctx ObjectContext<'ctx>,
        run_id: &str,
        handler: &str,
        input: Value,
    ) -> HandlerResult<Outcome> {
        // One store for the whole attempt: its handler is the object's only
        // writer meanwhile, so what it read or wrote stays true.
        let store = Arc::new(StateQueue::new(ctx.raw_state()));
        let namespace = format!("{}/{}", self.program, ctx.key());
        let scope = (Scope::new(store.clone()).cached(&self.cache, &namespace))
            .extended(self.extensions.clone());
        let mut lease = self.budget.lease();
        lease.reserve(self.budget.peak(&self.program)).await;
        let started = Run::start(
            &self.programs,
            &scope,
            ctx.key(),
            run_id,
            &self.program,
            handler,
            input,
            now(),
        )
        .await;
        let mut run = committed(&store, started)?;
        self.budget.record(&self.program, run.turn_peak());
        lease.hold(run.resident());
        let signals_key = signals_of(&self.program, ctx.key());
        let mut in_flight = InFlight::new();
        loop {
            let state = run.view().state;
            if let Some(outcome) = state.outcome {
                in_flight.forget_signals(ctx, signals_key);
                return Ok(outcome);
            }
            // Making a call reads its input from the heap: a run that is not
            // live restores first, within the budget, as for a delivery.
            if in_flight.reads_heap(&state.pending) && run.resident() == 0 {
                lease
                    .reserve(self.budget.peak(&self.program).max(run.footprint()))
                    .await;
            }
            let answered = in_flight
                .make(ctx, &signals_key, &store, &mut run, &state.pending)
                .await?;
            lease.hold(run.resident());
            let (outcomes, at) = match answered {
                Some(answer) => (vec![answer], 0),
                None => in_flight.arrived(&mut run, &mut lease).await?,
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
                in_flight.signals.remove(operation);
            }
            let delivered = run.deliver_all(outcomes, now().max(at)).await;
            self.budget.record(&self.program, run.turn_peak());
            lease.hold(run.resident());
            committed(&store, delivered)?;
        }
    }
}

/// Routes each of the object's handlers to [`ProgramObject::call`], or a
/// built-in one to [`ProgramObject::signals`].
struct Dispatch(Arc<ProgramObject>);

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

/// A request Restate completes (a call, a timer, a signal's wait): its
/// outcome, and the time to deliver it at.
type RestateFuture<'ctx> = Pin<Box<dyn DurableFuture<Output = (Outcome, u64)> + Send + 'ctx>>;
/// An extension's operation, which the handler makes itself: its ID and
/// outcome.
type ExtensionFuture = BoxFuture<'static, (String, Outcome)>;

/// The requests this attempt has made for its run, until they are delivered.
struct InFlight<'ctx> {
    /// Restate's futures, and the operation of each, by its index.
    restate: DurableFuturesUnordered<RestateFuture<'ctx>>,
    operations: Vec<String>,
    extensions: FuturesUnordered<ExtensionFuture>,
    /// Every operation made, delivered or not: none is made twice.
    made: HashSet<String>,
    /// The signals waited for, until delivered: those left when the run
    /// settles are forgotten.
    signals: HashSet<String>,
}

impl<'ctx> InFlight<'ctx> {
    fn new() -> Self {
        Self {
            restate: DurableFuturesUnordered::new(),
            operations: Vec::new(),
            extensions: FuturesUnordered::new(),
            made: HashSet::new(),
            signals: HashSet::new(),
        }
    }

    /// Whether making `pending` reads a call's input from the run's heap.
    fn reads_heap(&self, pending: &[Operation]) -> bool {
        (pending.iter())
            .filter(|operation| !self.made.contains(operation.id()))
            .any(|operation| matches!(operation, Operation::Rpc { .. } | Operation::Call { .. }))
    }

    /// Make each of `pending` not made yet. One the host answers now (a
    /// wakeup, or a call that fails before it is made) is not made: the
    /// first such is returned, to deliver in the next turn on its own.
    async fn make(
        &mut self,
        ctx: &'ctx ObjectContext<'ctx>,
        signals_key: &str,
        store: &StateQueue,
        run: &mut Run,
        pending: &[Operation],
    ) -> HandlerResult<Option<(String, Outcome)>> {
        let mut answered = None;
        for operation in pending {
            let id = operation.id();
            if self.made.contains(id) {
                continue;
            }
            let made = match operation {
                Operation::Resume { .. } => Err(Outcome::success(Value::Null)),
                Operation::Sleep { deadline, .. } => Ok(Made::Restate(sleep(ctx, *deadline))),
                Operation::Signal { name, input, .. } => {
                    self.signals.insert(id.to_owned());
                    let key = signals_key.to_owned();
                    Ok(Made::Restate(signal(ctx, key, operation, name, input)))
                }
                Operation::Rpc { target, .. } => (run.input(id).await)
                    .map_err(|error| failed(store, error))?
                    .map(|input| Made::Restate(rpc(ctx, operation, target, input)))
                    .map_err(|error| Outcome::from_result(Err(error))),
                Operation::Call { .. } => (run.operation(id).await)
                    .map_err(|error| failed(store, error))?
                    .map(|making| Made::Extension(extension(id, making)))
                    .map_err(|error| Outcome::from_result(Err(error))),
                // Answered in the turn: never pending.
                Operation::KvGet { .. } | Operation::KvSet { .. } | Operation::KvDelete { .. } => {
                    continue;
                }
            };
            match made {
                Ok(Made::Restate(future)) => {
                    self.restate.push(future);
                    self.operations.push(id.to_owned());
                    self.made.insert(id.to_owned());
                }
                Ok(Made::Extension(future)) => {
                    self.extensions.push(future);
                    self.made.insert(id.to_owned());
                }
                Err(outcome) => {
                    answered.get_or_insert((id.to_owned(), outcome));
                }
            }
        }
        Ok(answered)
    }

    /// Wait for the first outcome, parking the run if the budget asks it to
    /// meanwhile; then take what else has arrived, to deliver in the same
    /// turn: the outcomes, and the latest time among them.
    async fn arrived(
        &mut self,
        run: &mut Run,
        lease: &mut Lease,
    ) -> HandlerResult<(Vec<(String, Outcome)>, u64)> {
        if self.restate.is_empty() && self.extensions.is_empty() {
            return Err(TerminalError::new("The run waits on nothing").into());
        }
        let park = lease.wait();
        let (first, mut at) = loop {
            tokio::select! {
                ready = self.restate.next(), if !self.restate.is_empty() => {
                    let (index, (outcome, at)) = ready?.expect("a request to wait on");
                    break ((self.operations[index].clone(), outcome), at);
                }
                Some(made) = self.extensions.next() => break (made, 0),
                () = park.notified(), if run.resident() > 0 => {
                    run.park();
                    lease.hold(0);
                }
            }
        };
        lease.woken();
        let mut outcomes = vec![first];
        while let Some(ready) = futures::FutureExt::now_or_never(self.restate.next()) {
            let Some((index, (outcome, ready_at))) = ready? else {
                break;
            };
            outcomes.push((self.operations[index].clone(), outcome));
            at = at.max(ready_at);
        }
        while let Some(Some(made)) = futures::FutureExt::now_or_never(self.extensions.next()) {
            outcomes.push(made);
        }
        Ok((outcomes, at))
    }

    /// The run settled: the signals it still waits for are forgotten.
    fn forget_signals(self, ctx: &ObjectContext<'_>, signals_key: String) {
        if !self.signals.is_empty() {
            let ids: Vec<String> = self.signals.into_iter().collect();
            (ctx.object_client::<FunctionSignalsClient>(signals_key))
                .forget(Json(ids))
                .send();
        }
    }
}

/// A request, made: a future of Restate's, or an extension's operation.
enum Made<'ctx> {
    Restate(RestateFuture<'ctx>),
    Extension(ExtensionFuture),
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
) -> RestateFuture<'ctx> {
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

/// An extension's operation `id`, made: its outcome, as the guest sees it.
fn extension(id: &str, making: Making) -> ExtensionFuture {
    let id = id.to_owned();
    Box::pin(async move { (id, Outcome::from_result(making.await)) })
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
) -> RestateFuture<'ctx> {
    let waiting = Waiting {
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
fn sleep<'ctx>(ctx: &'ctx ObjectContext<'ctx>, deadline: u64) -> RestateFuture<'ctx> {
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
        let program = crate::Program::compile(programs.runtime(), *language, name, source)
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
/// `extensions`.
pub fn bind(
    mut builder: restate_sdk::endpoint::Builder,
    programs: &Arc<Programs>,
    cache: &PageCache,
    budget: &Arc<Budget>,
    extensions: &Extensions,
) -> anyhow::Result<restate_sdk::endpoint::Builder> {
    for name in programs.names() {
        let object = ProgramObject {
            program: name.to_owned(),
            programs: programs.clone(),
            cache: cache.clone(),
            budget: budget.clone(),
            extensions: extensions.clone(),
        };
        let mut options = ServiceOptions::new()
            .journal_replay(false)
            // TODO: preload the head eagerly (`eagerStateKeysWhitelist`), see FUTURE.md.
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

/// An endpoint serving `programs`, with the extensions `fetch` (under
/// [`crate::FetchPolicy::from_env`]), `llm` and `fs`, and
/// [`crate::Workspace`] for them to call.
pub async fn endpoint(programs: &Arc<Programs>) -> anyhow::Result<Endpoint> {
    let budget = Budget::from_env()?;
    let fetch = crate::Fetch::new(crate::FetchPolicy::from_env());
    let extensions = (Extensions::new().with("fetch", fetch))
        .with("llm", crate::Llm::from_env())
        .with("fs", crate::Files::new())
        .with_blobs(crate::DirBlobs::from_env());
    let cache = page_cache().await?;
    Ok(
        bind(Endpoint::builder(), programs, &cache, &budget, &extensions)?
            .bind(crate::Workspace::from_env())
            .build(),
    )
}

/// A cache of the scopes' databases for this process: 64 MiB in memory, and
/// 256 MiB in a file that goes away with the process.
pub async fn page_cache() -> anyhow::Result<PageCache> {
    Ok(PageCache::open(PageCacheConfig::new(64 << 20, 256 << 20)).await?)
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
