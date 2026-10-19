//! A VM: one Wasm instance whose whole state is its linear memory, for one
//! run, turn after turn. The engine inside (QuickJS or CPython) is a driver
//! ([`crate::quickjs`], [`crate::python`]); this module starts instances,
//! tracks the pages they write, and captures and restores their memory.
use crate::cow::{self, Golden};
use crate::db::Db;
use crate::model::{Capture, Image, MAX_INPUT, MAX_MEMORY, PAGE, Report, VmMeta, fingerprint};
use crate::python::{self, Python};
use crate::quickjs::{self, QuickJs};
use crate::wasi::{Fds, Files};
use crate::{heap, instrument::instrument, tracking::Tracker};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use wasmtime::error::Context;
use wasmtime::{
    Caller, Config, Engine, Global, Instance, InstanceAllocationStrategy, InstancePre, Linker,
    Memory, Module, PoolConcurrencyLimitError, PoolingAllocationConfig, Store, StoreLimits,
    StoreLimitsBuilder, Val,
};
use wasmtime::{Result, ensure};

/// How often the engine's epoch ticks: a running guest checks the clock, and
/// yields to the executor, at its next function entry or loop back-edge.
const TICK: Duration = Duration::from_millis(10);
/// Wasmtime's own stack for a VM's frames. CPython's frames take 10 to 30
/// times more of it than of their stack in linear memory, which its
/// recursion guard measures (512 KiB, `python/reactor.c`): with less,
/// deep recursion in C (`json.dumps`, `repr` of nested lists) traps instead
/// of raising `RecursionError`.
const NATIVE_STACK: usize = 32 << 20;
/// Most VMs at once, live or starting, in a process. Past it, starting one
/// fails as [`exhausted`], and the attempt is retried.
const POOL: u32 = 1000;
/// The runtime's version: an image restores only on the runtime that made it.
/// Bump it with anything an image depends on: the Wasm, the bridges
/// (`guest.js`, `guest.py`), the limits, the imports' behavior, the VM
/// metadata.
pub(crate) const VERSION: u64 = 9;

/// The language a program is written in, and so the engine it runs on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Language {
    /// QuickJS.
    JavaScript,
    /// CPython, built by `make python-engine`.
    Python,
}

/// A memory image cannot restore hidden mutable globals, table mutations or
/// passive segment lifecycle. Check the original reactor in both build modes.
fn validate_snapshot_abi(wasm: &[u8]) -> Result<()> {
    use wasmparser::{ExternalKind, Operator, Parser, Payload, TypeRef, ValType};
    let mut mutable = Vec::new();
    let mut stack = None;
    for payload in Parser::new(0).parse_all(wasm) {
        match payload? {
            Payload::ImportSection(imports) => {
                for import in imports.into_imports() {
                    ensure!(
                        matches!(import?.ty, TypeRef::Func(_)),
                        "Snapshot ABI permits only function imports"
                    );
                }
            }
            Payload::GlobalSection(globals) => {
                for (index, global) in globals.into_iter().enumerate() {
                    let ty = global?.ty;
                    if ty.mutable {
                        ensure!(
                            ty.content_type == ValType::I32,
                            "Unsupported mutable global type"
                        );
                        mutable.push(index as u32);
                    }
                }
            }
            Payload::ExportSection(exports) => {
                for export in exports {
                    let export = export?;
                    if export.kind == ExternalKind::Global && export.name == "__stack_pointer" {
                        stack = Some(export.index);
                    }
                }
            }
            Payload::CodeSectionEntry(body) => {
                for op in body.get_operators_reader()? {
                    ensure!(
                        !matches!(
                            op?,
                            Operator::TableSet { .. }
                                | Operator::TableGrow { .. }
                                | Operator::TableFill { .. }
                                | Operator::TableCopy { .. }
                                | Operator::TableInit { .. }
                                | Operator::ElemDrop { .. }
                                | Operator::MemoryInit { .. }
                                | Operator::DataDrop { .. }
                        ),
                        "Snapshot ABI forbids table mutation and passive segment lifecycle"
                    );
                }
            }
            _ => {}
        }
    }
    ensure!(
        stack.is_some() && mutable == stack.into_iter().collect::<Vec<_>>(),
        "Snapshot ABI requires __stack_pointer as its only mutable global"
    );
    Ok(())
}

