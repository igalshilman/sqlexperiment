//! A single write-ahead intent covers allocation and retirement. No scans.
use crate::{
    format::{HEAD, PENDING, PENDING_MAGIC, key},
    kv::KvStore,
    snapshot::Snapshot,
};
use futures::{FutureExt, StreamExt, stream};
use std::{collections::BTreeSet, io, sync::Arc};

/// Prototype limit: fail before any mutation rather than silently splitting an
/// atomic record. A backend must accept values this large (pages use at most 4099 bytes).
pub const MAX_PENDING_BYTES: usize = 1024 * 1024;
pub(crate) type Objects = BTreeSet<(&'static str, u64)>;

/// Ok means publication was durably acknowledged. A cleanup error is maintenance
/// debt, never a reason to replay already committed SQL. Open retries cleanup.
#[derive(Debug, Default)]
pub struct SaveReport {
    pub retired_objects: usize,
    pub cleanup_error: Option<io::Error>,
}

pub(crate) struct PendingSave {
    pub old_head: Option<Vec<u8>>,
    pub new_head: Vec<u8>,
    pub created: Objects,
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
    pub(crate) async fn finish(
        &self,
        store: Arc<dyn KvStore>,
        committed: bool,
        concurrency: usize,
    ) -> io::Result<()> {
        let entries = if committed {
            &self.retired
        } else {
            &self.created
        };
        let jobs = entries.iter().copied().map(|(kind, id)| {
            let store = store.clone();
            async move { store.delete(&key(kind, &id)).await }.boxed()
        });
        let mut deletes = stream::iter(jobs).boxed().buffer_unordered(concurrency);
        let mut error = None;
        while let Some(result) = deletes.next().await {
            if let Err(e) = result {
                error.get_or_insert(e);
            }
        }
        if let Some(e) = error {
            return Err(e);
        }
        store.delete(PENDING).await
    }
}

/// Exclusive ownership, including settlement of prior cancelled requests, is
/// required. Failure leaves recovery retryable; no session opens until it ends.
pub(crate) async fn recover(
    store: Arc<dyn KvStore>,
    concurrency: usize,
) -> io::Result<Option<Vec<u8>>> {
    let pending = store.get(PENDING).await?;
    let head = store.get(HEAD).await?;
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
    intent.finish(store, committed, concurrency).await?;
    Ok(head)
}

#[cfg(all(test, feature = "simulation"))]
#[path = "../tests/unit/pending.rs"]
mod tests;
