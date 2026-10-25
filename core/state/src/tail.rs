//! The requests a state makes: the head and the tail in Restate, the packs
//! in a [`BlobStore`] (S3, or Restate too).
//!
//! **A flush** puts its tail entry and the head, then syncs: one round trip,
//! which Restate fences. A flush that adopts a checkpoint deletes, in the
//! same sync, the tail entries that the checkpoint before covered (those
//! the new one covers stay, for readers that opened before it).
//!
//! **Zombies.** A pack's name is unique (its version and a random nonce), so
//! an attempt that Restate fenced may still put packs, but never one that a
//! head names, and never over another's: they are garbage, as are a crashed
//! attempt's.
//!
//! **Garbage** ([`Tiered::collect_garbage`]): a pack older than the head's
//! previous checkpoint that neither of its checkpoints names is deleted. Any
//! state may collect, an owner's or a reader's, and so may a zombie:
//! whatever head it holds, every pack a later head names is either named by
//! its head's checkpoints or newer than the previous one.
use crate::{
    blobs::BlobStore,
    format::{Checkpoint, Entry, HEAD_KEY, Head, Manifest, PACK_PREFIX, PackId},
    kv::KvStore,
};
use bytes::Bytes;
use futures::{StreamExt, TryStreamExt, future::try_join_all, stream};
use std::{collections::BTreeSet, io, ops::Range, sync::Arc};

/// Requests in flight to the pack store.
const PARALLEL: usize = 32;

/// The stores of a state: see the module documentation.
pub(crate) struct Tiered {
    pub restate: Arc<dyn KvStore>,
    packs: Arc<dyn BlobStore>,
}

impl Tiered {
    pub fn new(restate: Arc<dyn KvStore>, packs: Arc<dyn BlobStore>) -> Self {
        Self { restate, packs }
    }

    pub async fn head(&self) -> io::Result<Option<Bytes>> {
        self.restate.get(HEAD_KEY).await
    }

    /// The tail entries of `head` from `first` on, fetched together.
    pub async fn tail(&self, head: &Head, first: u64) -> io::Result<Vec<Entry>> {
        let first = first.max(head.tail_first);
        let gets = (first..=head.version).map(|version| async move {
            let keys = head.entry_keys(version);
            let parts = try_join_all(keys.into_iter().map(|key| async move {
                (self.restate.get(&key).await?)
                    .ok_or_else(|| io::Error::other(format!("{key} is gone")))
            }))
            .await?;
            Entry::decode(&parts)
        });
        try_join_all(gets).await
    }

    /// Bytes `range` of `pack`.
    pub async fn get(&self, pack: PackId, range: Range<u64>) -> io::Result<Bytes> {
        let key = pack.key();
        (self.packs.get_range(&key, range).await?).ok_or_else(|| {
            io::Error::other(format!(
                "{key} is gone: the owner collected it since this state opened"
            ))
        })
    }

    pub async fn manifest(&self, checkpoint: &Checkpoint) -> io::Result<Manifest> {
        Manifest::decode(&self.get(checkpoint.pack, checkpoint.range()).await?)
    }

    pub async fn put_pack(&self, pack: PackId, body: Bytes) -> io::Result<()> {
        self.packs.put(&pack.key(), body).await
    }

    /// Put a flush's tail entry (`parts`) and its head, and delete the tail
    /// entries `deletes`; then sync.
    pub async fn write(
        &self,
        head: &Head,
        parts: Vec<Bytes>,
        deletes: &[String],
    ) -> io::Result<()> {
        for (key, part) in head.entry_keys(head.version).iter().zip(parts) {
            self.restate.put(key, part)?;
        }
        for key in deletes {
            self.restate.delete(key)?;
        }
        self.restate.put(HEAD_KEY, head.encode())?;
        self.restate.sync().await
    }

    /// The packs `head`'s checkpoints need.
    pub async fn needed(&self, head: &Head) -> io::Result<BTreeSet<PackId>> {
        let checkpoints: Vec<Checkpoint> = [head.checkpoint, head.previous]
            .into_iter()
            .flatten()
            .collect();
        let manifests = try_join_all(
            checkpoints
                .iter()
                .copied()
                .map(|c| async move { self.manifest(&c).await }),
        )
        .await?;
        let mut needed: BTreeSet<PackId> = checkpoints.iter().map(|c| c.pack).collect();
        for manifest in manifests {
            needed.extend(manifest.packs());
        }
        Ok(needed)
    }

    /// Delete the packs no checkpoint `head` names needs: see the module
    /// documentation. How many.
    pub async fn collect_garbage(&self, head: &Head) -> io::Result<usize> {
        let Some(previous) = head.previous else {
            return Ok(0);
        };
        let keep = self.needed(head).await?;
        let listed = self.packs.list(PACK_PREFIX).await?;
        let garbage: Vec<String> = (listed.into_iter())
            .filter(|key| {
                PackId::parse(key)
                    .is_some_and(|id| id.version < previous.version() && !keep.contains(&id))
            })
            .collect();
        let deleted = garbage.len();
        stream::iter(
            garbage
                .into_iter()
                .map(|key| async move { self.packs.delete(&key).await }),
        )
        .buffer_unordered(PARALLEL)
        .try_collect::<()>()
        .await?;
        Ok(deleted)
    }
}