/// One engine shared by every run of the process, with each language's
/// module. An image holds guest promises or tasks, never Rust futures or
/// Wasmtime execution stacks.
pub struct Runtime {
    engine: Engine,
    /// QuickJS, its imports resolved once for every VM.
    quickjs: Modules,
    /// CPython, loaded with the first Python program.
    python: OnceLock<std::result::Result<PythonModule, String>>,
    tracking: Tracking,
    limits: Limits,
}

/// An engine's two modules: as built, to evaluate programs into golden
/// images; and without its data segments, for VMs that start from a golden
/// image mapped over their memory ([`crate::cow`]).
struct Modules {
    evaluate: InstancePre<Host>,
    run: InstancePre<Host>,
}

impl Modules {
    fn of(&self, image: Option<&Image>) -> &InstancePre<Host> {
        if image.is_some() {
            &self.run
        } else {
            &self.evaluate
        }
    }
}

struct PythonModule {
    modules: Modules,
    files: Arc<Files>,
    /// Where the allocator's heap starts: after the stack and static data.
    heap: usize,
}

/// How long a turn's guest code may run, by the wall clock. Nothing is
/// replayed, so a turn needs no deterministic budget.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Past this, the run fails with a trap. It is generous: it only ends a
    /// synchronous job that never returns, which would otherwise hold its
    /// scope and do so again on every retry. The endpoint declares it to
    /// Restate as each program's inactivity timeout, so Restate waits as long.
    pub turn: Duration,
    /// Past this, a turn answering local requests stops at the next one and
    /// commits; the work goes on in the next turn.
    pub checkpoint: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            turn: Duration::from_secs(10 * 60),
            checkpoint: Duration::from_secs(1),
        }
    }
}

/// A turn's guest code ran past [`Limits::turn`].
#[derive(Debug)]
struct Overtime(Duration);
impl std::fmt::Display for Overtime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Out of time: one synchronous job ran longer than a turn allows ({:?})",
            self.0
        )
    }
}
impl std::error::Error for Overtime {}

/// How a VM learns which pages a turn wrote, chosen once per process.
#[derive(Clone)]
enum Tracking {
    /// The kernel tracks them (`tracking`): the module runs as compiled.
    Native(Arc<Tracker>),
    /// Every store in the module is instrumented to mark its page.
    Barriers,
    /// None: every page is compared. The benchmarks' control.
    Nothing,
}

/// A VM's host state: what its imports see.
pub(crate) struct Host {
    now: u64,
    /// When the turn began, and its limits.
    began: Instant,
    time: Limits,
    seed: [u8; 8],
    /// Whether randomness is the operating system's. Golden evaluation must
    /// be deterministic, and so are QuickJS's VMs, which seed their own.
    entropy: bool,
    limits: StoreLimits,
    /// The scope's database, which the guest's host calls reach.
    db: Option<Arc<Mutex<Db>>>,
    /// A Python VM's filesystem, its open files, and the host's answer
    /// waiting to be read.
    pub(crate) files: Option<Arc<Files>>,
    pub(crate) fds: Fds,
    pub(crate) answer: Vec<u8>,
}

impl Runtime {
    /// The runtime, tracking writes natively where the kernel can, else with
    /// instrumented stores (or always, with `QJS_TRACKING=barriers`).
    pub fn new() -> Result<Self> {
        Self::tracked(std::env::var("QJS_TRACKING").as_deref() != Ok("barriers"))
    }

