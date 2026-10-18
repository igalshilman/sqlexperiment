//! Durable JavaScript in Restate virtual objects. A program's QuickJS VM runs
//! under Wasmtime; at each wait its memory is saved, as the pages that changed
//! since the program's golden image, and a crash or a result restores it.
//! Nothing is replayed.
//!
//! - [`restate`]: each program as a virtual object; its `run` handler drives
//!   the runtime and waits on Restate for the run's calls and timers.
//! - [`Run`], [`Runtime`], [`Programs`]: the runtime. A run keeps its VM live
//!   from turn to turn, and commits each turn to a [`Store`] (in Restate, the
//!   object's state; in tests, a [`MemoryStore`]).
//! - [`Claude`]: the `claude` service, for programs to call models.
//!
//! The design: [docs/DESIGN.md](../docs/DESIGN.md).

mod claude;
mod codec;
mod heap;
mod instrument;
mod model;
mod program;
pub mod restate;
mod run;
mod store;
mod tracking;
mod vm;

pub use claude::Claude;
pub use model::{GuestError, Operation, Outcome, RpcTarget, State, Status, View};
pub use program::{Program, Programs};
pub use restate::{bind, embedded, endpoint, faults};
pub use run::Run;
pub use store::{MemoryStore, Store};
pub use vm::Runtime;

#[cfg(test)]
#[path = "../tests/unit/codec.rs"]
mod codec_tests;

#[cfg(test)]
#[path = "../tests/unit/heap.rs"]
mod heap_tests;

#[cfg(test)]
#[path = "../tests/unit/tracking.rs"]
mod tracking_tests;

#[cfg(test)]
#[path = "../tests/unit/instrument.rs"]
mod instrument_tests;
