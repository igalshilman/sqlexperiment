//! The JavaScript engine: quickjs-wasi's reactor ABI (`qjs_*` exports), its
//! imports, and the bridge `guest.js`.
use crate::model::{MAX_INPUT, VmMeta};
use crate::vm::{Host, Uncatchable, Vm, caller_memory, caller_write, host_answer};
use wasmtime::error::Context;
use wasmtime::{Caller, Instance, Linker, Store, TypedFunc};
use wasmtime::{Result, bail, ensure};

pub(crate) const WASM: &[u8] = include_bytes!("../assets/quickjs.wasm");
const BOOTSTRAP: &str = include_str!("../assets/guest.js");
// Below upstream's 512 KiB ceiling: Wasmtime native frames need their own
// headroom, especially in the instrumented interpreter. Reapply after restore.
const MAX_STACK: i32 = 32 * 1024;
const MAX_HEAP: i32 = 32 * 1024 * 1024;
// qjs_init2 flags: every intrinsic but weak references/finalization (bit 9)
// and performance (bit 10).
const INTRINSICS: i32 = !(1 << 9 | 1 << 10);

/// The reactor's exports.
pub(crate) struct QuickJs {
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
    let answer = host_answer(c, &operation, request).await?;
    new_guest_string(c, &answer).await
}

/// The engine's own imports: QuickJS's `env` hooks, and the little WASI it
/// uses beyond the clock and randomness. Guest code gets no external I/O.
pub(crate) fn link(linker: &mut Linker<Host>) -> Result<()> {
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
    // Internal runtime logs are discarded.
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
    Ok(())
}

impl QuickJs {
    pub(crate) fn new(store: &mut Store<Host>, instance: &Instance) -> Result<Self> {
        Ok(Self {
            malloc: instance.get_typed_func(&mut *store, "wasm_malloc")?,
            free: instance.get_typed_func(&mut *store, "wasm_free")?,
            eval_fn: instance.get_typed_func(&mut *store, "qjs_eval")?,
            call_fn: instance.get_typed_func(&mut *store, "qjs_call")?,
            global: instance.get_typed_func(&mut *store, "qjs_get_global")?,
            property: instance.get_typed_func(&mut *store, "qjs_get_prop_string")?,
            new_string: instance.get_typed_func(&mut *store, "qjs_new_string")?,
            free_value: instance.get_typed_func(&mut *store, "qjs_free_value")?,
            is_exception: instance.get_typed_func(&mut *store, "qjs_is_exception")?,
            get_exception: instance.get_typed_func(&mut *store, "qjs_get_exception")?,
            get_string: instance.get_typed_func(&mut *store, "qjs_get_string_len")?,
            free_string: instance.get_typed_func(&mut *store, "qjs_free_cstring")?,
            pending: instance.get_typed_func(&mut *store, "qjs_is_job_pending")?,
            execute: instance.get_typed_func(&mut *store, "qjs_execute_pending_job")?,
        })
    }

    /// Start QuickJS, run the bridge, and evaluate the program's module.
    pub(crate) async fn evaluate(&self, vm: &mut Vm, source: &str) -> Result<()> {
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
        self.limits(vm).await?;
        self.host_function(vm).await?;
        self.eval(vm, BOOTSTRAP, false).await?;
        self.eval_module(vm, source).await?;
        self.drain(vm).await
    }

    /// Evaluate `source` as the program's module, and hand the promise of its
    /// namespace to the bridge (`__durable.bind`).
    async fn eval_module(&self, vm: &mut Vm, source: &str) -> Result<()> {
        const MODULE: i32 = 1;
        let code = self.alloc(vm, source.as_bytes()).await?;
        let filename = self.alloc(vm, b"program.js").await?;
        let namespace = self
            .eval_fn
            .call_async(
                &mut vm.store,
                (code, source.len().try_into()?, filename, MODULE),
            )
            .await?;
        self.free.call_async(&mut vm.store, code).await?;
        self.free.call_async(&mut vm.store, filename).await?;
        ensure!(namespace != 0, "Missing evaluated module");
        if self
            .is_exception
            .call_async(&mut vm.store, namespace)
            .await?
            != 0
        {
            self.free_value.call_async(&mut vm.store, namespace).await?;
            bail!(
                "The program does not compile: {}",
                self.exception(vm).await?
            )
        }
        let global = self.global.call_async(&mut vm.store, ()).await?;
        let key = self.alloc(vm, b"__namespace").await?;
        vm.instance
            .get_typed_func::<(i32, i32, i32), i32>(&mut vm.store, "qjs_set_prop_string")?
            .call_async(&mut vm.store, (global, key, namespace))
            .await?;
        self.free.call_async(&mut vm.store, key).await?;
        self.free_value.call_async(&mut vm.store, namespace).await?;
        self.free_value.call_async(&mut vm.store, global).await?;
        self.eval(
            vm,
            "__durable.bind(globalThis.__namespace); delete globalThis.__namespace",
            false,
        )
        .await?;
        Ok(())
    }

