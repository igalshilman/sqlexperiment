//! Write-ahead publication intent and idempotent reclamation, without key scanning.
//!
//! The pending record is durable before any new object is written. It contains the
//! exact old/new heads and two complete ID lists: objects allocated by the attempt
//! and committed objects retired by it. Payload bytes are not duplicated in this
//! record. At most one intent exists in an exclusively owned namespace.
//!
//! # Ordering and commit
//!
//! Save awaits, in order: intent put; each new-object put; head put; each retired
//! object delete; intent delete. **One backend mutation is active at a time.** The
//! head put's durable acknowledgement commits the snapshot. An error stops the
//! phase immediately. Until the final intent deletion takes effect, the complete
//! record remains available for recovery, including IDs never written.
//!
//! # Recovery of uncertain outcomes
//!
//! Open reads intent and head concurrently while ownership is quiescent. A head
//! matching the new bytes selects commit; one matching the old bytes (including
//! absence) selects abort. Any other value fails before mutation. Recovery first
//! confirms its choice durably by putting the selected head, or deleting an absent
//! head. Only then does it delete retired or created objects and finally the intent.
//!
//! Confirmation matters when a failed head put was visible but not durable: deleting
//! objects based solely on that read could otherwise destroy the state a crash later
//! restores. Selecting a visible new head is safe because all its objects had already
//! acknowledged durability before the original head put began. Deleting a missing
//! object succeeds, so partial cleanup and aborted allocations are retryable.
//!
//! Cancellation still requires host settlement of remote work before recovery. This
//! protocol has no lock/fencing token and cannot prevent a late old head put from
//! arriving after a new owner starts.
use crate::{
    format::{HEAD, PENDING, PENDING_MAGIC, key},
    kv::KvStore,
    snapshot::Snapshot,
};
use std::{collections::BTreeSet, io, sync::Arc};

