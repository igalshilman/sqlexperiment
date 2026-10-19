use crate::db::Db;
use crate::model::{Capture, Image, MAX_INPUT, MAX_MEMORY, PAGE, Report, VmMeta, fingerprint};
use crate::{heap, instrument::instrument, tracking::Tracker};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use wasmtime::error::Context;
use wasmtime::{
    Caller, Config, Engine, Global, Instance, InstanceAllocationStrategy, InstancePre, Linker,
    Memory, Module, PoolConcurrencyLimitError, PoolingAllocationConfig, Store, StoreLimits,
    StoreLimitsBuilder, TypedFunc, Val,
};
use wasmtime::{Result, bail, ensure};

const WASM: &[u8] = include_bytes!("../assets/quickjs.wasm");
const BOOTSTRAP: &str = include_str!("../assets/guest.js");
/// How often the engine's epoch ticks: a running guest checks the clock, and
/// yields to the executor, at its next function entry or loop back-edge.
const TICK: Duration = Duration::from_millis(10);
// Below upstream's 512 KiB ceiling: Wasmtime native frames need their own
// headroom, especially in the instrumented interpreter. Reapply after restore.
const MAX_STACK: i32 = 32 * 1024;
const MAX_HEAP: i32 = 32 * 1024 * 1024;
/// Most VMs at once, live or starting, in a process. Past it, starting one
/// fails as [`exhausted`], and the attempt is retried.
const POOL: u32 = 1000;
/// Bytes of each VM's memory kept mapped from one VM to the next: golden's
/// and a little more.
const RESIDENT: usize = 2 * 1024 * 1024;
// qjs_init2 flags: every intrinsic but weak references/finalization (bit 9)
// and performance (bit 10).
const INTRINSICS: i32 = !(1 << 9 | 1 << 10);
/// The runtime's version: an image restores only on the runtime that made it.
/// Bump it with anything an image depends on: the Wasm, the bridge
/// (`guest.js`), the limits above, the imports' behavior, the VM metadata.
pub(crate) const VERSION: u64 = 7;

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

/// One compiled engine/module shared by every run of the process. An image
/// holds guest promises, never Rust futures or Wasmtime execution stacks.
pub struct Runtime {
    engine: Engine,
    /// The module, its imports resolved once for every VM.
    instance: InstancePre<Host>,
    tracking: Tracking,
    limits: Limits,
}

/// How long a turn's JavaScript may run, by the wall clock. Nothing is
/// replayed, so a turn needs no deterministic budget.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Past this, the run fails with a trap: a synchronous job that never
    /// returns would otherwise hold the handler, and do so on every retry.
    pub turn: Duration,
    /// Past this, a turn answering local requests stops at the next one and
    /// commits; the work goes on in the next turn.
    pub checkpoint: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            turn: Duration::from_secs(10),
            checkpoint: Duration::from_secs(1),
        }
    }
}

/// A turn's JavaScript ran past [`Limits::turn`].
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
struct Host {
    now: u64,
    /// When the turn began, and its limits.
    began: Instant,
    time: Limits,
    seed: [u8; 8],
    limits: StoreLimits,
    /// The scope's database, which the guest's host calls reach.
    db: Option<Arc<Mutex<Db>>>,
}

impl Runtime {
    /// The runtime, tracking writes natively where the kernel can, else with
    /// instrumented stores (or always, with `QJS_TRACKING=barriers`).
    pub fn new() -> Result<Self> {
        Self::tracked(std::env::var("QJS_TRACKING").as_deref() != Ok("barriers"))
    }

