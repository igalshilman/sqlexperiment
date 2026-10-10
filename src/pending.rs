//! A single write-ahead intent covers allocation and retirement. No scans.
use crate::{
    kv::KvStore,
    snapshot::{HEAD, Snapshot, key},
};
use futures::{StreamExt, future::BoxFuture, stream};
use std::{collections::BTreeSet, io, sync::Arc};

pub(crate) const PENDING: &str = "v7/pending_save";
/// Prototype limit: fail before any mutation rather than silently splitting an
/// atomic record. A backend must accept values this large (plus 4 KiB pages).
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
        let mut bytes = b"TKP7".to_vec();
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
        if bytes.len() < 20 || bytes.len() > MAX_PENDING_BYTES || &bytes[..4] != b"TKP7" {
            return Err(invalid());
        }
        let lengths: Vec<_> = bytes[4..20]
            .as_chunks::<4>()
            .0
            .iter()
            .map(|n| u32::from_le_bytes(*n) as usize)
            .collect();
        let [old_len, new_len, created_len, retired_len] = lengths[..] else {
            unreachable!()
        };
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
        let jobs: Vec<BoxFuture<'static, io::Result<()>>> = entries
            .iter()
            .copied()
            .map(|(kind, id)| {
                let store = store.clone();
                Box::pin(async move { store.delete(&key(kind, &id)).await })
                    as BoxFuture<'static, io::Result<()>>
            })
            .collect();
        let mut deletes = stream::iter(jobs).buffer_unordered(concurrency);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kv::MemoryKv;

    fn head(high: u64) -> Vec<u8> {
        let mut h = b"TKV7".to_vec();
        h.extend(0u64.to_le_bytes());
        h.extend(high.to_le_bytes());
        h.extend([0, 0]);
        h.extend(8u16.to_le_bytes());
        h.extend(0u16.to_le_bytes());
        h.extend(0u64.to_le_bytes());
        h
    }
    #[test]
    fn malformed_intents_and_unexpected_heads_fail_before_mutation() -> io::Result<()> {
        futures::executor::block_on(async {
            let intent = PendingSave {
                old_head: Some(head(10)),
                new_head: head(12),
                created: Objects::from([("block", 11), ("node", 12)]),
                retired: Objects::from([("block", 2)]),
            };
            let valid = intent.encode()?;
            let mut bad: Vec<_> = (0..valid.len()).map(|n| valid[..n].to_vec()).collect();
            let mut b = valid.clone();
            b[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
            bad.push(b);
            let mut b = valid.clone();
            b[20 + 68] = 9;
            bad.push(b); // invalid object kind
            let mut b = valid.clone();
            b[20 + 68 + 1..20 + 68 + 9].copy_from_slice(&1u64.to_le_bytes());
            bad.push(b); // created ID in committed range
            let mut b = valid.clone();
            let n = b.len();
            b[n - 8..].copy_from_slice(&11u64.to_le_bytes());
            bad.push(b); // retire new ID
            for bytes in bad {
                let store = Arc::new(MemoryKv::default());
                store.put(HEAD, head(10)).await?;
                store.put(PENDING, bytes).await?;
                store.reset_stats();
                assert!(recover(store.clone(), 8).await.is_err());
                assert_eq!(store.stats().puts + store.stats().deletes, 0);
            }
            let store = Arc::new(MemoryKv::default());
            store.put(HEAD, head(99)).await?;
            store.put(PENDING, valid).await?;
            store.reset_stats();
            assert!(
                recover(store.clone(), 8)
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("does not match")
            );
            assert_eq!(store.stats().puts + store.stats().deletes, 0);
            Ok(())
        })
    }
    #[test]
    fn large_intent_has_an_explicit_limit() {
        let intent = PendingSave {
            old_head: None,
            new_head: head(120000),
            created: (1..=120000).map(|id| ("block", id)).collect(),
            retired: Objects::new(),
        };
        assert!(intent.encode().unwrap_err().to_string().contains("1 MiB"));
    }
}
