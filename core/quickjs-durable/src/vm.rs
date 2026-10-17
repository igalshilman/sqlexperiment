use crate::model::{
    Application, Capture, Event, Image, MAX_INPUT, MAX_MEMORY, MAX_TIME, PAGE, State, VmMeta, hash,
};
use crate::{Operation, Resolution, Transaction};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    num::NonZeroUsize,
    sync::{Arc, Mutex},
};
use wasmtime::error::Context;
use wasmtime::{
    Caller, Config, Engine, Global, Instance, Linker, Memory, Module, Store, StoreLimits,
    StoreLimitsBuilder, TypedFunc, Val,
};
use wasmtime::{Result, bail, ensure};

const WASM: &[u8] = include_bytes!("../assets/quickjs.wasm");
const BOOTSTRAP: &str = include_str!("../assets/guest.js");
const FUEL: u64 = 200_000_000;
// Below upstream's 512 KiB ceiling: Wasmtime native frames need their own
// headroom, especially in the instrumented interpreter. Reapply after restore.
const MAX_STACK: i32 = 32 * 1024;
const MAX_HEAP: i32 = 32 * 1024 * 1024;
// qjs_init2 flags: every intrinsic but weak references/finalization (bit 9)
// and performance (bit 10).
const INTRINSICS: i32 = !(1 << 9 | 1 << 10);
/// What a restored image depends on besides the Wasm and the bridge source.
/// Bump the version when the restore contract changes in a way these inputs
/// don't show: the imports' behavior, the bridge protocol, the VM metadata.
/// Editing the host's code otherwise keeps every stored run restorable.
const ABI: &str = "abi=1;wasmtime=49.0.3";

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

/// One compiled engine/module shared by many invocations. Each turn deliberately
/// uses a fresh Store so cold restoration is continuously exercised. This stores
/// guest promises, never Rust futures or Wasmtime execution stacks.
pub struct Runtime {
    engine: Engine,
    module: Module,
    pub(crate) build: String,
    /// Decompressed golden memory per application version (ID and source).
    /// Golden creation is deterministic for a build, so an entry is valid for
    /// any store whose catalog names that version.
    goldens: Mutex<HashMap<String, Arc<Vec<u8>>>>,
}
// The source hash is hexadecimal: no `/` in it, so the key is unambiguous.
fn golden_key(application: &Application) -> String {
    format!("{}/{}", application.source_hash, application.id)
}

struct Host {
    now: u64,
    seed: [u8; 32],
    limits: StoreLimits,
}

impl Runtime {
    pub fn new() -> Result<Self> {
        validate_snapshot_abi(WASM)?;
        let mut config = Config::new();
        config.consume_fuel(true);
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
        // The instrumented Wasm already covers the tracking transform.
        let limits = format!("{ABI};stack={MAX_STACK};heap={MAX_HEAP};intrinsics={INTRINSICS}");
        let build = hash(&[wasm, BOOTSTRAP.as_bytes(), limits.as_bytes()].concat());
        Ok(Self {
            engine,
            module,
            build,
            goldens: Mutex::default(),
        })
    }

    /// The application's golden memory, if this runtime has decompressed it.
    pub(crate) fn cached_golden(&self, application: &Application) -> Option<Arc<Vec<u8>>> {
        self.goldens
            .lock()
            .unwrap()
            .get(&golden_key(application))
            .cloned()
    }
    pub(crate) fn cache_golden(&self, application: &Application, memory: Vec<u8>) -> Arc<Vec<u8>> {
        let memory = Arc::new(memory);
        let mut goldens = self.goldens.lock().unwrap();
        goldens.insert(golden_key(application), memory.clone());
        memory
    }
    pub fn build_id(&self) -> &str {
        &self.build
    }