    pub(crate) fn tracked(native: bool) -> Result<Self> {
        validate_snapshot_abi(WASM)?;
        let mut config = Config::new();
        // A cheap clock check at function entries and loop back-edges,
        // instead of counting every instruction.
        config.epoch_interruption(true);
        // A pool of instance slots, reused from VM to VM, with the first
        // pages of each memory kept resident: a new VM writes golden over
        // memory already mapped, instead of faulting in every page.
        let mut pool = PoolingAllocationConfig::default();
        pool.total_core_instances(POOL)
            .total_memories(2 * POOL)
            .total_tables(POOL)
            .max_memories_per_module(2)
            .max_memory_size(MAX_MEMORY)
            .linear_memory_keep_resident(RESIDENT);
        config.allocation_strategy(InstanceAllocationStrategy::Pooling(pool));
        // Memory starts as golden, written in: no copy-on-write image of the
        // module's data, which would also replace a slot's tracked mapping.
        config.memory_init_cow(false);
        let engine = Engine::new(&config)?;
        if cfg!(feature = "full-scan") {
            return Self::with(engine, WASM, Tracking::Nothing);
        }
        if native && let Ok(tracker) = Tracker::new() {
            let native = Self::with(engine.clone(), WASM, Tracking::Native(Arc::new(tracker)));
            // The pool's memory must register: a VM tries.
            if let Ok(runtime) = native
                && futures::executor::block_on(Vm::new(&runtime, "probe", 0))
                    .and_then(|vm| vm.written())
                    .is_ok()
            {
                return Ok(runtime);
            }
        }
        let instrumented = instrument(WASM).map_err(wasmtime::Error::from_anyhow)?;
        Self::with(engine, &instrumented, Tracking::Barriers)
    }

    fn with(engine: Engine, wasm: &[u8], tracking: Tracking) -> Result<Self> {
        let module = Module::new(&engine, wasm)?;
        // This pinned, extension-free reactor exports exactly the global that
        // must be restored outside linear memory. A new ABI must fail closed.
        for export in module.exports() {
            if matches!(export.ty(), wasmtime::ExternType::Global(_)) {
                ensure!(
                    export.name() == "__stack_pointer",
                    "Unsupported exported Wasm global"
                );
            }
        }
        let instance = linker(&engine)?.instantiate_pre(&module)?;
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
            instance,
            tracking,
            limits: Limits::default(),
        })
    }

    /// This runtime, with turns limited to `limits`.
    pub fn limited(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    /// Whether the kernel tracks writes, instead of instrumented stores.
    pub fn native_tracking(&self) -> bool {
        matches!(self.tracking, Tracking::Native(_))
    }

    /// Evaluate a program's source into its golden image. No host function
    /// awaits anything while it runs, so it blocks rather than being async.
    pub(crate) fn golden(&self, name: &str, source: &str) -> Result<Image> {
        futures::executor::block_on(self.evaluate(name, source))
    }

    async fn evaluate(&self, name: &str, source: &str) -> Result<Image> {
        let mut vm = Vm::new(self, &format!("program/{name}"), 0).await?;
        vm.instance
            .get_typed_func::<(), ()>(&mut vm.store, "_initialize")?
            .call_async(&mut vm.store, ())
            .await?;
        ensure!(
            vm.instance
                .get_typed_func::<i32, i32>(&mut vm.store, "qjs_init2")?
                .call_async(&mut vm.store, INTRINSICS)
                .await?
                == 0,
            "QuickJS initialization failed"
        );
        vm.limits().await?;
        vm.host_function().await?;
        vm.eval(BOOTSTRAP, false).await?;
        vm.eval(source, false).await?;
        vm.drain().await?;
        ensure!(
            vm.eval("typeof main === 'function'", true)
                .await?
                .as_deref()
                == Some("true"),
            "A program must define main(input, ctx)"
        );
        vm.snapshot().await
    }
}

/// Whether `error` is the process running out of VM slots: the attempt is
/// retried later, as for a storage failure.
pub(crate) fn exhausted(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.is::<PoolConcurrencyLimitError>())
}

/// QuickJS failed a job with an error no script can catch, such as running
/// out of memory while throwing.
#[derive(Debug)]
struct Uncatchable(String);
impl std::fmt::Display for Uncatchable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Uncatchable guest error: {}", self.0)
    }
}
impl std::error::Error for Uncatchable {}

/// The guest's own failure, if `error` is one: a host's error is not. The
/// guest ran out of time, trapped, or QuickJS hit an error no script can
/// catch: its image is mid-mutation, and the same image and delivery would
/// likely fail again.
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

