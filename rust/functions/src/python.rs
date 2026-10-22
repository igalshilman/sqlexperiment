//! The Python engine: CPython 3.15, built for WASI as a reactor
//! (`python/reactor.c`, by `python/build.sh`), with the bridge `guest.py`.
//!
//! The reactor exports a small call surface: start the interpreter, run
//! source, call one of the bridge's entry points with a string and read back
//! the string it returns. Python code reaches the host through one import,
//! `durable.host`, which answers host calls (the database's, extensions')
//! synchronously.
use crate::model::MAX_INPUT;
use crate::vm::{Host, Vm, caller_memory, caller_write, host_answer};
use crate::wasi::Filesystem;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use wasmtime::error::Context as _;
use wasmtime::{Caller, Instance, Linker, Store, TypedFunc};
use wasmtime::{Result, bail, ensure};

pub(crate) const BRIDGE: &str = include_str!("../assets/guest.py");
/// Where the standard library is mounted in the guest's filesystem.
const STDLIB: &str = "lib/python3.15";

/// The built engine: the reactor's Wasm and the standard library's sources.
pub(crate) struct Build {
    pub(crate) wasm: Vec<u8>,
    pub(crate) filesystem: Arc<Filesystem>,
}

impl Build {
    /// The build in `FUNCTIONS_PYTHON`, by default `target/pywasm/out`, where
    /// `make python-engine` puts it.
    pub(crate) fn load() -> Result<Self> {
        let dir = std::env::var_os("FUNCTIONS_PYTHON")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/pywasm/out")
            });
        let wasm = std::fs::read(dir.join("python.wasm")).with_context(|| {
            format!(
                "No Python engine in {}: run `make python-engine`",
                dir.display()
            )
        })?;
        let filesystem = Filesystem::load(&dir.join("lib"), STDLIB)?;
        Ok(Self {
            wasm,
            filesystem: Arc::new(filesystem),
        })
    }
}

/// The reactor's exports.
pub(crate) struct Python {
    alloc: TypedFunc<i32, i32>,
    free: TypedFunc<i32, ()>,
    result: TypedFunc<(), i32>,
    result_len: TypedFunc<(), i32>,
    call_fn: TypedFunc<(i32, i32, i32, i32), i32>,
}

impl Python {
    pub(crate) fn new(store: &mut Store<Host>, instance: &Instance) -> Result<Self> {
        Ok(Self {
            alloc: instance.get_typed_func(&mut *store, "py_alloc")?,
            free: instance.get_typed_func(&mut *store, "py_free")?,
            result: instance.get_typed_func(&mut *store, "py_result")?,
            result_len: instance.get_typed_func(&mut *store, "py_result_len")?,
            call_fn: instance.get_typed_func(&mut *store, "py_call")?,
        })
    }

    async fn alloc(&self, vm: &mut Vm, data: &[u8]) -> Result<i32> {
        let ptr = self
            .alloc
            .call_async(&mut vm.store, data.len().try_into()?)
            .await?;
        ensure!(ptr != 0, "Guest allocation failed");
        vm.write(ptr as u32 as usize, data)?;
        Ok(ptr)
    }

    /// The last call's result, or its error's traceback.
    async fn result(&self, vm: &mut Vm) -> Result<String> {
        let ptr = self.result.call_async(&mut vm.store, ()).await?;
        let len = self.result_len.call_async(&mut vm.store, ()).await? as u32 as usize;
        ensure!(len <= MAX_INPUT, "Guest result exceeds limit");
        let mut bytes = vec![0; len];
        vm.memory.read(&vm.store, ptr as u32 as usize, &mut bytes)?;
        Ok(String::from_utf8(bytes)?)
    }

