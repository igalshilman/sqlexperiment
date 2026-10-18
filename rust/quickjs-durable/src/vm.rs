use crate::heap;
use crate::model::{Capture, Image, MAX_INPUT, MAX_MEMORY, PAGE, Report, VmMeta, fingerprint};
use wasmtime::error::Context;
use wasmtime::{
    Caller, Config, Engine, Global, Instance, InstanceAllocationStrategy, InstancePre, Linker,
    Memory, Module, PoolConcurrencyLimitError, PoolingAllocationConfig, Store, StoreLimits,
    StoreLimitsBuilder, TypedFunc, Val,
};
use wasmtime::{Result, bail, ensure};

const WASM: &[u8] = include_bytes!("../assets/quickjs.wasm");
const BOOTSTRAP: &str = include_str!("../assets/guest.js");
const FUEL: u64 = 200_000_000;
/// Fuel left when a turn stops at its next safe boundary to checkpoint, so a
/// long run of local requests makes progress across turns instead of trapping
/// in every one of them. The rest covers that boundary's batch and capture.
const CHECKPOINT_FUEL: u64 = FUEL / 2;
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
pub(crate) const VERSION: u64 = 3;

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
}
struct Host {
    now: u64,
    seed: [u8; 8],
    limits: StoreLimits,
}

impl Runtime {
    pub fn new() -> Result<Self> {
        validate_snapshot_abi(WASM)?;
        let mut config = Config::new();
        config.consume_fuel(true);
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
        let engine = Engine::new(&config)?;
        #[cfg(not(feature = "full-scan"))]
        let instrumented =
            crate::instrument::instrument(WASM).map_err(wasmtime::Error::from_anyhow)?;
        #[cfg(not(feature = "full-scan"))]
        let wasm = instrumented.as_slice();
        #[cfg(feature = "full-scan")]
        let wasm = WASM;
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
        Ok(Self { engine, instance })
    }

