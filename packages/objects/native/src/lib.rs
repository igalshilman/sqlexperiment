//! The Node.js addon: SQLite-compatible SQL over a key/value store the host
//! implements in JavaScript, such as a Restate object's state.
//!
//! Three modules, each on the one before; only the addon is public:
//!
//! - [`state`]: one paged file and the effects over the key/value store,
//!   changed through a mutation that a flush makes durable at once
//!   (`src/state/README.md`).
//! - [`sql`]: the [`sql::Database`], SQLite-compatible SQL in the state's
//!   file, each commit flushed with the effects (`src/sql/README.md`).
//! - `node`: the napi bindings, around [`sql::Database`].
//!
//! With the feature `bench`, `state` and `sql` are public instead, for the
//! benchmarks, and the addon is left out.

#[cfg(not(any(test, feature = "bench")))]
mod node;

#[cfg(feature = "bench")]
#[doc(hidden)]
pub mod sql;
#[cfg(not(feature = "bench"))]
#[allow(dead_code, unused_imports)]
mod sql;

#[cfg(feature = "bench")]
#[doc(hidden)]
pub mod state;
#[cfg(not(feature = "bench"))]
#[allow(dead_code, unused_imports)]
mod state;
