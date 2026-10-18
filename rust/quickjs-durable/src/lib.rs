//! Durable JavaScript in Restate virtual objects. A program's QuickJS VM runs
//! under Wasmtime; at each wait its memory is saved, as the pages that changed
//! since the program's golden image, and a crash or a result restores it.
//! Nothing is replayed.
//!
//! - [`restate`]: each program as a virtual object; its `run` handler drives
//!   the runtime and waits on Restate for the run's calls and timers.
//! - [`Engine`], [`Runtime`], [`Programs`]: the runtime, which stores runs
//!   through a [`Transaction`] (in Restate, [`namespace`]: sqlite-kv in the
//!   object's state; in tests, [`MemoryTransaction`]).
//! - [`Claude`]: the `claude` service, for programs to call models.
//!
//! The design: [docs/DESIGN.md](../docs/DESIGN.md).

mod claude;
mod codec;
mod engine;
mod host;
#[cfg(any(not(feature = "full-scan"), test))]
mod instrument;
mod kv;
mod model;
pub mod namespace;
mod program;
pub mod restate;
pub mod tracking;
mod vm;

pub use claude::Claude;
pub use engine::Engine;
pub use host::{Resolution, Transaction};
pub use kv::{Kv, MemoryTransaction};
pub use model::{GuestError, Operation, Outcome, RpcTarget, State, Status, View};
pub use program::{Program, Programs};
pub use restate::{bind, embedded, endpoint, faults};
pub use vm::Runtime;

#[cfg(test)]
#[path = "../tests/unit/codec.rs"]
mod codec_tests;

#[cfg(test)]
#[path = "../tests/unit/instrument.rs"]
mod instrument_tests;
