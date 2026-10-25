//! The requests a state makes: the head, the tail and the checkpoints'
//! manifests in Restate, the packs in a [`BlobStore`] (S3, or Restate too).
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
//! previous checkpoint that neither of its checkpoints names is deleted, and
//! so is the manifest of a checkpoint older than the previous one. Any
//! state may collect, an owner's or a reader's, and so may a zombie:
//! whatever head it holds, every pack a later head names is either named by
//! its head's checkpoints or newer than the previous one.
use crate::{
    blobs::{BlobStore, KvBlobs},
    format::{Checkpoint, Entry, HEAD_KEY, Head, Manifest, PACK_PREFIX, PackId, parse_tail_key},
    kv::KvStore,
};
use bytes::Bytes;
use futures::future::try_join_all;
use std::{collections::BTreeSet, io, ops::Range, sync::Arc};

/// The stores of a state: see the module documentation.
pub(crate) struct Tiered {
    pub restate: Arc<dyn KvStore>,
    packs: Arc<dyn BlobStore>,
    manifests: KvBlobs,
    /// Whether the packs are kept in Restate too, without `packs`.
    pub packs_in_restate: bool,
}

impl Tiered {
    pub fn new(restate: Arc<dyn KvStore>, packs: Option<Arc<dyn BlobStore>>) -> Self {
        Self {
            packs_in_restate: packs.is_none(),
            packs: packs.unwrap_or_else(|| Arc::new(KvBlobs::new(restate.clone()))),
            manifests: KvBlobs::manifests(restate.clone()),
            restate,
        }
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
        Manifest::decode(&self.encoded_manifest(checkpoint).await?)
    }

    /// The manifest of `checkpoint`, as stored.
    pub async fn encoded_manifest(&self, checkpoint: &Checkpoint) -> io::Result<Bytes> {
        let name = checkpoint.pack.name();
        let bytes = self
            .manifests
            .get_range(&name, 0..checkpoint.manifest)
            .await?;
        bytes.ok_or_else(|| {
            io::Error::other(format!(
                "manifest {name} is gone: the owner collected it since this state opened"
            ))
        })
    }

    /// Queue the manifest of the checkpoint whose pack is `pack`: like the
    /// tail entry, it is durable with the head that names it.
    pub async fn put_manifest(&self, pack: PackId, manifest: Bytes) -> io::Result<()> {
        self.manifests.put(&pack.name(), manifest).await
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

    /// The packs `head`'s checkpoints need: those their manifests name.
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
        // A checkpoint's own pack, if it put one, holds what its manifest
        // names.
        Ok(manifests.iter().flat_map(Manifest::packs).collect())
    }

    /// Delete the packs no checkpoint `head` names needs, and the manifests
    /// of checkpoints older than its previous one: see the module
    /// documentation. How many packs.
    pub async fn collect_garbage(&self, head: &Head) -> io::Result<usize> {
        let Some(previous) = head.previous else {
            return Ok(0);
        };
        // One listing of Restate's keys finds the manifests, and the packs
        // kept there; S3's, the packs kept there. A store that cannot list
        // keeps its garbage.
        let in_s3 = async {
            match self.packs_in_restate {
                true => Ok(Vec::new()),
                false => unlisted(self.packs.list(PACK_PREFIX).await),
            }
        };
        let (keys, in_s3) =
            futures::try_join!(async { unlisted(self.restate.keys().await) }, in_s3)?;
        let in_restate = KvBlobs::new(self.restate.clone());
        let packs = match self.packs_in_restate {
            true => in_restate.listed(&keys, PACK_PREFIX).into_iter().collect(),
            false => in_s3,
        };
        let older = |id: &PackId| id.version < previous.version();
        let manifests: BTreeSet<String> = (self.manifests.listed(&keys, "").into_iter())
            .filter(|name| PackId::parse_name(name).is_some_and(|id| older(&id)))
            .collect();
        let older: Vec<(PackId, String)> = (packs.into_iter())
            .filter_map(|key| Some((PackId::parse(&key)?, key)))
            .filter(|(id, _)| older(id))
            .collect();
        let garbage: Vec<String> = match older.is_empty() {
            true => Vec::new(),
            false => {
                let keep = self.needed(head).await?;
                (older.into_iter())
                    .filter(|(id, _)| !keep.contains(id))
                    .map(|(_, key)| key)
                    .collect()
            }
        };
        // Restate's deletes are queued, and stored by one sync.
        let mut queued = (self.manifests).queue_delete(&keys, |name| manifests.contains(name))?;
        // Parts of tail entries the checkpoint covers that the head does not
        // name: a crashed flush's, past the parts its retry put.
        for key in &keys {
            if let Some((version, part)) = parse_tail_key(key)
                && version <= head.checkpointed()
                && part >= head.parts_of(version)
            {
                self.restate.delete(key)?;
                queued = true;
            }
        }
        let delete_in_s3 = async {
            match self.packs_in_restate {
                true => Ok(()),
                false => self.packs.delete(&garbage).await,
            }
        };
        if self.packs_in_restate {
            let doomed: BTreeSet<&str> = garbage.iter().map(String::as_str).collect();
            queued |= in_restate.queue_delete(&keys, |key| doomed.contains(key))?;
        }
        let sync = async {
            match queued {
                true => self.restate.sync().await,
                false => Ok(()),
            }
        };
        futures::try_join!(sync, delete_in_s3)?;
        Ok(garbage.len())
    }
}

/// A listing; nothing, from a store that cannot list.
fn unlisted(listed: io::Result<Vec<String>>) -> io::Result<Vec<String>> {
    match listed {
        Err(error) if error.kind() == io::ErrorKind::Unsupported => Ok(Vec::new()),
        listed => listed,
    }
}