    pub(crate) fn tracked(native: bool) -> Result<Self> {
        validate_snapshot_abi(quickjs::WASM)?;
        let mut config = Config::new();
        // A cheap clock check at function entries and loop back-edges,
        // instead of counting every instruction.
        config.epoch_interruption(true);
        // Room for deep C recursion in CPython, whose guard counts only its
        // stack in linear memory (python/reactor.c). Address space: pages are
        // touched only as deep as a guest recurses.
        config.max_wasm_stack(NATIVE_STACK);
        config.async_stack_size(NATIVE_STACK + (1 << 20));
        // A pool of instance slots, reused from VM to VM. A VM maps its
        // golden image over its memory, so nothing is kept resident: what
        // would be is unmapped when the VM ends.
        let mut pool = PoolingAllocationConfig::default();
        pool.total_core_instances(POOL)
            .total_memories(2 * POOL)
            .total_tables(POOL)
            .max_memories_per_module(2)
            .max_memory_size(MAX_MEMORY)
            .linear_memory_keep_resident(0);
        config.allocation_strategy(InstanceAllocationStrategy::Pooling(pool));
        // The modules VMs run from have no data to map: their memory starts as
        // zeros, and the runtime maps golden over it.
        config.memory_init_cow(false);
        let engine = Engine::new(&config)?;
        if cfg!(feature = "full-scan") {
            return Self::with(engine, Tracking::Nothing);
        }
        if native && let Ok(tracker) = Tracker::new() {
            let native = Self::with(engine.clone(), Tracking::Native(Arc::new(tracker)));
            // The pool's memory must register: a VM tries.
            if let Ok(runtime) = native
                && futures::executor::block_on(Vm::new(&runtime, Language::JavaScript, "probe", 0))
                    .and_then(|vm| vm.written())
                    .is_ok()
            {
                return Ok(runtime);
            }
        }
        Self::with(engine, Tracking::Barriers)
    }

    fn with(engine: Engine, tracking: Tracking) -> Result<Self> {
        let mut linker = base_linker(&engine)?;
        quickjs::link(&mut linker)?;
        let quickjs = Modules {
            evaluate: linker.instantiate_pre(&module(&engine, &tracking, quickjs::WASM)?)?,
            run: linker.instantiate_pre(&module(
                &engine,
                &tracking,
                &without_data(quickjs::WASM)?,
            )?)?,
        };
        // The epoch's ticker, for as long as the engine lives.
        let weak = engine.weak();
        std::thread::Builder::new()
            .name("qjs-epoch".into())
            .spawn(move || {
                while let Some(engine) = weak.upgrade() {
                    engine.increment_epoch();
                    drop(engine);
                    std::thread::sleep(TICK);
                }
            })?;
        Ok(Self {
            engine,
            quickjs,
            python: OnceLock::new(),
            tracking,
            limits: Limits::default(),
        })
    }

    /// CPython, loaded and compiled on first use.
    fn python(&self) -> Result<&PythonModule> {
        let loaded = self.python.get_or_init(|| {
            let load = || -> Result<PythonModule> {
                let build = python::Build::load()?;
                validate_snapshot_abi(&build.wasm)?;
                let heap = heap::first_chunk(heap_base(&build.wasm)?);
                let mut linker = base_linker(&self.engine)?;
                python::link(&mut linker)?;
                let compile = |wasm: &[u8]| -> Result<InstancePre<Host>> {
                    linker.instantiate_pre(&cached(&self.engine, &self.tracking, wasm, &build.dir)?)
                };
                Ok(PythonModule {
                    modules: Modules {
                        evaluate: compile(&build.wasm)?,
                        run: compile(&without_data(&build.wasm)?)?,
                    },
                    files: build.files,
                    heap,
                })
            };
            load().map_err(|error| format!("{error:#}"))
        });
        loaded
            .as_ref()
            .map_err(|error| wasmtime::Error::msg(error.clone()))
    }