    pub(crate) fn golden(&self, id: &str, source: &str) -> Result<Image> {
        ensure!(
            source.len() <= MAX_INPUT,
            "Application source exceeds limit"
        );
        let mut vm = Vm::new(self, &format!("application/{id}"), 0)?;
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
            "Application must define main(input)"
        );
        vm.snapshot()
    }

    pub(crate) async fn turn(
        &self,
        run: &str,
        image: &Image,
        event: &Event,
        pending: &[Operation],
        transaction: &mut impl Transaction,
        checkpoint_interval: Option<NonZeroUsize>,
    ) -> Result<(Capture, State)> {
        let mut vm = Vm::new(self, run, event.now())?;
        vm.restore(image)?;
        match event {
            Event::Start { input, .. } => {
                let seed = u32::from_be_bytes(Sha256::digest(run.as_bytes())[..4].try_into()?);
                vm.invoke(
                    "init",
                    &json!({"runId":run,"now":event.now(),"maxTime":MAX_TIME,"seed":seed}),
                )?;
                vm.invoke("start", input)?;
            }
            Event::Complete { .. } => vm.invoke("deliver", &serde_json::to_value(event)?)?,
        }
        // Requests already present in the restored image belong to existing
        // host effects. Recovery/completion must not emit them again.
        let mut handled: HashSet<String> = pending.iter().map(|op| op.id().to_owned()).collect();
        let mut ready_operations = 0_usize;
        let state = loop {
            vm.drain()?;
            let state: State = serde_json::from_str(
                &vm.eval("__durable.inspect()", true)?
                    .context("Missing state")?,
            )?;
            let mut ready = false;
            for operation in &state.pending {
                if !handled.insert(operation.id().to_owned()) {
                    continue;
                }
                match transaction
                    .execute(operation)
                    .await
                    .map_err(wasmtime::Error::from_anyhow)?
                {
                    Resolution::Ready(outcome) => {
                        ensure!(
                            !matches!(
                                operation,
                                Operation::Resume { .. } | Operation::Sleep { .. }
                            ),
                            "Resume wakeups and timers must remain pending until commit"
                        );
                        outcome.validate().map_err(wasmtime::Error::from_anyhow)?;
                        ensure!(
                            serde_json::to_vec(&outcome)?.len() <= MAX_INPUT,
                            "Host result exceeds limit"
                        );
                        let completion = Event::Complete {
                            operation_id: operation.id().into(),
                            outcome,
                            now: event.now(),
                        };
                        vm.invoke("deliver", &serde_json::to_value(completion)?)?;
                        ready = true;
                        ready_operations += 1;
                    }
                    Resolution::Pending => {
                        ensure!(
                            !matches!(
                                operation,
                                Operation::KvGet { .. }
                                    | Operation::KvSet { .. }
                                    | Operation::KvDelete { .. }
                            ),
                            "KV must resolve inside the transaction"
                        );
                    }
                }
            }
            if !ready {
                break state;
            }
            if checkpoint_interval.is_some_and(|limit| ready_operations >= limit.get()) {
                // Every operation in this batch has been handled, and ready
                // outcomes are now in the heap/job queue. No Wasm call or Rust
                // operation future is live. Persist a local wakeup so recovery
                // can schedule these queued continuations without replaying I/O.
                vm.eval("__durable.pause()", false)?;
                let state: State = serde_json::from_str(
                    &vm.eval("__durable.inspect()", true)?
                        .context("Missing state")?,
                )?;
                let wakeup = state.pending.last().context("Missing resume wakeup")?;
                ensure!(
                    matches!(wakeup, Operation::Resume { .. }),
                    "Invalid resume wakeup"
                );
                ensure!(
                    matches!(
                        transaction
                            .execute(wakeup)
                            .await
                            .map_err(wasmtime::Error::from_anyhow)?,
                        Resolution::Pending
                    ),
                    "Resume wakeup must be staged until commit"
                );
                break state;
            }
        };
        if let Some(outcome) = &state.outcome {
            outcome.validate().map_err(wasmtime::Error::from_anyhow)?;
        }
        let next = vm.capture(image)?;
        Ok((next, state))
    }
}

