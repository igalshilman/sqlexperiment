//! Host-driven QuickJS durability: Wasmtime, immutable golden images, and a KV
//! page map. Only one writer may own a run. See the crate README for the commit
//! protocol and the distinction between full scanning and dirty-page tracking.
//! [`Transaction`] couples application state, effects and the VM checkpoint.
//! Recovery restores the committed image; the host supplies effect completions.

mod codec;
mod engine;
mod host;
#[cfg(any(not(feature = "full-scan"), test))]
mod instrument;
mod kv;
mod model;
#[cfg(feature = "sqlite-kv-host")]
pub mod namespace;
mod program;
#[cfg(feature = "sqlite-host")]
pub mod sqlite;
pub mod tracking;
mod vm;

pub use engine::Engine;
pub use host::{Resolution, Transaction};
pub use kv::{FileKv, Kv, MemoryTransaction};
pub use model::{GuestError, Operation, Outcome, RpcTarget, State, Status, View};
pub use program::{Program, Programs};
pub use vm::Runtime;

#[cfg(test)]
#[path = "../tests/unit/codec.rs"]
mod codec_tests;

#[cfg(test)]
#[path = "../tests/unit/instrument.rs"]
mod instrument_tests;