    /// Define the global `__host`, the guest's one host function, which the
    /// bridge takes for itself. The function holds only its name, so a
    /// restored heap calls the same import.
    async fn host_function(&self, vm: &mut Vm) -> Result<()> {
        let name = self.alloc(vm, b"host").await?;
        let function = vm
            .instance
            .get_typed_func::<(i32, i32, i32), i32>(&mut vm.store, "qjs_new_host_function")?
            .call_async(&mut vm.store, (name, 4, 2))
            .await?;
        self.free.call_async(&mut vm.store, name).await?;
        ensure!(function != 0, "Host function creation failed");
        let global = self.global.call_async(&mut vm.store, ()).await?;
        let key = self.alloc(vm, b"__host").await?;
        vm.instance
            .get_typed_func::<(i32, i32, i32), i32>(&mut vm.store, "qjs_set_prop_string")?
            .call_async(&mut vm.store, (global, key, function))
            .await?;
        self.free.call_async(&mut vm.store, key).await?;
        self.free_value.call_async(&mut vm.store, function).await?;
        self.free_value.call_async(&mut vm.store, global).await?;
        Ok(())
    }

    async fn limits(&self, vm: &mut Vm) -> Result<()> {
        vm.instance
            .get_typed_func::<i32, ()>(&mut vm.store, "qjs_set_memory_limit")?
            .call_async(&mut vm.store, MAX_HEAP)
            .await?;
        vm.instance
            .get_typed_func::<i32, ()>(&mut vm.store, "qjs_set_max_stack_size")?
            .call_async(&mut vm.store, MAX_STACK)
            .await?;
        Ok(())
    }

    /// After a restore: point QuickJS at the image's runtime and context, and
    /// reapply the limits.
    pub(crate) async fn restored(&self, vm: &mut Vm, meta: &VmMeta) -> Result<()> {
        vm.instance
            .get_typed_func::<(i32, i32), ()>(&mut vm.store, "qjs_set_runtime_and_context")?
            .call_async(&mut vm.store, (meta.runtime, meta.context))
            .await?;
        self.limits(vm).await
    }

    /// The runtime and context pointers, which an image saves.
    pub(crate) async fn pointers(&self, vm: &mut Vm) -> Result<(i32, i32)> {
        let runtime = vm
            .instance
            .get_typed_func::<(), i32>(&mut vm.store, "qjs_get_runtime_ptr")?
            .call_async(&mut vm.store, ())
            .await?;
        let context = vm
            .instance
            .get_typed_func::<(), i32>(&mut vm.store, "qjs_get_context_ptr")?
            .call_async(&mut vm.store, ())
            .await?;
        Ok((runtime, context))
    }

    async fn alloc(&self, vm: &mut Vm, data: &[u8]) -> Result<i32> {
        let ptr = self
            .malloc
            .call_async(&mut vm.store, (data.len() + 1).try_into()?)
            .await?;
        ensure!(ptr != 0, "Guest allocation failed");
        vm.write(ptr as u32 as usize, data)?;
        vm.write(ptr as u32 as usize + data.len(), &[0])?;
        Ok(ptr)
    }