/// A live QuickJS VM: one Wasm instance, for one run, turn after turn.
pub(crate) struct Vm {
    store: Store<Host>,
    instance: Instance,
    memory: Memory,
    stack: Global,
    writes: Writes,
    malloc: TypedFunc<i32, i32>,
    free: TypedFunc<i32, ()>,
    eval_fn: TypedFunc<(i32, i32, i32, i32), i32>,
    call_fn: TypedFunc<(i32, i32, i32, i32), i32>,
    global: TypedFunc<(), i32>,
    property: TypedFunc<(i32, i32), i32>,
    new_string: TypedFunc<(i32, i32), i32>,
    free_value: TypedFunc<i32, ()>,
    is_exception: TypedFunc<i32, i32>,
    get_exception: TypedFunc<(), i32>,
    get_string: TypedFunc<(i32, i32), i32>,
    free_string: TypedFunc<i32, ()>,
    pending: TypedFunc<(), i32>,
    execute: TypedFunc<(), i32>,
}

fn caller_memory(c: &mut Caller<'_, Host>) -> Result<Memory> {
    c.get_export("memory")
        .and_then(|e| e.into_memory())
        .context("Missing guest memory")
}

fn caller_write(c: &mut Caller<'_, Host>, offset: usize, bytes: &[u8]) -> Result<()> {
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

/// One of the module's exports, from inside a host function.
fn export<P: wasmtime::WasmParams, R: wasmtime::WasmResults>(
    c: &mut Caller<'_, Host>,
    name: &str,
) -> Result<TypedFunc<P, R>> {
    c.get_export(name)
        .and_then(|e| e.into_func())
        .with_context(|| format!("Missing export {name}"))?
        .typed(&*c)
}

/// A guest value as a string, from inside a host function.
async fn guest_string(c: &mut Caller<'_, Host>, value: i32) -> Result<String> {
    let malloc = export::<i32, i32>(c, "wasm_malloc")?;
    let free = export::<i32, ()>(c, "wasm_free")?;
    let len_ptr = malloc.call_async(&mut *c, 4).await?;
    ensure!(len_ptr != 0, "Guest allocation failed");
    let ptr = export::<(i32, i32), i32>(c, "qjs_get_string_len")?
        .call_async(&mut *c, (value, len_ptr))
        .await?;
    ensure!(ptr != 0, "String conversion failed");
    let memory = caller_memory(c)?;
    let mut len = [0; 4];
    memory.read(&*c, len_ptr as u32 as usize, &mut len)?;
    let len = u32::from_le_bytes(len) as usize;
    ensure!(len <= MAX_INPUT, "Host call exceeds limit");
    let mut bytes = vec![0; len];
    memory.read(&*c, ptr as u32 as usize, &mut bytes)?;
    export::<i32, ()>(c, "qjs_free_cstring")?
        .call_async(&mut *c, ptr)
        .await?;
    free.call_async(&mut *c, len_ptr).await?;
    Ok(String::from_utf8(bytes)?)
}

/// A new guest string, from inside a host function.
async fn new_guest_string(c: &mut Caller<'_, Host>, text: &str) -> Result<i32> {
    let ptr = export::<i32, i32>(c, "wasm_malloc")?
        .call_async(&mut *c, (text.len() + 1).try_into()?)
        .await?;
    ensure!(ptr != 0, "Guest allocation failed");
    caller_write(c, ptr as u32 as usize, text.as_bytes())?;
    caller_write(c, ptr as u32 as usize + text.len(), &[0])?;
    let value = export::<(i32, i32), i32>(c, "qjs_new_string")?
        .call_async(&mut *c, (ptr, text.len().try_into()?))
        .await?;
    export::<i32, ()>(c, "wasm_free")?
        .call_async(&mut *c, ptr)
        .await?;
    ensure!(value != 0, "Guest string allocation failed");
    Ok(value)
}

/// The guest's one host function, `__host(operation, request)`: two strings
/// in, a JSON string out, answered from the scope's database (see
/// [`crate::db`]). It may await storage; the guest waits on its fiber. The
/// returned value is the caller's to free.
async fn host_call(
    c: &mut Caller<'_, Host>,
    name: (i32, i32),
    argc: i32,
    argv: i32,
) -> Result<i32> {
    let memory = caller_memory(c)?;
    let mut function = [0; 4];
    ensure!(name.1 == 4, "Unknown host function");
    memory.read(&*c, name.0 as u32 as usize, &mut function)?;
    ensure!(&function == b"host", "Unknown host function");
    ensure!(argc == 2, "A host call takes an operation and a request");
    let mut handles = [0; 8];
    memory.read(&*c, argv as u32 as usize, &mut handles)?;
    let operation = i32::from_le_bytes(handles[..4].try_into()?);
    let request = i32::from_le_bytes(handles[4..].try_into()?);
    let operation = guest_string(c, operation).await?;
    let request = serde_json::from_str(&guest_string(c, request).await?)?;
    let db = c.data().db.clone().context("No database for host calls")?;
    let answer =
        (db.lock().await.call(&operation, request).await).map_err(wasmtime::Error::from_anyhow)?;
    let mut text = serde_json::to_string(&answer)?;
    if text.len() > MAX_INPUT {
        let error = serde_json::json!({"ok": false, "error": {"name": "RangeError", "message": "The result exceeds the limit"}});
        text = error.to_string();
    }
    new_guest_string(c, &text).await
}

/// The host functions the module imports: a clock, randomness and logs, all
/// fixed by the VM's identity and logical time; nothing else.
fn linker(engine: &Engine) -> Result<Linker<Host>> {
    let mut linker = Linker::new(engine);
    linker.func_wrap("env", "host_get_timezone_offset", |_hi: i32, _lo: i32| {
        0_i32
    })?;
    linker.func_wrap("env", "host_interrupt", || 0_i32)?;
    linker.func_wrap("env", "host_module_normalize", |_base: i32, _name: i32| {
        0_i32
    })?;
    linker.func_wrap("env", "host_module_load", |_name: i32, _len: i32| 0_i32)?;
    linker.func_wrap_async(
        "env",
        "host_call",
        |mut c: Caller<'_, Host>, (name, len, _this, argc, argv): (i32, i32, i32, i32, i32)| {
            Box::new(async move { host_call(&mut c, (name, len), argc, argv).await })
        },
    )?;
    linker.func_wrap(
        "env",
        "host_promise_rejection",
        |_: i32, _: i32, _: i32| -> Result<()> { bail!("Unexpected rejection callback") },
    )?;
    linker.func_wrap(
        "wasi_snapshot_preview1",
        "clock_time_get",
        |mut c: Caller<'_, Host>, clock: i32, _precision: i64, ptr: i32| -> Result<i32> {
            if clock != 0 && clock != 1 {
                return Ok(52);
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
            let seed = c.data().seed;
            let bytes: Vec<u8> = (0..len).map(|i| seed[i % 8]).collect();
            caller_write(&mut c, ptr as u32 as usize, &bytes)?;
            Ok(0)
        },
    )?;
    linker.func_wrap("wasi_snapshot_preview1", "fd_close", |_: i32| 52_i32)?;
    linker.func_wrap(
        "wasi_snapshot_preview1",
        "fd_seek",
        |_: i32, _: i64, _: i32, _: i32| 52_i32,
    )?;
    linker.func_wrap(
        "wasi_snapshot_preview1",
        "fd_fdstat_get",
        |mut c: Caller<'_, Host>, fd: i32, ptr: i32| -> Result<i32> {
            if fd != 1 && fd != 2 {
                return Ok(8);
            }
            let mut bytes = [0_u8; 24];
            bytes[0] = 2;
            caller_write(&mut c, ptr as u32 as usize, &bytes)?;
            Ok(0)
        },
    )?;
    // Internal runtime logs are discarded. Guest code gets no external I/O.
    linker.func_wrap(
        "wasi_snapshot_preview1",
        "fd_write",
        |mut c: Caller<'_, Host>, fd: i32, ptr: i32, count: i32, written: i32| -> Result<i32> {
            if fd != 1 && fd != 2 {
                return Ok(8);
            }
            let count = count as u32 as usize;
            ensure!(count <= 1024, "Too many I/O vectors");
            let memory = caller_memory(&mut c)?;
            let mut total = 0_u32;
            for i in 0..count {
                let mut pair = [0_u8; 8];
                memory.read(&c, ptr as u32 as usize + i * 8, &mut pair)?;
                let at = u32::from_le_bytes(pair[..4].try_into()?) as usize;
                let len = u32::from_le_bytes(pair[4..].try_into()?);
                ensure!(
                    at.checked_add(len as usize)
                        .is_some_and(|end| end <= memory.data_size(&c)),
                    "Invalid I/O vector"
                );
                total = total.checked_add(len).context("I/O length overflow")?;
            }
            caller_write(&mut c, written as u32 as usize, &total.to_le_bytes())?;
            Ok(0)
        },
    )?;
    Ok(linker)
}

