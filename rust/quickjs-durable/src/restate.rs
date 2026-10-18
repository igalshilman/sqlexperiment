//! Durable JavaScript in Restate virtual objects. A program is a static
//! JavaScript file defining `main(input, ctx)`, which the build script
//! evaluates into its golden memory image, embedded with it. Each program is
//! served as a virtual object of its name, whose key is a scope: its runs
//! share their `ctx.kv`, kept with the runs' images in one SQL database
//! (sqlite-kv) in the object's state.
//!
//! `<program>/<key>/run` runs `main(input)` in its handler, exclusive and
//! without journal replay, and returns its outcome. The JavaScript runs until
//! it waits; the handler commits the run's image, its KV writes and its
//! requests (a turn), then waits on Restate itself for the first request to
//! complete, and delivers it in the next turn:
//!
//! | `ctx` request             | The handler waits on                                        |
//! | ------------------------- | ----------------------------------------------------------- |
//! | `ctx.rpc.call(target, x)` | A call to `target`, its idempotency key the operation's ID |
//! | `ctx.sleep(ms)`           | A timer, until the request's deadline                       |
//! | `ctx.kv.*`                | Nothing: answered in the turn, from the scope's database    |
//!
//! A retried attempt replays nothing: it restores the last committed image and
//! waits again on its requests, under the same idempotency keys, so a call
//! made before the crash is attached to, not made again. The run ends when
//! `main` settles: requests still outstanding (a race's loser) are dropped,
//! and their continuations never run.
use crate::{Engine, Operation, Outcome, Programs, Runtime, View, namespace::NamespaceTransaction};
use restate_sdk::{
    context::{DurableFuture, RequestTarget},
    prelude::*,
};
use serde_json::Value;
use sqlite_kv::{PageCache, PageCacheConfig, StorageOptions};
use std::{
    collections::HashSet,
    pin::Pin,
    sync::{Arc, Mutex, OnceLock},
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

/// What a turn delivers to the run.
enum Event {
    Start(Value),
    Complete {
        operation: String,
        outcome: Outcome,
        /// The earliest logical time to deliver it at: a timer's deadline.
        at: u64,
    },
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
        let mut waiting = DurableFuturesUnordered::<Waiting<'ctx>>::new();
        let mut operations: Vec<String> = Vec::new();
        let mut awaited: HashSet<String> = HashSet::new();
        let mut event = Event::Start(input);
        // One store for the whole invocation: its handler is the object's
        // only writer meanwhile, so what a turn read or wrote stays true.
        let store = Arc::new(ObjectStore::new(ctx.raw_state()));
        let options = StorageOptions {
            cache: Some(page_cache()?.namespace(&format!("{}/{}", self.program, ctx.key()))),
            // A turn's log segment is small (~5 KiB), and a checkpoint copies
            // every changed page into place: rarely, then.
            checkpoint_after: 32,
            ..StorageOptions::default()
        };
        loop {
            let state = self.turn(&store, &options, run, event).await?.state;
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
                        now_ready.get_or_insert((id, Outcome::success(Value::Null)));
                    }
                    Operation::Call { name, .. } => {
                        let message =
                            format!("No host operation {name:?}: call a service with ctx.rpc.call");
                        now_ready.get_or_insert((id, Outcome::failure("Error", message)));
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
            event = match now_ready {
                Some((operation, outcome)) => Event::Complete {
                    operation: operation.to_owned(),
                    outcome,
                    at: 0,
                },
                None => {
                    let Some((index, (outcome, at))) = waiting.next().await? else {
                        return Err(TerminalError::new("The run waits on nothing").into());
                    };
                    Event::Complete {
                        operation: operations[index].clone(),
                        outcome,
                        at,
                    }
                }
            };
        }
    }

    async fn turn(
        &self,
        store: &Arc<ObjectStore>,
        options: &StorageOptions,
        run: &str,
        event: Event,
    ) -> HandlerResult<View> {
        match self
            .commit(store.clone(), options.clone(), run, event)
            .await
        {
            Ok(view) => {
                faults::after_commit()?;
                Ok(view)
            }
            // A storage failure is the invocation's, and retried; anything
            // else would fail again, the same way.
            Err(error) if store.failed() => Err(error.into()),
            Err(error) => Err(TerminalError::new(format!("{error:#}")).into()),
        }
    }

    /// Deliver `event` in one session of the scope's database, and save it:
    /// the run's next image, its KV writes and its requests, together.
    async fn commit(
        &self,
        store: Arc<ObjectStore>,
        options: StorageOptions,
        run: &str,
        event: Event,
    ) -> anyhow::Result<View> {
        let transaction = NamespaceTransaction::begin(store, options, run).await?;
        let mut engine = Engine::new(transaction, self.programs.clone());
        let view = match event {
            // On a retried attempt, the run as it is.
            Event::Start(input) => engine.start(run, &self.program, input, now()).await?,
            Event::Complete {
                operation,
                outcome,
                at,
            } => {
                engine
                    .complete(run, &operation, outcome, now().max(at))
                    .await?
            }
        };
        engine.into_storage()?.commit().await?;
        Ok(view)
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
/// replay (a retried attempt restores the last image instead),
/// its state loaded lazily (a turn reads only the pages it needs), and no
/// journal kept once an invocation is done (it holds every page it read).
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

/// The page cache of the process's objects: 256 MiB of memory and 1 GiB of
/// disk, in a file under the temporary directory. It keeps what each object's
/// database last read and wrote, so a turn reads from Restate only what
/// changed meanwhile, and does not write again what this process already
/// wrote and confirmed. Opened outside any async runtime, it runs its disk I/O
/// on a runtime of its own, which outlives the caller's.
fn page_cache() -> std::io::Result<&'static PageCache> {
    static CACHE: OnceLock<PageCache> = OnceLock::new();
    static OPENING: Mutex<()> = Mutex::new(());
    if let Some(cache) = CACHE.get() {
        return Ok(cache);
    }
    let _opening = OPENING.lock().unwrap();
    if let Some(cache) = CACHE.get() {
        return Ok(cache);
    }
    let file = std::env::temp_dir().join(format!("quickjs-durable-{}.cache", std::process::id()));
    let config = PageCacheConfig {
        // No idle SQL engines kept: they are far larger than the cache's
        // estimate (~3.6 MiB each, measured, against ~50 KiB), so a thousand
        // objects took gigabytes.
        engine_bytes: 0,
        ..PageCacheConfig::new(&file, 256 << 20, 1 << 30)
    };
    let cache = std::thread::spawn(move || futures::executor::block_on(PageCache::open(config)))
        .join()
        .map_err(|_| std::io::Error::other("Opening the page cache panicked"))??;
    // The cache keeps the file open: unlinked now, it goes away with the
    // process however it ends.
    let _ = std::fs::remove_file(&file);
    Ok(CACHE.get_or_init(|| cache))
}

/// Milliseconds since the epoch: a turn's logical time never runs backwards.
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}