    /// This runtime, with turns limited to `limits`.
    pub fn limited(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    pub fn limits(&self) -> Limits {
        self.limits
    }

    /// Whether the kernel tracks writes, instead of instrumented stores.
    pub fn native_tracking(&self) -> bool {
        matches!(self.tracking, Tracking::Native(_))
    }

    /// Evaluate a program's module into its golden image, and the names of
    /// the functions it exports. No host function awaits anything while it
    /// runs, so it blocks rather than being async.
    pub(crate) fn golden(
        &self,
        language: Language,
        name: &str,
        source: &str,
    ) -> Result<(Image, Vec<String>)> {
        futures::executor::block_on(self.evaluate(language, name, source))
    }

    async fn evaluate(
        &self,
        language: Language,
        name: &str,
        source: &str,
    ) -> Result<(Image, Vec<String>)> {
        let mut vm = Vm::new(self, language, &format!("program/{name}"), 0).await?;
        vm.store.data_mut().entropy = false;
        match vm.guest.clone() {
            Guest::QuickJs(quickjs) => quickjs.evaluate(&mut vm, source).await?,
            Guest::Python(python) => python.evaluate(&mut vm, name, source).await?,
        }
        let handlers: Vec<String> = serde_json::from_str(&vm.call("handlers", None).await?)?;
        ensure!(
            !handlers.is_empty(),
            "A program exports at least one function"
        );
        Ok((vm.snapshot().await?, handlers))
    }
}

/// `wasm` compiled for `tracking`: as it is, or with every store marking its
/// page. Its only exported global must be the one restored outside memory.
fn module(engine: &Engine, tracking: &Tracking, wasm: &[u8]) -> Result<Module> {
    let module = match tracking {
        Tracking::Barriers => Module::new(
            engine,
            instrument(wasm).map_err(wasmtime::Error::from_anyhow)?,
        )?,
        _ => Module::new(engine, wasm)?,
    };
    for export in module.exports() {
        if matches!(export.ty(), wasmtime::ExternType::Global(_)) {
            ensure!(
                matches!(export.name(), "__stack_pointer" | "__heap_base"),
                "Unsupported exported Wasm global"
            );
        }
    }
    Ok(module)
}

/// [`module`], compiled once into `dir` and loaded from there after: CPython
/// takes seconds to compile. Each engine configuration and tracking mode has
/// its own file; a directory that cannot be written only loses the cache.
fn cached(
    engine: &Engine,
    tracking: &Tracking,
    wasm: &[u8],
    dir: &std::path::Path,
) -> Result<Module> {
    use std::hash::{Hash, Hasher};
    let mut key = std::collections::hash_map::DefaultHasher::new();
    engine.precompile_compatibility_hash().hash(&mut key);
    matches!(tracking, Tracking::Barriers).hash(&mut key);
    fingerprint(wasm).hash(&mut key);
    let path = dir.join(format!("python-{:016x}.cwasm", key.finish()));
    if path.exists() {
        // SAFETY: the file is this engine's own serialization of a module,
        // under a name that only this engine configuration and Wasm produce.
        if let Ok(module) = unsafe { Module::deserialize_file(engine, &path) } {
            return Ok(module);
        }
    }
    let module = module(engine, tracking, wasm)?;
    let temporary = path.with_extension("tmp");
    if std::fs::write(&temporary, module.serialize()?).is_ok() {
        let _ = std::fs::rename(&temporary, &path);
    }
    Ok(module)
}

/// `wasm` without its data segments: its memory starts as zeros.
fn without_data(wasm: &[u8]) -> Result<Vec<u8>> {
    use wasm_encoder::RawSection;
    use wasmparser::Payload;
    let mut module = wasm_encoder::Module::new();
    for payload in wasmparser::Parser::new(0).parse_all(wasm) {
        let payload = payload?;
        if matches!(
            payload,
            Payload::DataSection(_) | Payload::DataCountSection { .. }
        ) {
            continue;
        }
        if let Some((id, range)) = payload.as_section() {
            let data = &wasm[range.start as usize..range.end as usize];
            module.section(&RawSection { id, data });
        }
    }
    Ok(module.finish())
}

/// The value of `wasm`'s exported constant `__heap_base`.
fn heap_base(wasm: &[u8]) -> Result<usize> {
    use wasmparser::{ExternalKind, Operator, Parser, Payload};
    let mut globals = Vec::new();
    let mut index = None;
    for payload in Parser::new(0).parse_all(wasm) {
        match payload? {
            Payload::GlobalSection(section) => {
                for global in section {
                    let mut ops = global?.init_expr.get_operators_reader();
                    globals.push(match ops.read()? {
                        Operator::I32Const { value } => Some(value as u32 as usize),
                        _ => None,
                    });
                }
            }
            Payload::ExportSection(exports) => {
                for export in exports {
                    let export = export?;
                    if export.kind == ExternalKind::Global && export.name == "__heap_base" {
                        index = Some(export.index as usize);
                    }
                }
            }
            _ => {}
        }
    }
    index
        .and_then(|index| *globals.get(index)?)
        .context("No __heap_base")
}

/// Whether `error` is the process running out of VM slots: the attempt is
/// retried later, as for a storage failure.
pub(crate) fn exhausted(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.is::<PoolConcurrencyLimitError>())
}

/// The engine failed a job with an error no guest code can catch, such as
/// QuickJS running out of memory while throwing.
#[derive(Debug)]
pub(crate) struct Uncatchable(pub(crate) String);
impl std::fmt::Display for Uncatchable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Uncatchable guest error: {}", self.0)
    }
}
impl std::error::Error for Uncatchable {}

