//! Opt-in in-memory simulation, fault injection and measurement support.
//! Enabled by the `simulation` Cargo feature; excluded from the default library.
mod fault;
mod memory;
pub use fault::{Effect, Event, Failpoint, FaultKv, Operation};
pub use memory::{MemoryKv, Stats, Traffic, object_kind};

/// Local engine traffic, distinct from durable backend traffic.
#[derive(Clone, Copy, Debug)]
pub struct IoStats {
    pub database_bytes_written: u64,
    pub private_wal_bytes_written: u64,
    pub database_len: u64,
    pub private_wal_len: u64,
}