    pub(crate) fn golden(&self, name: &str, source: &str) -> Result<Image> {
        let mut vm = Vm::new(self, &format!("program/{name}"), 0)?;
        vm.instance
            .get_typed_func::<(), ()>(&mut vm.store, "_initialize")?
            .call(&mut vm.store, ())?;
        ensure!(
            vm.instance
                .get_typed_func::<i32, i32>(&mut vm.store, "qjs_init2")?
                .call(&mut vm.store, INTRINSICS)?
                == 0,
            "QuickJS initialization failed"
        );
        vm.limits()?;
        vm.eval(BOOTSTRAP, false)?;
        vm.eval(source, false)?;
        vm.drain()?;
        ensure!(
            vm.eval("typeof main === 'function'", true)?.as_deref() == Some("true"),
            "A program must define main(input, ctx)"
        );
        vm.snapshot()
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
/// guest ran out of fuel, or QuickJS hit an error no script can catch: its
/// image is mid-mutation, and the same image and delivery would fail again.
pub(crate) fn trap(error: &wasmtime::Error) -> Option<String> {
    if let Some(trap) = error.downcast_ref::<wasmtime::Trap>() {
        return Some(match trap {
            wasmtime::Trap::OutOfFuel => {
                "Out of fuel: one synchronous job ran longer than a turn allows".into()
            }
            trap => trap.to_string(),
        });
    }
    error.downcast_ref::<Uncatchable>().map(ToString::to_string)
}

/// A live QuickJS VM: one Wasm instance, for one run, turn after turn.
pub(crate) struct Vm {
    store: Store<Host>,
    instance: Instance,
    memory: Memory,
    stack: Global,
    #[cfg(not(feature = "full-scan"))]
    dirty: Memory,
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
    #[cfg(not(feature = "full-scan"))]
    {
        let dirty = c
            .get_export(crate::instrument::EXPORT)
            .and_then(|e| e.into_memory())
            .context("Missing tracker")?;
        mark_host_range(dirty.data_mut(c), offset, bytes.len())?;
    }
    Ok(())
}
#[cfg(not(feature = "full-scan"))]
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
    linker.func_wrap(
        "env",
        "host_call",
        |_: i32, _: i32, _: i32, _: i32, _: i32| -> Result<i32> {
            bail!("Host callbacks are not enabled")
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
    pub(crate) fn new(runtime: &Runtime, identity: &str, now: u64) -> Result<Self> {
        let host = Host {
            now,
            seed: fingerprint(identity.as_bytes()).to_le_bytes(),
            limits: StoreLimitsBuilder::new()
                .memory_size(MAX_MEMORY)
                .instances(1)
                .build(),
        };
        let mut store = Store::new(&runtime.engine, host);
        store.limiter(|h| &mut h.limits);
        store.set_fuel(FUEL)?;
        let instance = runtime.instance.instantiate(&mut store)?;
        let memory = instance
            .get_memory(&mut store, "memory")
            .context("Missing memory export")?;
        let stack = instance
            .get_global(&mut store, "__stack_pointer")
            .context("Missing stack export")?;
        #[cfg(not(feature = "full-scan"))]
        let dirty = instance
            .get_memory(&mut store, crate::instrument::EXPORT)
            .context("Missing tracker")?;
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
            #[cfg(not(feature = "full-scan"))]
            dirty,
        })
    }
    /// Begin a turn at logical time `now`, with a full fuel budget.
    pub(crate) fn begin_turn(&mut self, now: u64) -> Result<()> {
        self.store.data_mut().now = now;
        self.store.set_fuel(FUEL)
    }
    /// Whether the turn has spent half its fuel: time to stop at the next safe
    /// boundary, so a long run of local requests goes on in the next turn
    /// instead of running out.
    pub(crate) fn tired(&self) -> Result<bool> {
        Ok(self.store.get_fuel()? < CHECKPOINT_FUEL)
    }
    /// The captured pages are committed: track writes anew.
    pub(crate) fn committed(&mut self) {
        #[cfg(not(feature = "full-scan"))]
        self.dirty.data_mut(&mut self.store).fill(0);
    }
    fn limits(&mut self) -> Result<()> {
        self.instance
            .get_typed_func::<i32, ()>(&mut self.store, "qjs_set_memory_limit")?
            .call(&mut self.store, MAX_HEAP)?;
        self.instance
            .get_typed_func::<i32, ()>(&mut self.store, "qjs_set_max_stack_size")?
            .call(&mut self.store, MAX_STACK)?;
        Ok(())
    }
    pub(crate) fn restore(&mut self, image: &Image) -> Result<()> {
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
        #[cfg(not(feature = "full-scan"))]
        self.dirty.data_mut(&mut self.store).fill(0);
        self.stack
            .set(&mut self.store, Val::I32(image.meta.stack))?;
        self.instance
            .get_typed_func::<(i32, i32), ()>(&mut self.store, "qjs_set_runtime_and_context")?
            .call(&mut self.store, (image.meta.runtime, image.meta.context))?;
        self.limits()
    }
    fn alloc(&mut self, data: &[u8]) -> Result<i32> {
        let ptr = self
            .malloc
            .call(&mut self.store, (data.len() + 1).try_into()?)?;
        ensure!(ptr != 0, "Guest allocation failed");
        self.write(ptr as u32 as usize, data)?;
        self.write(ptr as u32 as usize + data.len(), &[0])?;
        Ok(ptr)
    }
    fn string(&mut self, value: i32) -> Result<String> {
        let len_ptr = self.alloc(&[0; 4])?;
        let ptr = self.get_string.call(&mut self.store, (value, len_ptr))?;
        ensure!(ptr != 0, "String conversion failed");
        let mut len = [0; 4];
        self.memory
            .read(&self.store, len_ptr as u32 as usize, &mut len)?;
        let len = u32::from_le_bytes(len) as usize;
        ensure!(len <= MAX_INPUT, "Host result exceeds limit");
        let mut bytes = vec![0; len];
        self.memory
            .read(&self.store, ptr as u32 as usize, &mut bytes)?;
        self.free_string.call(&mut self.store, ptr)?;
        self.free.call(&mut self.store, len_ptr)?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }
    fn exception(&mut self) -> Result<String> {
        let ptr = self.get_exception.call(&mut self.store, ())?;
        ensure!(ptr != 0, "Missing exception value");
        let result = self.string(ptr);
        self.free_value.call(&mut self.store, ptr)?;
        result
    }
    fn eval(&mut self, code: &str, stringify: bool) -> Result<Option<String>> {
        let code_ptr = self.alloc(code.as_bytes())?;
        let filename = self.alloc(b"program.js")?;
        let value = self.eval_fn.call(
            &mut self.store,
            (code_ptr, code.len().try_into()?, filename, 0),
        )?;
        self.free.call(&mut self.store, code_ptr)?;
        self.free.call(&mut self.store, filename)?;
        ensure!(value != 0, "Missing evaluated value");
        let is_exception = self.is_exception.call(&mut self.store, value)? != 0;
        let result = if !is_exception && stringify {
            Some(self.string(value)?)
        } else {
            None
        };
        self.free_value.call(&mut self.store, value)?;
        if is_exception {
            bail!("Guest exception: {}", self.exception()?)
        }
        Ok(result)
    }
    /// Call a bridge hook, `__durable.<name>(argument)`, with an optional
    /// string argument, returning its result as a string. A direct call: no
    /// source to compile, so it costs a fraction of an eval, and no argument
    /// can inject source. Every handle is freed before returning; after an
    /// error the turn is discarded, so leaks then don't matter.
    pub(crate) fn call(&mut self, name: &str, argument: Option<&str>) -> Result<String> {
        let global = self.global.call(&mut self.store, ())?;
        ensure!(global != 0, "Missing global object");
        let bridge = self.property(global, "__durable")?;
        self.free_value.call(&mut self.store, global)?;
        let function = self.property(bridge, name)?;
        let (argc, argv, value, text) = match argument {
            Some(argument) => {
                let text = self.alloc(argument.as_bytes())?;
                let value = self
                    .new_string
                    .call(&mut self.store, (text, argument.len().try_into()?))?;
                ensure!(value != 0, "Guest string allocation failed");
                let argv = self.alloc(&value.to_le_bytes())?;
                (1, argv, value, text)
            }
            None => (0, 0, 0, 0),
        };
        let result = self
            .call_fn
            .call(&mut self.store, (function, bridge, argc, argv))?;
        for handle in [function, bridge, value] {
            if handle != 0 {
                self.free_value.call(&mut self.store, handle)?;
            }
        }
        for buffer in [argv, text] {
            if buffer != 0 {
                self.free.call(&mut self.store, buffer)?;
            }
        }
        ensure!(result != 0, "Missing call result");
        if self.is_exception.call(&mut self.store, result)? != 0 {
            self.free_value.call(&mut self.store, result)?;
            bail!("Guest exception: {}", self.exception()?)
        }
        let string = self.string(result);
        self.free_value.call(&mut self.store, result)?;
        string
    }
    fn property(&mut self, object: i32, name: &str) -> Result<i32> {
        let key = self.alloc(name.as_bytes())?;
        let value = self.property.call(&mut self.store, (object, key))?;
        self.free.call(&mut self.store, key)?;
        ensure!(value != 0, "Missing property {name}");
        Ok(value)
    }
    pub(crate) fn invoke(&mut self, name: &str, value: &serde_json::Value) -> Result<()> {
        self.call(name, Some(&serde_json::to_string(value)?))?;
        Ok(())
    }
    /// What the guest issued since it last reported, and where it stands.
    pub(crate) fn inspect(&mut self) -> Result<Report> {
        Ok(serde_json::from_str(&self.call("inspect", None)?)?)
    }
    pub(crate) fn drain(&mut self) -> Result<()> {
        while self.pending.call(&mut self.store, ())? != 0 {
            if self.execute.call(&mut self.store, ())? < 0 {
                return Err(wasmtime::Error::new(Uncatchable(self.exception()?)));
            }
        }
        Ok(())
    }
    fn write(&mut self, offset: usize, bytes: &[u8]) -> Result<()> {
        self.memory.write(&mut self.store, offset, bytes)?;
        #[cfg(not(feature = "full-scan"))]
        mark_host_range(self.dirty.data_mut(&mut self.store), offset, bytes.len())?;
        Ok(())
    }
    fn snapshot(&mut self) -> Result<Image> {
        // Only golden registration needs a complete copy of guest memory.
        let meta = self.metadata()?;
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
    pub(crate) fn capture(&mut self, base: &Image) -> Result<Capture> {
        let meta = self.metadata()?;
        let stack = meta.stack as u32 as usize;
        let memory_len = self.memory.data_size(&self.store);
        ensure!(
            memory_len <= MAX_MEMORY && memory_len >= base.len() && stack <= memory_len,
            "Invalid captured memory size"
        );
        #[cfg(not(feature = "full-scan"))]
        let candidates: Vec<usize> = self
            .dirty
            .data(&self.store)
            .iter()
            .take(memory_len / PAGE)
            .enumerate()
            .filter_map(|(index, bit)| (*bit != 0).then_some(index))
            .collect();
        #[cfg(feature = "full-scan")]
        let candidates: Vec<usize> = (0..memory_len / PAGE).collect();
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
        #[cfg(all(feature = "verify-dirty", not(feature = "full-scan")))]
        let dirty = self.dirty.data(&self.store);
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
        #[cfg(all(feature = "verify-dirty", not(feature = "full-scan")))]
        for (index, next) in memory.as_chunks::<PAGE>().0.iter().enumerate() {
            ensure!(
                next == base.page(index as u32) || dirty[index] != 0,
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
    fn metadata(&mut self) -> Result<VmMeta> {
        let runtime = self
            .instance
            .get_typed_func::<(), i32>(&mut self.store, "qjs_get_runtime_ptr")?
            .call(&mut self.store, ())?;
        let context = self
            .instance
            .get_typed_func::<(), i32>(&mut self.store, "qjs_get_context_ptr")?
            .call(&mut self.store, ())?;
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