/// The guest's own failure, if `error` is one: a host's error is not. The
/// guest ran out of time, trapped, or hit an error no guest code can catch:
/// its image is mid-mutation, and the same image and delivery would likely
/// fail again.
pub(crate) fn trap(error: &wasmtime::Error) -> Option<String> {
    if let Some(trap) = error.downcast_ref::<wasmtime::Trap>() {
        return Some(trap.to_string());
    }
    if let Some(overtime) = error.downcast_ref::<Overtime>() {
        return Some(overtime.to_string());
    }
    error.downcast_ref::<Uncatchable>().map(ToString::to_string)
}

/// A VM's record of the pages written since it last committed.
enum Writes {
    /// The kernel's, for the memory at `base`.
    Native {
        tracker: Arc<Tracker>,
        base: usize,
    },
    /// The instrumented module's: a byte per page in this memory.
    Barriers(Memory),
    Nothing,
}

/// The engine inside a VM.
#[derive(Clone)]
enum Guest {
    QuickJs(Arc<QuickJs>),
    Python(Arc<Python>),
}

/// A live VM: one Wasm instance, for one run, turn after turn.
pub(crate) struct Vm {
    pub(crate) store: Store<Host>,
    pub(crate) instance: Instance,
    pub(crate) memory: Memory,
    stack: Global,
    writes: Writes,
    guest: Guest,
    /// Where the allocator's heap starts, for [`heap::dead`].
    heap: usize,
    /// The golden memory mapped over this VM's memory, if it is: a restore
    /// relative to it writes only the pages that differ.
    golden: Option<Arc<Golden>>,
}

impl Drop for Vm {
    fn drop(&mut self) {
        if let Some(golden) = &self.golden {
            let base = self.memory.data_ptr(&self.store);
            // SAFETY: golden is mapped there (`Vm::starting`), and the store,
            // which still owns the memory, runs no more guest code.
            if let Err(error) = unsafe { cow::unmap(base, golden.bytes().len()) } {
                // Wasmtime would reuse memory that still holds this image.
                eprintln!("Could not unmap a golden image: {error}");
                std::process::abort();
            }
        }
    }
}

pub(crate) fn caller_memory(c: &mut Caller<'_, Host>) -> Result<Memory> {
    c.get_export("memory")
        .and_then(|e| e.into_memory())
        .context("Missing guest memory")
}

pub(crate) fn caller_write(c: &mut Caller<'_, Host>, offset: usize, bytes: &[u8]) -> Result<()> {
    caller_memory(c)?.write(&mut *c, offset, bytes)?;
    // Instrumented stores do not cover the host's: mark them too.
    if let Some(dirty) = c
        .get_export(crate::instrument::EXPORT)
        .and_then(|e| e.into_memory())
    {
        mark_host_range(dirty.data_mut(c), offset, bytes.len())?;
    }
    Ok(())
}
fn mark_host_range(bitmap: &mut [u8], offset: usize, len: usize) -> Result<()> {
    if len != 0 {
        let end = offset.checked_add(len - 1).context("Host range overflow")? / PAGE;
        bitmap
            .get_mut(offset / PAGE..=end)
            .context("Host range exceeds tracker")?
            .fill(1);
    }
    Ok(())
}

/// The scope's database's answer to a guest's host call, as JSON (see
/// [`crate::db`]). It may await storage; the guest waits on its fiber.
pub(crate) async fn host_answer(
    c: &mut Caller<'_, Host>,
    operation: &str,
    request: serde_json::Value,
) -> Result<String> {
    let db = c.data().db.clone().context("No database for host calls")?;
    let answer =
        (db.lock().await.call(operation, request).await).map_err(wasmtime::Error::from_anyhow)?;
    let text = serde_json::to_string(&answer)?;
    if text.len() > MAX_INPUT {
        let error = serde_json::json!({"ok": false, "error": {"name": "RangeError", "message": "The result exceeds the limit"}});
        return Ok(error.to_string());
    }
    Ok(text)
}