    async fn string(&self, vm: &mut Vm, value: i32) -> Result<String> {
        let len_ptr = self.alloc(vm, &[0; 4]).await?;
        let ptr = self
            .get_string
            .call_async(&mut vm.store, (value, len_ptr))
            .await?;
        ensure!(ptr != 0, "String conversion failed");
        let mut len = [0; 4];
        vm.memory
            .read(&vm.store, len_ptr as u32 as usize, &mut len)?;
        let len = u32::from_le_bytes(len) as usize;
        ensure!(len <= MAX_INPUT, "Host result exceeds limit");
        let mut bytes = vec![0; len];
        vm.memory.read(&vm.store, ptr as u32 as usize, &mut bytes)?;
        self.free_string.call_async(&mut vm.store, ptr).await?;
        self.free.call_async(&mut vm.store, len_ptr).await?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    async fn exception(&self, vm: &mut Vm) -> Result<String> {
        let ptr = self.get_exception.call_async(&mut vm.store, ()).await?;
        ensure!(ptr != 0, "Missing exception value");
        let result = self.string(vm, ptr).await;
        self.free_value.call_async(&mut vm.store, ptr).await?;
        result
    }

    async fn eval(&self, vm: &mut Vm, code: &str, stringify: bool) -> Result<Option<String>> {
        let code_ptr = self.alloc(vm, code.as_bytes()).await?;
        let filename = self.alloc(vm, b"program.js").await?;
        let value = self
            .eval_fn
            .call_async(
                &mut vm.store,
                (code_ptr, code.len().try_into()?, filename, 0),
            )
            .await?;
        self.free.call_async(&mut vm.store, code_ptr).await?;
        self.free.call_async(&mut vm.store, filename).await?;
        ensure!(value != 0, "Missing evaluated value");
        let is_exception = self.is_exception.call_async(&mut vm.store, value).await? != 0;
        let result = if !is_exception && stringify {
            Some(self.string(vm, value).await?)
        } else {
            None
        };
        self.free_value.call_async(&mut vm.store, value).await?;
        if is_exception {
            bail!("Guest exception: {}", self.exception(vm).await?)
        }
        Ok(result)
    }

    /// Call a bridge hook, `__durable.<name>(argument)`, with an optional
    /// string argument, returning its result as a string. A direct call: no
    /// source to compile, so it costs a fraction of an eval, and no argument
    /// can inject source. Every handle is freed before returning; after an
    /// error the turn is discarded, so leaks then don't matter.
    pub(crate) async fn call(
        &self,
        vm: &mut Vm,
        name: &str,
        argument: Option<&str>,
    ) -> Result<String> {
        let global = self.global.call_async(&mut vm.store, ()).await?;
        ensure!(global != 0, "Missing global object");
        let bridge = self.property(vm, global, "__durable").await?;
        self.free_value.call_async(&mut vm.store, global).await?;
        let function = self.property(vm, bridge, name).await?;
        let (argc, argv, value, text) = match argument {
            Some(argument) => {
                let text = self.alloc(vm, argument.as_bytes()).await?;
                let value = self
                    .new_string
                    .call_async(&mut vm.store, (text, argument.len().try_into()?))
                    .await?;
                ensure!(value != 0, "Guest string allocation failed");
                let argv = self.alloc(vm, &value.to_le_bytes()).await?;
                (1, argv, value, text)
            }
            None => (0, 0, 0, 0),
        };
        let result = self
            .call_fn
            .call_async(&mut vm.store, (function, bridge, argc, argv))
            .await?;
        for handle in [function, bridge, value] {
            if handle != 0 {
                self.free_value.call_async(&mut vm.store, handle).await?;
            }
        }
        for buffer in [argv, text] {
            if buffer != 0 {
                self.free.call_async(&mut vm.store, buffer).await?;
            }
        }
        ensure!(result != 0, "Missing call result");
        if self.is_exception.call_async(&mut vm.store, result).await? != 0 {
            self.free_value.call_async(&mut vm.store, result).await?;
            bail!("Guest exception: {}", self.exception(vm).await?)
        }
        let string = self.string(vm, result).await;
        self.free_value.call_async(&mut vm.store, result).await?;
        string
    }

    async fn property(&self, vm: &mut Vm, object: i32, name: &str) -> Result<i32> {
        let key = self.alloc(vm, name.as_bytes()).await?;
        let value = self
            .property
            .call_async(&mut vm.store, (object, key))
            .await?;
        self.free.call_async(&mut vm.store, key).await?;
        ensure!(value != 0, "Missing property {name}");
        Ok(value)
    }

    /// Run pending jobs until there are none.
    pub(crate) async fn drain(&self, vm: &mut Vm) -> Result<()> {
        while self.pending.call_async(&mut vm.store, ()).await? != 0 {
            if self.execute.call_async(&mut vm.store, ()).await? < 0 {
                return Err(wasmtime::Error::new(Uncatchable(self.exception(vm).await?)));
            }
        }
        Ok(())
    }
}
