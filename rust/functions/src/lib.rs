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
//! - [`Bindings`]: the operations the host makes for programs,
//!   `ctx.call(name, input)`, such as [`Claude`]'s `claude.messages`.
//! - [`FunctionSignals`]: the `function_signals` object, which keeps the
//!   programs' waits for signals, `ctx.signal(name, input)`.
//! - [`Workspace`]: the `workspace` object, a directory and a shell for
//!   coding agents.
//!
//! The design: [docs/DESIGN.md](../docs/DESIGN.md).

mod bindings;
mod budget;
mod claude;
mod codec;
mod cow;
mod db;
mod heap;
mod instrument;
mod model;
mod program;
mod python;
mod quickjs;
pub mod restate;
mod run;
mod signals;
mod store;
mod tracking;
mod vm;
mod wasi;
mod workspace;

pub use bindings::{Binding, Bindings, HostCall};
pub use budget::Budget;
pub use claude::Claude;
pub use model::{GuestError, Operation, Outcome, RpcTarget, State, Status, View};
pub use program::{Program, Programs};
pub use restate::{bind, embedded, endpoint, faults, page_cache};
pub use run::Run;
pub use signals::{FunctionSignals, Resolved, SIGNALS, Signal, Waiting, signals_of};
pub use store::Scope;
pub use vm::{Language, Limits, Runtime};
pub use workspace::Workspace;

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
