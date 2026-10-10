//! Write-ahead publication intent and idempotent reclamation, without key scanning.
//!
//! The intent is durable before any new object is written. It holds the exact
//! old/new heads and two complete ID lists: objects allocated by the attempt and
//! committed objects it retires. A save runs these phases, each fully
//! acknowledged before the next starts:
//!
//! 1. put intent;
//! 2. put new objects (concurrently);
//! 3. put head: the commit point;
//! 4. delete retired objects (concurrently);
//! 5. delete intent.
//!
//! Mutations inside a phase target distinct keys. After an error no further
//! mutation starts, and those already started are awaited before returning, so the
//! same key is never mutated by overlapping requests of one attempt.
//!
//! Open reads intent and head together. A head equal to the new head selects
//! commit; one equal to the old head (including absence) selects abort; anything
//! else fails before mutation. Recovery first makes the selected head durable,
//! because a failed head put may be visible without being durable, then deletes
//! the retired (commit) or created (abort) set and finally the intent. Deleting a
//! missing key succeeds, so every step is retryable. See `docs/PROTOCOL.md`.
use crate::{
    format::{HEAD, Id, Kind, PENDING, PENDING_MAGIC},
    kv::KvStore,
    snapshot::Head,
};
use futures::{StreamExt, stream::FuturesUnordered};
use std::{collections::BTreeSet, io};

/// Prototype limit: fail before any mutation rather than splitting the record.
/// The backend must accept values this large.
pub const MAX_PENDING_BYTES: usize = 1024 * 1024;
/// Upper bound on concurrent backend mutations within one save phase.
pub(crate) const MAX_MUTATIONS_IN_FLIGHT: usize = 16;
/// Fixed prefix: magic plus four u32 lengths.
const PREFIX: usize = 20;
/// One `(kind u8, ID u64 LE)` entry.
const ENTRY: usize = 9;

/// Sorted object identities; the order is the encoded order.
pub(crate) type Objects = BTreeSet<(Kind, Id)>;

/// `Ok` means publication was durably acknowledged. A cleanup error is
/// maintenance debt, never a reason to replay committed SQL; open retries it.
#[derive(Debug, Default)]
pub struct SaveReport {
    /// Objects scheduled for retirement, not a count of confirmed deletions.
    pub retired_objects: usize,
    /// Publication committed, but reclamation needs a retry on open.
    pub cleanup_error: Option<io::Error>,
}

/// One complete allocation/retirement plan, stored at `pending_save`.
///
/// Layout: `SP11`; u32 LE lengths of old head, new head, created and retired
/// lists; old and new head bytes; then created and retired `(kind u8, ID u64 LE)`
/// entries. Old-head length zero means no old head. Created IDs lie above the old
/// counter and at most the new one; retired IDs are nonzero and at most the old one.
/// Retired log segments (kind 2) lie between the old and new `log_start`.
pub(crate) struct PendingSave {
    pub old_head: Option<Vec<u8>>,
    pub new_head: Vec<u8>,
    /// Every object of the attempt, including ones a failed attempt never wrote.
    pub created: Objects,
    /// Committed objects unreachable from the new head.
    pub retired: Objects,
}

fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid pending-save record")
}

impl PendingSave {
    fn validate(&self) -> io::Result<()> {
        let old = match &self.old_head {
            Some(bytes) => Head::decode(bytes)?,
            None => Head::default(),
        };
        let new = Head::decode(&self.new_head)?;
        // Retired segments lie in the old log and before the new one, if any.
        let retired_log = |seq: u64| {
            old.log_start != 0
                && seq >= old.log_start
                && (new.log_start == 0 || seq < new.log_start)
        };
        let valid = self.old_head.as_ref() != Some(&self.new_head)
            && new.last_id >= old.last_id
            && self
                .created
                .iter()
                .all(|&(kind, id)| kind != Kind::Log && id > old.last_id && id <= new.last_id)
            && self.retired.iter().all(|&(kind, id)| match kind {
                Kind::Log => retired_log(id),
                _ => id != 0 && id <= old.last_id,
            });
        if valid { Ok(()) } else { Err(invalid()) }
    }

