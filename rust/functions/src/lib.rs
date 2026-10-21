//! Durable JavaScript in Restate virtual objects. A program's QuickJS VM runs
//! under Wasmtime; at each wait its memory is saved, as the pages that changed
//! since the program's golden image, and a crash or a result restores it.
//! Nothing is replayed.
//!
//! - [`restate`]: each program as a virtual object; its `run` handler drives
//!   the runtime and waits on Restate for the run's calls and timers.
//! - [`Run`], [`Runtime`], [`Programs`]: the runtime. A run keeps its VM live
//!   from turn to turn, and commits each turn to its [`Scope`]'s database (in
//!   Restate, kept in the object's state).
//! - [`Extensions`]: what programs use beside their memory and their
//!   scope's database, under one contract with the run's commit: calls in
//!   the turn, operations after it, and state saved with it. [`Llm`] (the
//!   model gateway) and [`Files`] (a checkpointed filesystem) are; so is
//!   each [`Binding`], an operation alone, such as [`Fetch`].
//! - [`FunctionSignals`]: the `function_signals` object, which keeps the
//!   programs' waits for signals, `ctx.signal(name, input)`.
//! - [`Workspace`]: the `workspace` object, a directory and a shell for
//!   coding agents.
//!
//! The design: [docs/DESIGN.md](../docs/DESIGN.md).

mod bindings;
mod budget;
mod cow;
mod db;
mod extension;
mod fetch;
mod files;
mod heap;
mod instrument;
mod llm;
mod model;
mod program;
mod python;
mod quickjs;
pub mod restate;
mod run;
mod signals;
mod store;
mod tracking;
mod transaction;
mod vm;
mod wasi;
mod workspace;

pub use bindings::Binding;
pub use budget::Budget;
pub use extension::{Attached, Extension, Extensions, HostCall, Making, Tables};
pub use fetch::{Fetch, FetchPolicy};
pub use files::Files;
pub use llm::{Llm, Provider};
pub use model::{GuestError, Operation, Outcome, RpcTarget, State, Status, View};
pub use program::{Program, Programs};
pub use restate::{bind, embedded, endpoint, faults, page_cache};
pub use run::Run;
pub use signals::{FunctionSignals, Resolved, SIGNALS, Signal, Waiting, signals_of};
pub use store::Scope;
pub use transaction::{Blobs, DirBlobs, Journal, MemoryBlobs, Participant, Record, ScopeBlobs};
pub use vm::{Language, Limits, Runtime};
pub use workspace::Workspace;

#[cfg(test)]
#[path = "../tests/unit/heap.rs"]
mod heap_tests;

#[cfg(test)]
#[path = "../tests/unit/tracking.rs"]
mod tracking_tests;

#[cfg(test)]
#[path = "../tests/unit/instrument.rs"]
mod instrument_tests;