struct Vm {
    store: Store<Host>,
    instance: Instance,
    memory: Memory,
    stack: Global,
    #[cfg(not(feature = "full-scan"))]
    dirty: Memory,
    malloc: TypedFunc<i32, i32>,
    free: TypedFunc<i32, ()>,
    eval_fn: TypedFunc<(i32, i32, i32, i32), i32>,
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

impl Vm {
    fn new(runtime: &Runtime, identity: &str, now: u64) -> Result<Self> {
        let host = Host {
            now,
            seed: Sha256::digest(identity.as_bytes()).into(),
            limits: StoreLimitsBuilder::new()
                .memory_size(MAX_MEMORY)
                .instances(1)
                .build(),
        };
        let mut store = Store::new(&runtime.engine, host);
        store.limiter(|h| &mut h.limits);
        store.set_fuel(FUEL)?;
        let mut linker = Linker::new(&runtime.engine);
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
                let bytes: Vec<u8> = (0..len).map(|i| seed[i % 32]).collect();
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
        let instance = linker.instantiate(&mut store, &runtime.module)?;
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
    fn limits(&mut self) -> Result<()> {
        self.instance
            .get_typed_func::<i32, ()>(&mut self.store, "qjs_set_memory_limit")?
            .call(&mut self.store, MAX_HEAP)?;
        self.instance
            .get_typed_func::<i32, ()>(&mut self.store, "qjs_set_max_stack_size")?
            .call(&mut self.store, MAX_STACK)?;
        Ok(())
    }
    fn restore(&mut self, image: &Image) -> Result<()> {
        ensure!(
            image.memory.len() <= MAX_MEMORY && image.memory.len().is_multiple_of(65536),
            "Invalid image size"
        );
        let current = self.memory.data_size(&self.store);
        ensure!(
            image.memory.len() >= current,
            "Image smaller than module's initial memory"
        );
        if image.memory.len() > current {
            self.memory.grow(
                &mut self.store,
                ((image.memory.len() - current) / 65536) as u64,
            )?;
        }
        self.memory.write(&mut self.store, 0, &image.memory)?;
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
    fn invoke(&mut self, name: &str, value: &serde_json::Value) -> Result<()> {
        // Double JSON encoding passes a string argument without source injection.
        let argument = serde_json::to_string(&serde_json::to_string(value)?)?;
        self.eval(&format!("__durable.{name}({argument})"), false)?;
        Ok(())
    }
    fn drain(&mut self) -> Result<()> {
        while self.pending.call(&mut self.store, ())? != 0 {
            if self.execute.call(&mut self.store, ())? < 0 {
                bail!("Guest job failed: {}", self.exception()?)
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
        let memory = self.memory.data(&self.store).to_vec();
        ensure!(memory.len() <= MAX_MEMORY, "Image exceeds limit");
        Ok(Image { meta, memory })
    }
    fn capture(&mut self, base: &Image) -> Result<Capture> {
        let meta = self.metadata()?;
        let memory = self.memory.data(&self.store);
        ensure!(
            memory.len() <= MAX_MEMORY && memory.len() >= base.memory.len(),
            "Invalid captured memory size"
        );
        #[cfg(not(feature = "full-scan"))]
        let dirty = self.dirty.data(&self.store);
        let mut pages = Vec::new();
        let mut examined_pages = 0;
        #[cfg(not(feature = "full-scan"))]
        let candidates = dirty
            .iter()
            .take(memory.len() / PAGE)
            .enumerate()
            .filter_map(|(index, bit)| (*bit != 0).then_some(index));
        #[cfg(feature = "full-scan")]
        let candidates = 0..memory.len() / PAGE;
        for index in candidates {
            examined_pages += 1;
            let range = index * PAGE..(index + 1) * PAGE;
            let previous = base.memory.get(range.clone()).unwrap_or(&[0; PAGE]);
            let next = &memory[range];
            if next != previous {
                pages.push((index as u32, next.to_vec()));
            }
        }
        #[cfg(all(feature = "verify-dirty", not(feature = "full-scan")))]
        for (index, next) in memory.as_chunks::<PAGE>().0.iter().enumerate() {
            let range = index * PAGE..(index + 1) * PAGE;
            let previous = base.memory.get(range).unwrap_or(&[0; PAGE]);
            ensure!(
                next == previous || dirty[index] != 0,
                "Dirty tracker missed page {index}"
            );
        }
        Ok(Capture {
            meta,
            memory_len: memory.len(),
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