    /// Validate the plan and serialize it; no backend mutation happens here.
    pub(crate) fn encode(&self) -> io::Result<Vec<u8>> {
        self.validate()?;
        let old = self.old_head.as_deref().unwrap_or_default();
        let size = PREFIX
            + old.len()
            + self.new_head.len()
            + ENTRY * (self.created.len() + self.retired.len());
        if size > MAX_PENDING_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "pending-save record exceeds 1 MiB limit; use smaller saves",
            ));
        }
        let mut bytes = Vec::with_capacity(size);
        bytes.extend_from_slice(PENDING_MAGIC);
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
        for &(kind, id) in self.created.iter().chain(&self.retired) {
            bytes.push(kind as u8);
            bytes.extend_from_slice(&id.to_le_bytes());
        }
        Ok(bytes)
    }

    /// Validate lengths, ID ranges and strict list ordering before any cleanup.
    pub(crate) fn decode(bytes: &[u8]) -> io::Result<Self> {
        if bytes.len() < PREFIX || bytes.len() > MAX_PENDING_BYTES || &bytes[..4] != PENDING_MAGIC {
            return Err(invalid());
        }
        let [old_len, new_len, created_len, retired_len] = [4, 8, 12, 16]
            .map(|at| u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize);
        // u64 arithmetic cannot overflow for u32 inputs, even on 32-bit hosts.
        let expected = (PREFIX + old_len + new_len) as u64
            + ENTRY as u64 * (created_len as u64 + retired_len as u64);
        if expected != bytes.len() as u64 {
            return Err(invalid());
        }
        let (heads, entries) = bytes[PREFIX..].split_at(old_len + new_len);
        let (created, retired) = entries.split_at(ENTRY * created_len);
        let result = Self {
            old_head: (old_len != 0).then(|| heads[..old_len].to_vec()),
            new_head: heads[old_len..].to_vec(),
            created: decode_objects(created)?,
            retired: decode_objects(retired)?,
        };
        result.validate()?;
        Ok(result)
    }

    /// Delete the unneeded object set, then remove the intent last. On error the
    /// complete intent stays in place, so the same cleanup can be retried.
    pub(crate) async fn finish(&self, store: &dyn KvStore, committed: bool) -> io::Result<()> {
        let objects = if committed {
            &self.retired
        } else {
            &self.created
        };
        let deletes = objects
            .iter()
            .map(|&(kind, id)| Mutation::Delete(kind.key(id)))
            .collect();
        concurrently(store, deletes).await?;
        store.delete(PENDING).await
    }
}

fn decode_objects(bytes: &[u8]) -> io::Result<Objects> {
    let mut objects = Objects::new();
    for entry in bytes.as_chunks::<ENTRY>().0 {
        let kind = match entry[0] {
            0 => Kind::Block,
            1 => Kind::Node,
            2 => Kind::Log,
            _ => return Err(invalid()),
        };
        let object = (kind, u64::from_le_bytes(entry[1..].try_into().unwrap()));
        if objects.last().is_some_and(|last| *last >= object) {
            return Err(invalid());
        }
        objects.insert(object);
    }
    Ok(objects)
}

/// One backend mutation of a save phase.
pub(crate) enum Mutation {
    Put(String, Vec<u8>),
    Delete(String),
}

async fn apply(store: &dyn KvStore, mutation: Mutation) -> io::Result<()> {
    match mutation {
        Mutation::Put(key, value) => store.put(&key, value).await,
        Mutation::Delete(key) => store.delete(&key).await,
    }
}

/// Apply mutations of distinct keys, at most [`MAX_MUTATIONS_IN_FLIGHT`] at once.
/// After the first error no new mutation starts; those already running are
/// awaited, so nothing from this phase is outstanding when it returns.
pub(crate) async fn concurrently(store: &dyn KvStore, mutations: Vec<Mutation>) -> io::Result<()> {
    let mut queued = mutations.into_iter();
    let mut running = FuturesUnordered::new();
    let mut error = None;
    loop {
        while error.is_none() && running.len() < MAX_MUTATIONS_IN_FLIGHT {
            let Some(mutation) = queued.next() else { break };
            running.push(apply(store, mutation));
        }
        match running.next().await {
            Some(Ok(())) => {}
            Some(Err(e)) => {
                error.get_or_insert(e);
            }
            None => return error.map_or(Ok(()), Err),
        }
    }
}

/// Settle an interrupted save and return the committed head bytes.
///
/// Requires exclusive ownership, with every request of a previous owner settled.
/// On failure no session opens; retrying is safe.
pub(crate) async fn recover(store: &dyn KvStore) -> io::Result<Option<Vec<u8>>> {
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
    // Make the decision durable before deleting either set: the visible head may
    // come from a failed, not yet durable put. Absence is confirmed the same way.
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
