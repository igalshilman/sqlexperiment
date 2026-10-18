//! What the integration tests share: one runtime, and catalogs compiled once.
use quickjs_durable::{Programs, Runtime};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, OnceLock},
};

/// The runtime every catalog of a test binary is on: compiling it is slow.
pub fn runtime() -> Arc<Runtime> {
    static RUNTIME: OnceLock<Arc<Runtime>> = OnceLock::new();
    RUNTIME
        .get_or_init(|| Arc::new(Runtime::new().expect("compile QuickJS")))
        .clone()
}

/// A catalog with `source` as program "v1", compiled once per source.
pub fn programs(source: &str) -> Arc<Programs> {
    static PROGRAMS: OnceLock<Mutex<HashMap<String, Arc<Programs>>>> = OnceLock::new();
    let mut catalogs = PROGRAMS.get_or_init(Default::default).lock().unwrap();
    catalogs
        .entry(source.into())
        .or_insert_with(|| {
            Arc::new(
                Programs::new(runtime())
                    .compile("v1", source)
                    .expect("compile v1"),
            )
        })
        .clone()
}