    /// Start the interpreter and run the bridge: the base every program's
    /// golden image starts from.
    pub(crate) async fn boot(&self, vm: &mut Vm) -> Result<()> {
        vm.instance
            .get_typed_func::<(), ()>(&mut vm.store, "_initialize")?
            .call_async(&mut vm.store, ())
            .await?;
        let init = vm
            .instance
            .get_typed_func::<(), i32>(&mut vm.store, "py_init")?;
        if init.call_async(&mut vm.store, ()).await? != 0 {
            bail!("Python initialization failed: {}", self.result(vm).await?);
        }
        let exec = vm
            .instance
            .get_typed_func::<(i32, i32, i32, i32), i32>(&mut vm.store, "py_exec")?;
        let code = self.alloc(vm, BRIDGE.as_bytes()).await?;
        let file = self.alloc(vm, b"guest.py").await?;
        let status = exec
            .call_async(&mut vm.store, (code, BRIDGE.len().try_into()?, file, 8))
            .await?;
        self.free.call_async(&mut vm.store, code).await?;
        self.free.call_async(&mut vm.store, file).await?;
        if status != 0 {
            bail!("The Python bridge failed: {}", self.result(vm).await?);
        }
        Ok(())
    }

    /// Load a program into a booted VM: its module, whose functions are its
    /// handlers.
    pub(crate) async fn load(&self, vm: &mut Vm, name: &str, source: &str) -> Result<()> {
        let program = serde_json::json!({"name": name, "source": source}).to_string();
        self.call(vm, "load", Some(&program)).await?;
        Ok(())
    }

    /// Call the bridge's `__durable.<name>(argument)`; the string it returns.
    pub(crate) async fn call(
        &self,
        vm: &mut Vm,
        name: &str,
        argument: Option<&str>,
    ) -> Result<String> {
        let name_ptr = self.alloc(vm, name.as_bytes()).await?;
        let (argument_ptr, argument_len) = match argument {
            Some(argument) => (
                self.alloc(vm, argument.as_bytes()).await?,
                argument.len().try_into()?,
            ),
            None => (0, -1),
        };
        let status = self
            .call_fn
            .call_async(
                &mut vm.store,
                (name_ptr, name.len().try_into()?, argument_ptr, argument_len),
            )
            .await?;
        self.free.call_async(&mut vm.store, name_ptr).await?;
        if argument_ptr != 0 {
            self.free.call_async(&mut vm.store, argument_ptr).await?;
        }
        let result = self.result(vm).await?;
        if status != 0 {
            bail!("Guest exception: {result}");
        }
        Ok(result)
    }
}

/// The guest's host function: `host(operation, request)` answers with the
/// length of its JSON answer, which `host_read` then copies into the guest.
pub(crate) fn link(linker: &mut Linker<Host>) -> Result<()> {
    linker.func_wrap_async(
        "durable",
        "host",
        |mut c: Caller<'_, Host>,
         (operation, operation_len, request, request_len): (i32, i32, i32, i32)| {
            Box::new(async move {
                let memory = caller_memory(&mut c)?;
                let read = |c: &Caller<'_, Host>, ptr: i32, len: i32| -> Result<String> {
                    let len = len as u32 as usize;
                    ensure!(len <= MAX_INPUT, "Host call exceeds limit");
                    let mut bytes = vec![0; len];
                    memory.read(c, ptr as u32 as usize, &mut bytes)?;
                    Ok(String::from_utf8(bytes)?)
                };
                let operation = read(&c, operation, operation_len)?;
                let request = serde_json::from_str(&read(&c, request, request_len)?)?;
                let answer = host_answer(&mut c, &operation, request).await?;
                let len: i32 = answer.len().try_into()?;
                c.data_mut().answer = answer.into_bytes();
                Ok(len)
            })
        },
    )?;
    linker.func_wrap(
        "durable",
        "host_read",
        |mut c: Caller<'_, Host>, ptr: i32| {
            let answer = std::mem::take(&mut c.data_mut().answer);
            caller_write(&mut c, ptr as u32 as usize, &answer)
        },
    )?;
    crate::wasi::link(linker)
}
