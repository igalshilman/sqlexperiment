use crate::{Kv, Operation, Outcome};
use anyhow::Result;
use std::future::Future;

/// A transaction-local result, or an effect whose completion arrives later.
pub enum Resolution {
    Ready(Outcome),
    Pending,
}

/// Host-owned atomic transaction spanning checkpoint KV, application state and
/// durable effects. Reads observe staged writes. Nothing is externally visible
/// until the host completes the entire storage transaction. A pre-decision abort
/// discards staged changes; a durable commit decision must be finished.
///
/// `execute` handles new guest requests in issue order, outside an active Wasm
/// call. KV get/set/delete return `Ready` using transactional state; multiple JS
/// awaits then continue in one VM until the host's checkpoint policy stops it.
/// The callback
/// may await transactional I/O: the VM and transaction remain live, with no
/// active Wasm stack. Awaiting this future does not create a durable boundary.
/// A custom function or local RPC may also return a transaction-local `Ready` result.
///
/// Return `Pending` only after staging a durable effect with the request's ID.
/// RPCs and timers can suspend this way. An internal `Operation::Resume` from
/// the runtime's host scheduling policy must stage a ready local wakeup and
/// return `Pending`; it is not a guest API. Dispatch becomes
/// eligible only when this same transaction commits its VM snapshot. Do not
/// perform irreversible external work from inside this transaction.
///
/// The host finishes storage recovery before VM recovery, reads or effect dispatch.
/// It has its own explicit recovery/delivery path. `recover` restores the
/// snapshot without executing requests. The host reconnects pending IDs to its
/// effect state and calls `complete` when a result is available. `acknowledge`
/// stages consumption of that result in the same transaction as the new image.
/// No per-operation response journal, replay or read-result receipts are needed.
///
/// On an execution error or cancellation of an engine future, discard the whole
/// engine and staged transaction; never commit a partially executed turn.
/// After a durable commit decision, storage errors require roll-forward recovery.
/// If the commit outcome is unknown, resolve storage recovery before inspecting
/// committed state or delivering again. Storage redo must not execute guest calls.
/// Single-writer ownership and commit/abort are controlled by the embedding host.
pub trait Transaction: Kv {
    /// Scoped adapters reject a turn/recovery for a different invocation, even
    /// when that turn makes no host requests. Unscoped workspaces may allow it.
    fn check_run(&self, _run: &str) -> Result<()> {
        Ok(())
    }
    fn execute(&mut self, operation: &Operation) -> impl Future<Output = Result<Resolution>>;
    /// Verify that the selected outcome is the recoverable completion and that
    /// delivery is not before its availability time, then stage consumption.
    fn acknowledge(
        &mut self,
        operation_id: &str,
        outcome: &Outcome,
        now: u64,
    ) -> impl Future<Output = Result<()>>;
}