impl Vm {
    pub(crate) async fn new(runtime: &Runtime, identity: &str, now: u64) -> Result<Self> {
        let host = Host {
            now,
            began: Instant::now(),
            time: runtime.limits,
            seed: fingerprint(identity.as_bytes()).to_le_bytes(),
            limits: StoreLimitsBuilder::new()
                .memory_size(MAX_MEMORY)
                .instances(1)
                .build(),
            db: None,
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
        let instance = runtime.instance.instantiate_async(&mut store).await?;
        let memory = instance
            .get_memory(&mut store, "memory")
            .context("Missing memory export")?;
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
        Ok(Self {
            malloc: instance.get_typed_func(&mut store, "wasm_malloc")?,
            free: instance.get_typed_func(&mut store, "wasm_free")?,
            eval_fn: instance.get_typed_func(&mut store, "qjs_eval")?,
            call_fn: instance.get_typed_func(&mut store, "qjs_call")?,
            global: instance.get_typed_func(&mut store, "qjs_get_global")?,
            property: instance.get_typed_func(&mut store, "qjs_get_prop_string")?,
            new_string: instance.get_typed_func(&mut store, "qjs_new_string")?,
            free_value: instance.get_typed_func(&mut store, "qjs_free_value")?,
            is_exception: instance.get_typed_func(&mut store, "qjs_is_exception")?,
            get_exception: instance.get_typed_func(&mut store, "qjs_get_exception")?,
            get_string: instance.get_typed_func(&mut store, "qjs_get_string_len")?,
            free_string: instance.get_typed_func(&mut store, "qjs_free_cstring")?,
            pending: instance.get_typed_func(&mut store, "qjs_is_job_pending")?,
            execute: instance.get_typed_func(&mut store, "qjs_execute_pending_job")?,
            store,
            instance,
            memory,
            stack,
            writes,
        })
    }
    /// Let the guest's host calls reach `db`.
    pub(crate) fn attach(&mut self, db: Arc<Mutex<Db>>) {
        self.store.data_mut().db = Some(db);
    }
    /// Define the global `__host`, the guest's one host function, which the
    /// bridge takes for itself. The function holds only its name, so a
    /// restored heap calls the same import.
    async fn host_function(&mut self) -> Result<()> {
        let name = self.alloc(b"host").await?;
        let function = self
            .instance
            .get_typed_func::<(i32, i32, i32), i32>(&mut self.store, "qjs_new_host_function")?
            .call_async(&mut self.store, (name, 4, 2))
            .await?;
        self.free.call_async(&mut self.store, name).await?;
        ensure!(function != 0, "Host function creation failed");
        let global = self.global.call_async(&mut self.store, ()).await?;
        let key = self.alloc(b"__host").await?;
        self.instance
            .get_typed_func::<(i32, i32, i32), i32>(&mut self.store, "qjs_set_prop_string")?
            .call_async(&mut self.store, (global, key, function))
            .await?;
        self.free.call_async(&mut self.store, key).await?;
        self.free_value
            .call_async(&mut self.store, function)
            .await?;
        self.free_value.call_async(&mut self.store, global).await?;
        Ok(())
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
    async fn limits(&mut self) -> Result<()> {
        self.instance
            .get_typed_func::<i32, ()>(&mut self.store, "qjs_set_memory_limit")?
            .call_async(&mut self.store, MAX_HEAP)
            .await?;
        self.instance
            .get_typed_func::<i32, ()>(&mut self.store, "qjs_set_max_stack_size")?
            .call_async(&mut self.store, MAX_STACK)
            .await?;
        Ok(())
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
        image.write(self.memory.data_mut(&mut self.store));
        // Committed restore bytes are not new writes. Begin tracking before
        // re-binding runtime pointers and limits, which may modify memory.
        self.committed()?;
        self.stack
            .set(&mut self.store, Val::I32(image.meta.stack))?;
        self.instance
            .get_typed_func::<(i32, i32), ()>(&mut self.store, "qjs_set_runtime_and_context")?
            .call_async(&mut self.store, (image.meta.runtime, image.meta.context))
            .await?;
        self.limits().await
    }
    async fn alloc(&mut self, data: &[u8]) -> Result<i32> {
        let ptr = self
            .malloc
            .call_async(&mut self.store, (data.len() + 1).try_into()?)
            .await?;
        ensure!(ptr != 0, "Guest allocation failed");
        self.write(ptr as u32 as usize, data)?;
        self.write(ptr as u32 as usize + data.len(), &[0])?;
        Ok(ptr)
    }
    async fn string(&mut self, value: i32) -> Result<String> {
        let len_ptr = self.alloc(&[0; 4]).await?;
        let ptr = self
            .get_string
            .call_async(&mut self.store, (value, len_ptr))
            .await?;
        ensure!(ptr != 0, "String conversion failed");
        let mut len = [0; 4];
        self.memory
            .read(&self.store, len_ptr as u32 as usize, &mut len)?;
        let len = u32::from_le_bytes(len) as usize;
        ensure!(len <= MAX_INPUT, "Host result exceeds limit");
        let mut bytes = vec![0; len];
        self.memory
            .read(&self.store, ptr as u32 as usize, &mut bytes)?;
        self.free_string.call_async(&mut self.store, ptr).await?;
        self.free.call_async(&mut self.store, len_ptr).await?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }
    async fn exception(&mut self) -> Result<String> {
        let ptr = self.get_exception.call_async(&mut self.store, ()).await?;
        ensure!(ptr != 0, "Missing exception value");
        let result = self.string(ptr).await;
        self.free_value.call_async(&mut self.store, ptr).await?;
        result
    }
    async fn eval(&mut self, code: &str, stringify: bool) -> Result<Option<String>> {
        let code_ptr = self.alloc(code.as_bytes()).await?;
        let filename = self.alloc(b"program.js").await?;
        let value = self
            .eval_fn
            .call_async(
                &mut self.store,
                (code_ptr, code.len().try_into()?, filename, 0),
            )
            .await?;
        self.free.call_async(&mut self.store, code_ptr).await?;
        self.free.call_async(&mut self.store, filename).await?;
        ensure!(value != 0, "Missing evaluated value");
        let is_exception = self.is_exception.call_async(&mut self.store, value).await? != 0;
        let result = if !is_exception && stringify {
            Some(self.string(value).await?)
        } else {
            None
        };
        self.free_value.call_async(&mut self.store, value).await?;
        if is_exception {
            bail!("Guest exception: {}", self.exception().await?)
        }
        Ok(result)
    }
    /// Call a bridge hook, `__durable.<name>(argument)`, with an optional
    /// string argument, returning its result as a string. A direct call: no
    /// source to compile, so it costs a fraction of an eval, and no argument
    /// can inject source. Every handle is freed before returning; after an
    /// error the turn is discarded, so leaks then don't matter.
    pub(crate) async fn call(&mut self, name: &str, argument: Option<&str>) -> Result<String> {
        let global = self.global.call_async(&mut self.store, ()).await?;
        ensure!(global != 0, "Missing global object");
        let bridge = self.property(global, "__durable").await?;
        self.free_value.call_async(&mut self.store, global).await?;
        let function = self.property(bridge, name).await?;
        let (argc, argv, value, text) = match argument {
            Some(argument) => {
                let text = self.alloc(argument.as_bytes()).await?;
                let value = self
                    .new_string
                    .call_async(&mut self.store, (text, argument.len().try_into()?))
                    .await?;
                ensure!(value != 0, "Guest string allocation failed");
                let argv = self.alloc(&value.to_le_bytes()).await?;
                (1, argv, value, text)
            }
            None => (0, 0, 0, 0),
        };
        let result = self
            .call_fn
            .call_async(&mut self.store, (function, bridge, argc, argv))
            .await?;
        for handle in [function, bridge, value] {
            if handle != 0 {
                self.free_value.call_async(&mut self.store, handle).await?;
            }
        }
        for buffer in [argv, text] {
            if buffer != 0 {
                self.free.call_async(&mut self.store, buffer).await?;
            }
        }
        ensure!(result != 0, "Missing call result");
        if self
            .is_exception
            .call_async(&mut self.store, result)
            .await?
            != 0
        {
            self.free_value.call_async(&mut self.store, result).await?;
            bail!("Guest exception: {}", self.exception().await?)
        }
        let string = self.string(result).await;
        self.free_value.call_async(&mut self.store, result).await?;
        string
    }
    async fn property(&mut self, object: i32, name: &str) -> Result<i32> {
        let key = self.alloc(name.as_bytes()).await?;
        let value = self
            .property
            .call_async(&mut self.store, (object, key))
            .await?;
        self.free.call_async(&mut self.store, key).await?;
        ensure!(value != 0, "Missing property {name}");
        Ok(value)
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
    pub(crate) async fn drain(&mut self) -> Result<()> {
        while self.pending.call_async(&mut self.store, ()).await? != 0 {
            if self.execute.call_async(&mut self.store, ()).await? < 0 {
                return Err(wasmtime::Error::new(Uncatchable(self.exception().await?)));
            }
        }
        Ok(())
    }
    fn write(&mut self, offset: usize, bytes: &[u8]) -> Result<()> {
        self.memory.write(&mut self.store, offset, bytes)?;
        if let Writes::Barriers(dirty) = &self.writes {
            mark_host_range(dirty.data_mut(&mut self.store), offset, bytes.len())?;
        }
        Ok(())
    }
    async fn snapshot(&mut self) -> Result<Image> {
        // Only golden registration needs a complete copy of guest memory.
        let meta = self.metadata().await?;
        let memory = self.memory.data_mut(&mut self.store);
        memory[..meta.stack as u32 as usize].fill(0);
        for range in heap::dead(memory).context("Unexpected heap layout")? {
            memory[range].fill(0);
        }
        let memory = memory.to_vec();
        ensure!(memory.len() <= MAX_MEMORY, "Image exceeds limit");
        Ok(Image::golden(meta, std::sync::Arc::new(memory)))
    }
    /// What changed since `base`, with the dead bytes of the pages it compares
    /// zeroed first: the stack below its pointer (between calls no frame is
    /// live there), and free heap chunks. The turn's leftover frames and the
    /// memory it allocated and freed would otherwise be saved as changes.
    pub(crate) async fn capture(&mut self, base: &Image) -> Result<Capture> {
        let meta = self.metadata().await?;
        let stack = meta.stack as u32 as usize;
        let memory_len = self.memory.data_size(&self.store);
        ensure!(
            memory_len <= MAX_MEMORY && memory_len >= base.len() && stack <= memory_len,
            "Invalid captured memory size"
        );
        let candidates = self.written()?;
        let memory = self.memory.data_mut(&mut self.store);
        let mut compared = vec![false; memory_len / PAGE];
        for &index in &candidates {
            compared[index] = true;
        }
        let dead = std::iter::once(0..stack).chain(heap::dead(memory).unwrap_or_default());
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
        let runtime = self
            .instance
            .get_typed_func::<(), i32>(&mut self.store, "qjs_get_runtime_ptr")?
            .call_async(&mut self.store, ())
            .await?;
        let context = self
            .instance
            .get_typed_func::<(), i32>(&mut self.store, "qjs_get_context_ptr")?
            .call_async(&mut self.store, ())
            .await?;
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