/// The host functions every engine imports: a clock and randomness, fixed by
/// the VM's identity and logical time.
fn base_linker(engine: &Engine) -> Result<Linker<Host>> {
    let mut linker = Linker::new(engine);
    linker.func_wrap(
        "wasi_snapshot_preview1",
        "clock_time_get",
        |mut c: Caller<'_, Host>, clock: i32, _precision: i64, ptr: i32| -> Result<i32> {
            if !(0..=3).contains(&clock) {
                return Ok(28);
            }
            let ns = c
                .data()
                .now
                .checked_mul(1_000_000)
                .context("Clock overflow")?;
            caller_write(&mut c, ptr as u32 as usize, &ns.to_le_bytes())?;
            Ok(0)
        },
    )?;
    linker.func_wrap(
        "wasi_snapshot_preview1",
        "random_get",
        |mut c: Caller<'_, Host>, ptr: i32, len: i32| -> Result<i32> {
            let len = len as u32 as usize;
            ensure!(len <= MAX_MEMORY, "Random request too large");
            let bytes: Vec<u8> = if c.data().entropy {
                let mut bytes = vec![0; len];
                getrandom::fill(&mut bytes).map_err(|e| wasmtime::Error::msg(e.to_string()))?;
                bytes
            } else {
                let seed = c.data().seed;
                (0..len).map(|i| seed[i % 8]).collect()
            };
            caller_write(&mut c, ptr as u32 as usize, &bytes)?;
            Ok(0)
        },
    )?;
    Ok(linker)
}

impl Vm {
    pub(crate) async fn new(
        runtime: &Runtime,
        language: Language,
        identity: &str,
        now: u64,
    ) -> Result<Self> {
        Self::starting(runtime, language, None, identity, now).await
    }

    /// A VM to restore `image` into: its memory starts as the image's golden
    /// memory, mapped copy-on-write.
    pub(crate) async fn for_image(
        runtime: &Runtime,
        language: Language,
        image: &Image,
        identity: &str,
        now: u64,
    ) -> Result<Self> {
        Self::starting(runtime, language, Some(image), identity, now).await
    }