/// Prototype limit: fail before any mutation rather than silently splitting an
/// atomic record. A backend must accept values this large (pages use at most 4099 bytes).
pub const MAX_PENDING_BYTES: usize = 1024 * 1024;
/// Sorted `(kind, ID)` pairs; block and node identities are recorded explicitly.
pub(crate) type Objects = BTreeSet<(&'static str, u64)>;

/// Ok means publication was durably acknowledged. A cleanup error is maintenance
/// debt, never a reason to replay already committed SQL. Open retries cleanup.
#[derive(Debug, Default)]
pub struct SaveReport {
    /// Objects scheduled for retirement, not a count of confirmed deletions.
    pub retired_objects: usize,
    /// Publication committed, but reclamation needs retry on open. Do not replay SQL.
    pub cleanup_error: Option<io::Error>,
}

/// One complete allocation/retirement plan, encoded atomically at `pending_save`.
///
/// Layout: `SP10`; four u32 LE lengths/counts (old head, new head, created, retired);
/// exact old/new head bytes; then created and retired `(kind u8, ID u64 LE)` entries.
/// Old-head length zero denotes an absent old head. Lists are strictly sorted and
/// unique. Created IDs exceed the old counter and do not exceed the new counter;
/// retired IDs are nonzero and no greater than the old counter. The record is
/// capped at [`MAX_PENDING_BYTES`] and validated before the first mutation.
pub(crate) struct PendingSave {
    /// Exact previously committed head, or absence for first publication.
    pub old_head: Option<Vec<u8>>,
    /// Exact candidate head whose publication commits this attempt.
    pub new_head: Vec<u8>,
    /// Complete new-object set, including objects a failed attempt never uploaded.
    pub created: Objects,
    /// Committed objects made unreachable by the new root.
    pub retired: Objects,
}
fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid pending-save record")
}
impl PendingSave {
    fn validate(&self) -> io::Result<()> {
        let old_high = self
            .old_head
            .as_deref()
            .map(Snapshot::validate_head)
            .transpose()?
            .unwrap_or(0);
        let new_high = Snapshot::validate_head(&self.new_head)?;
        if self.old_head.as_ref() == Some(&self.new_head)
            || new_high < old_high
            || self.created.iter().any(|(kind, id)| {
                !matches!(*kind, "block" | "node") || *id <= old_high || *id > new_high
            })
            || self
                .retired
                .iter()
                .any(|(kind, id)| !matches!(*kind, "block" | "node") || *id == 0 || *id > old_high)
        {
            return Err(invalid());
        }
        Ok(())
    }
    /// Validate the full plan and serialize it without performing any backend mutation.
    pub(crate) fn encode(&self) -> io::Result<Vec<u8>> {
        self.validate()?;
        let old = self.old_head.as_deref().unwrap_or_default();
        let size = 20usize
            .checked_add(old.len())
            .and_then(|n| n.checked_add(self.new_head.len()))
            .and_then(|n| {
                (self.created.len() + self.retired.len())
                    .checked_mul(9)
                    .and_then(|m| n.checked_add(m))
            })
            .ok_or_else(invalid)?;
        if size > MAX_PENDING_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "pending-save record exceeds 1 MiB limit; use smaller saves",
            ));
        }
        let mut bytes = PENDING_MAGIC.to_vec();
        for n in [
            old.len(),
            self.new_head.len(),
            self.created.len(),
            self.retired.len(),
        ] {
            bytes.extend_from_slice(&(n as u32).to_le_bytes());
        }
        bytes.extend_from_slice(old);
        bytes.extend_from_slice(&self.new_head);
        for entries in [&self.created, &self.retired] {
            for (kind, id) in entries {
                bytes.push(if *kind == "block" { 0 } else { 1 });
                bytes.extend_from_slice(&id.to_le_bytes());
            }
        }
        Ok(bytes)
    }
    /// Validate lengths, ID ranges and canonical list ordering before permitting cleanup.
    pub(crate) fn decode(bytes: &[u8]) -> io::Result<Self> {
        if bytes.len() < 20 || bytes.len() > MAX_PENDING_BYTES || &bytes[..4] != PENDING_MAGIC {
            return Err(invalid());
        }
        let [old_len, new_len, created_len, retired_len] = [4, 8, 12, 16].map(|offset| {
            u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize
        });
        // Bounded input first; checked u64 arithmetic also works on 32-bit hosts.
        let expected =
            20u64 + old_len as u64 + new_len as u64 + 9 * (created_len as u64 + retired_len as u64);
        if expected != bytes.len() as u64 {
            return Err(invalid());
        }
        let mut offset = 20 + old_len + new_len;
        let mut read_entries = |count: usize| -> io::Result<Objects> {
            let mut entries = Objects::new();
            let mut previous = None;
            for entry in bytes[offset..offset + count * 9].as_chunks::<9>().0 {
                let kind = match entry[0] {
                    0 => "block",
                    1 => "node",
                    _ => return Err(invalid()),
                };
                let id = u64::from_le_bytes(entry[1..].try_into().unwrap());
                if previous.is_some_and(|p| p >= (kind, id)) {
                    return Err(invalid());
                }
                entries.insert((kind, id));
                previous = Some((kind, id));
            }
            offset += count * 9;
            Ok(entries)
        };
        let result = Self {
            old_head: (old_len != 0).then(|| bytes[20..20 + old_len].to_vec()),
            new_head: bytes[20 + old_len..20 + old_len + new_len].to_vec(),
            created: read_entries(created_len)?,
            retired: read_entries(retired_len)?,
        };
        result.validate()?;
        Ok(result)
    }
    /// Delete the unneeded object set serially, then remove the intent last.
    /// Stops on the first error so the same complete plan remains retryable.
    pub(crate) async fn finish(&self, store: Arc<dyn KvStore>, committed: bool) -> io::Result<()> {
        let entries = if committed {
            &self.retired
        } else {
            &self.created
        };
        for (kind, id) in entries {
            store.delete(&key(kind, id)).await?;
        }
        store.delete(PENDING).await
    }
}

/// Exclusive ownership, including settlement of prior cancelled requests, is
/// required. Failure leaves recovery retryable; no session opens until it ends.
pub(crate) async fn recover(store: Arc<dyn KvStore>) -> io::Result<Option<Vec<u8>>> {
    // Ownership is quiescent, so these independent reads may overlap. No
    // mutation is issued until both values have been read and validated.
    let (pending, head) = futures::try_join!(store.get(PENDING), store.get(HEAD))?;
    let Some(bytes) = pending else {
        return Ok(head);
    };
    let intent = PendingSave::decode(&bytes)?;
    let committed = if head.as_ref() == Some(&intent.new_head) {
        true
    } else if head == intent.old_head {
        false
    } else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "head does not match pending save",
        ));
    };
    // Persist our decision before deleting either set. In particular a failed
    // publication may be visible but not durable. Absence is confirmed too.
    match &head {
        Some(bytes) => store.put(HEAD, bytes.clone()).await?,
        None => store.delete(HEAD).await?,
    }
    intent.finish(store, committed).await?;
    Ok(head)
}

#[cfg(all(test, feature = "simulation"))]
#[path = "../tests/unit/pending.rs"]
mod tests;