    async fn starting(
        runtime: &Runtime,
        language: Language,
        image: Option<&Image>,
        identity: &str,
        now: u64,
    ) -> Result<Self> {
        let python = match language {
            Language::JavaScript => None,
            Language::Python => Some(runtime.python()?),
        };
        let host = Host {
            now,
            began: Instant::now(),
            time: runtime.limits,
            seed: fingerprint(identity.as_bytes()).to_le_bytes(),
            entropy: python.is_some(),
            limits: StoreLimitsBuilder::new()
                .memory_size(MAX_MEMORY)
                .instances(1)
                .build(),
            db: None,
            files: python.map(|python| python.files.clone()),
            fds: Fds::default(),
            answer: Vec::new(),
        };
        let mut store = Store::new(&runtime.engine, host);
        store.limiter(|h| &mut h.limits);
        // At every tick: past the turn's limit, fail; else yield to the
        // executor, so a long job does not hold its thread.
        store.set_epoch_deadline(1);
        store.epoch_deadline_callback(|store| {
            let host = store.data();
            let elapsed = host.began.elapsed();
            if elapsed > host.time.turn {
                return Err(wasmtime::Error::new(Overtime(host.time.turn)));
            }
            Ok(wasmtime::UpdateDeadline::Yield(1))
        });
        let instance = match python {
            None => &runtime.quickjs,
            Some(python) => &python.modules,
        }
        .of(image)
        .instantiate_async(&mut store)
        .await?;
        let memory = instance
            .get_memory(&mut store, "memory")
            .context("Missing memory export")?;
        let golden = match image {
            Some(image) => {
                let golden = image.golden_memory().clone();
                let current = memory.data_size(&store);
                let len = golden.bytes().len().max(current);
                ensure!(len <= MAX_MEMORY, "Invalid image size");
                memory.grow(&mut store, ((len - current) / 65536) as u64)?;
                // SAFETY: the start of this VM's memory, at least golden's
                // length, which nothing reads until the VM runs; `Drop` puts
                // zeros back before Wasmtime has the memory again.
                unsafe { golden.map(memory.data_ptr(&store)) }?;
                Some(golden)
            }
            None => None,
        };
        let stack = instance
            .get_global(&mut store, "__stack_pointer")
            .context("Missing stack export")?;
        let writes = match &runtime.tracking {
            Tracking::Native(tracker) => {
                // The slot's whole reservation, however far memory grows.
                let base = memory.data_ptr(&store) as usize;
                tracker.register(base, MAX_MEMORY)?;
                Writes::Native {
                    tracker: tracker.clone(),
                    base,
                }
            }
            Tracking::Barriers => Writes::Barriers(
                instance
                    .get_memory(&mut store, crate::instrument::EXPORT)
                    .context("Missing tracker")?,
            ),
            Tracking::Nothing => Writes::Nothing,
        };
        let (guest, heap) = match python {
            None => (
                Guest::QuickJs(Arc::new(QuickJs::new(&mut store, &instance)?)),
                heap::QUICKJS,
            ),
            Some(python) => (
                Guest::Python(Arc::new(Python::new(&mut store, &instance)?)),
                python.heap,
            ),
        };
        Ok(Self {
            store,
            instance,
            memory,
            stack,
            writes,
            guest,
            heap,
            golden,
        })
    }
    /// Let the guest's host calls reach `db`.
    pub(crate) fn attach(&mut self, db: Arc<Mutex<Db>>) {
        self.store.data_mut().db = Some(db);
    }
    /// Begin a turn at logical time `now`, its clock started.
    pub(crate) fn begin_turn(&mut self, now: u64) -> Result<()> {
        let host = self.store.data_mut();
        host.now = now;
        host.began = Instant::now();
        Ok(())
    }
    /// Whether the turn has run past [`Limits::checkpoint`]: time to stop at
    /// the next safe boundary, so a long run of local requests goes on in the
    /// next turn instead of running out of time.
    pub(crate) fn tired(&self) -> Result<bool> {
        let host = self.store.data();
        Ok(host.began.elapsed() > host.time.checkpoint)
    }
    /// The captured pages are committed: track writes anew.
    pub(crate) fn committed(&mut self) -> Result<()> {
        let len = self.memory.data_size(&self.store);
        match &self.writes {
            // Only the written pages: protecting them all walks and
            // flushes every page, ~10x the cost.
            Writes::Native { tracker, base } => drop(tracker.written(*base, len, true)?),
            Writes::Barriers(dirty) => dirty.data_mut(&mut self.store).fill(0),
            Writes::Nothing => {}
        }
        Ok(())
    }
    /// The pages written since the last commit, in order.
    fn written(&self) -> Result<Vec<usize>> {
        let len = self.memory.data_size(&self.store);
        Ok(match &self.writes {
            Writes::Native { tracker, base } => tracker.written(*base, len, false)?,
            Writes::Barriers(dirty) => dirty
                .data(&self.store)
                .iter()
                .take(len / PAGE)
                .enumerate()
                .filter_map(|(index, bit)| (*bit != 0).then_some(index))
                .collect(),
            Writes::Nothing => (0..len / PAGE).collect(),
        })
    }
    pub(crate) async fn restore(&mut self, image: &Image) -> Result<()> {
        ensure!(
            image.len() <= MAX_MEMORY && image.len().is_multiple_of(65536),
            "Invalid image size"
        );
        let current = self.memory.data_size(&self.store);
        ensure!(
            image.len() >= current,
            "Image smaller than module's initial memory"
        );
        if image.len() > current {
            self.memory
                .grow(&mut self.store, ((image.len() - current) / 65536) as u64)?;
        }
        if self
            .golden
            .as_ref()
            .is_some_and(|golden| Arc::ptr_eq(golden, image.golden_memory()))
        {
            image.write_changed(self.memory.data_mut(&mut self.store));
        } else {
            image.write(self.memory.data_mut(&mut self.store));
        }
        // Committed restore bytes are not new writes. Begin tracking before
        // re-binding runtime pointers and limits, which may modify memory.
        self.committed()?;
        self.stack
            .set(&mut self.store, Val::I32(image.meta.stack))?;
        match self.guest.clone() {
            Guest::QuickJs(quickjs) => quickjs.restored(self, &image.meta).await,
            Guest::Python(_) => Ok(()),
        }
    }
    /// Call a bridge hook, `__durable.<name>(argument)`, with an optional
    /// string argument; the string it returns.
    pub(crate) async fn call(&mut self, name: &str, argument: Option<&str>) -> Result<String> {
        match self.guest.clone() {
            Guest::QuickJs(quickjs) => quickjs.call(self, name, argument).await,
            Guest::Python(python) => python.call(self, name, argument).await,
        }
    }
    pub(crate) async fn invoke(&mut self, name: &str, value: &serde_json::Value) -> Result<()> {
        self.call(name, Some(&serde_json::to_string(value)?))
            .await?;
        Ok(())
    }
    /// What the guest issued since it last reported, and where it stands.
    pub(crate) async fn inspect(&mut self) -> Result<Report> {
        Ok(serde_json::from_str(&self.call("inspect", None).await?)?)
    }
    /// Run the guest's ready work (jobs, tasks) until none is left.
    pub(crate) async fn drain(&mut self) -> Result<()> {
        match self.guest.clone() {
            Guest::QuickJs(quickjs) => quickjs.drain(self).await,
            Guest::Python(python) => python.call(self, "drain", None).await.map(drop),
        }
    }
    pub(crate) fn write(&mut self, offset: usize, bytes: &[u8]) -> Result<()> {
        self.memory.write(&mut self.store, offset, bytes)?;
        if let Writes::Barriers(dirty) = &self.writes {
            mark_host_range(dirty.data_mut(&mut self.store), offset, bytes.len())?;
        }
        Ok(())
    }
    async fn snapshot(&mut self) -> Result<Image> {
        // Only golden registration needs a complete copy of guest memory.
        let meta = self.metadata().await?;
        let heap = self.heap;
        let memory = self.memory.data_mut(&mut self.store);
        memory[..meta.stack as u32 as usize].fill(0);
        for range in heap::dead(memory, heap).context("Unexpected heap layout")? {
            memory[range].fill(0);
        }
        let memory = memory.to_vec();
        ensure!(memory.len() <= MAX_MEMORY, "Image exceeds limit");
        Ok(Image::golden(meta, memory))
    }
    /// What changed since `base`, with the dead bytes of the pages it compares
    /// zeroed first: the stack below its pointer (between calls no frame is
    /// live there), and free heap chunks. The turn's leftover frames and the
    /// memory it allocated and freed would otherwise be saved as changes.
    pub(crate) async fn capture(&mut self, base: &Image) -> Result<Capture> {
        // Open files are host state that no image keeps: the same turn would
        // leave them open again, so the run fails rather than retrying.
        if let Err(error) = self.store.data().fds.idle() {
            return Err(wasmtime::Error::new(Uncatchable(error.to_string())));
        }
        let meta = self.metadata().await?;
        let stack = meta.stack as u32 as usize;
        let memory_len = self.memory.data_size(&self.store);
        ensure!(
            memory_len <= MAX_MEMORY && memory_len >= base.len() && stack <= memory_len,
            "Invalid captured memory size"
        );
        let candidates = self.written()?;
        let heap = self.heap;
        let memory = self.memory.data_mut(&mut self.store);
        let mut compared = vec![false; memory_len / PAGE];
        for &index in &candidates {
            compared[index] = true;
        }
        let dead = std::iter::once(0..stack).chain(heap::dead(memory, heap).unwrap_or_default());
        for range in dead {
            let pages = range.start / PAGE..range.end.div_ceil(PAGE);
            for index in pages.filter(|&index| compared[index]) {
                let page = index * PAGE..(index + 1) * PAGE;
                memory[page.start.max(range.start)..page.end.min(range.end)].fill(0);
            }
        }
        let memory = self.memory.data(&self.store);
        let mut pages = Vec::new();
        let mut examined_pages = 0;
        for index in candidates {
            examined_pages += 1;
            let previous = base.page(index as u32);
            let next = &memory[index * PAGE..(index + 1) * PAGE];
            if next != previous {
                pages.push((index as u32, next.to_vec()));
            }
        }
        #[cfg(feature = "verify-dirty")]
        for (index, next) in memory.as_chunks::<PAGE>().0.iter().enumerate() {
            ensure!(
                next == base.page(index as u32) || compared[index],
                "Dirty tracker missed page {index}"
            );
        }
        Ok(Capture {
            meta,
            memory_len,
            pages,
            examined_pages,
        })
    }
    async fn metadata(&mut self) -> Result<VmMeta> {
        let (runtime, context) = match self.guest.clone() {
            Guest::QuickJs(quickjs) => quickjs.pointers(self).await?,
            Guest::Python(_) => (0, 0),
        };
        let stack = self
            .stack
            .get(&mut self.store)
            .i32()
            .context("Invalid stack global")?;
        Ok(VmMeta {
            stack,
            runtime,
            context,
        })
    }
}

#[cfg(all(test, not(feature = "full-scan")))]
#[path = "../tests/unit/vm.rs"]
mod tests;
