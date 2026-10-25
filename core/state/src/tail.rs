//! Where the stored state lives: a head and a tail in Restate, checkpoints
//! in packs (in S3, or in Restate too).
//!
//! | Store   | Key                            | Holds                                                                  |
//! | ------- | ------------------------------ | ---------------------------------------------------------------------- |
//! | Restate | `v18/head`                     | The commit point: the root, the checkpoint it builds on and the one before, the tail's extent |
//! | Restate | `v18/tail/<version>/<part>`    | The blobs the flush of `version` wrote, in parts of up to 1 MiB         |
//! | Packs   | `checkpoint/<version>-<nonce>` | A checkpoint: the blobs it moved out of the tail, then its manifest     |
//!
//! [`Tiered`] is the [`KvStore`] the [`crate::store`] reads and flushes
//! through, unchanged: it keeps the root in the head and a flush's blobs in
//! the flush's tail entry, both put, then synced, in one round trip. A blob
//! is read from the tail (all of it fetched at once, the first time one is
//! missed), or from the pack that the checkpoint's manifest says holds it.
//!
//! **Checkpoints.** Now and then ([`Tiered::due`]), the owner moves the
//! blobs that the durable root names and that the checkpoint does not hold
//! into one new pack, with a manifest: where each blob the root names is,
//! in this pack or in older ones. The next flush's head names it, and drops
//! the tail entries the checkpoint before covered (the one just covered
//! stay, for readers that opened before). The rule that keeps lookups
//! simple: blob numbers only grow, so a blob below the checkpoint's next
//! blob is in its manifest, and any other in the tail.
//!
//! **Zombies.** Restate fences the head and the tail. A pack's name is
//! unique (its version and a random nonce), so an attempt that Restate
//! fenced may still put packs, but never one that a head names: they are
//! garbage, as are a crashed attempt's.
//!
//! **Garbage** ([`Tiered::collect_garbage`]): a pack older than the
//! previous checkpoint that neither it nor the current one names is
//! deleted. Any state may collect, an owner's or a reader's, and so may a
//! zombie: whatever head it holds, every pack a later head names is either
//! named by its head's checkpoint or newer than it.
use crate::{
    blobs::BlobStore,
    cache,
    kv::KvStore,
    store::{ROOT_KEY, Reading, Snapshot, invalid, parse_blob_key, put_varint},
};
use bytes::Bytes;
use futures::{StreamExt, TryStreamExt, future::BoxFuture, lock::Mutex as AsyncMutex, stream};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    io,
    ops::Range,
    sync::{Arc, Mutex},
};

/// The head's key: the commit point. An open reads it first, so a host whose
/// store can send keys ahead of the first read (Restate's eager state)
/// should send this one.
pub const HEAD_KEY: &str = "v18/head";
pub(crate) const TAIL_PREFIX: &str = "v18/tail/";
/// Where packs are, in their store.
pub(crate) const PACK_PREFIX: &str = "checkpoint/";
const HEAD_MAGIC: &[u8; 4] = b"SH18";
const MANIFEST_MAGIC: &[u8; 4] = b"SM18";
/// Bytes of a tail entry's part, unless one blob is larger.
const PART_BYTES: usize = 1 << 20;
/// Requests in flight to the pack store.
const PARALLEL: usize = 32;

pub(crate) fn tail_key(version: u64, part: u32) -> String {
    format!("{TAIL_PREFIX}{version}/{part}")
}

/// A pack's name: the version it checkpoints, and a random nonce.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct PackId {
    pub version: u64,
    pub nonce: u64,
}

impl PackId {
    pub fn key(self) -> String {
        format!("{PACK_PREFIX}{:016x}-{:016x}", self.version, self.nonce)
    }

    pub fn parse(key: &str) -> Option<Self> {
        let (version, nonce) = key.strip_prefix(PACK_PREFIX)?.split_once('-')?;
        Some(Self {
            version: u64::from_str_radix(version, 16).ok()?,
            nonce: u64::from_str_radix(nonce, 16).ok()?,
        })
    }
}

/// A checkpoint, as a head names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Checkpoint {
    pub pack: PackId,
    /// The root's next blob at the checkpoint: every blob below it that a
    /// later root names is in the manifest.
    pub next_blob: u64,
    /// Where the manifest is in the pack.
    pub manifest: (u64, u64),
}

impl Checkpoint {
    fn encode(this: Option<&Self>, out: &mut Vec<u8>) {
        let Some(this) = this else {
            return out.push(0);
        };
        out.push(1);
        for n in [
            this.pack.version,
            this.pack.nonce,
            this.next_blob,
            this.manifest.0,
            this.manifest.1,
        ] {
            put_varint(out, n);
        }
    }

    fn decode(r: &mut Reading) -> io::Result<Option<Self>> {
        match r.varint()? {
            0 => Ok(None),
            1 => Ok(Some(Self {
                pack: PackId {
                    version: r.varint()?,
                    nonce: r.varint()?,
                },
                next_blob: r.varint()?,
                manifest: (r.varint()?, r.varint()?),
            })),
            _ => Err(r.invalid()),
        }
    }

    fn range(&self) -> Range<u64> {
        self.manifest.0..self.manifest.0 + self.manifest.1
    }
}

/// The commit point.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Head {
    /// The root's version.
    pub version: u64,
    pub root: Bytes,
    pub checkpoint: Option<Checkpoint>,
    /// The checkpoint before, which readers that opened on it may still read.
    pub previous: Option<Checkpoint>,
    /// The first version whose tail entry is kept.
    pub tail_first: u64,
    /// The parts of each tail entry kept, from `tail_first` to `version`.
    pub parts: Vec<u32>,
    /// Bytes of the tail entries since the checkpoint, about.
    pub tail_bytes: u64,
}

impl Default for Head {
    fn default() -> Self {
        Self {
            version: 0,
            root: Bytes::new(),
            checkpoint: None,
            previous: None,
            tail_first: 1,
            parts: Vec::new(),
            tail_bytes: 0,
        }
    }
}

impl Head {
    pub fn encode(&self) -> Bytes {
        let mut out = HEAD_MAGIC.to_vec();
        put_varint(&mut out, self.version);
        put_varint(&mut out, self.root.len() as u64);
        out.extend_from_slice(&self.root);
        Checkpoint::encode(self.checkpoint.as_ref(), &mut out);
        Checkpoint::encode(self.previous.as_ref(), &mut out);
        put_varint(&mut out, self.tail_first);
        put_varint(&mut out, self.tail_bytes);
        for &parts in &self.parts {
            put_varint(&mut out, parts.into());
        }
        out.into()
    }

    pub fn decode(bytes: &Bytes) -> io::Result<Self> {
        let body = bytes
            .strip_prefix(HEAD_MAGIC)
            .ok_or_else(|| invalid("head"))?;
        let mut r = Reading::new(body, "head");
        let version = r.varint()?;
        let root = r.bytes()?;
        let root = bytes.slice_ref(root);
        let checkpoint = Checkpoint::decode(&mut r)?;
        let previous = Checkpoint::decode(&mut r)?;
        let tail_first = r.varint()?;
        let tail_bytes = r.varint()?;
        let count = (version + 1)
            .checked_sub(tail_first)
            .filter(|&count| count <= body.len() as u64)
            .ok_or_else(|| r.invalid())?;
        let parts = (0..count)
            .map(|_| u32::try_from(r.varint()?).map_err(|_| r.invalid()))
            .collect::<io::Result<_>>()?;
        r.end()?;
        let covered = |c: &Option<Checkpoint>| c.is_some_and(|c| c.pack.version > version);
        if tail_first == 0 || covered(&checkpoint) || covered(&previous) || root.is_empty() {
            return Err(invalid("head"));
        }
        Ok(Self {
            version,
            root,
            checkpoint,
            previous,
            tail_first,
            parts,
            tail_bytes,
        })
    }

    /// The parts of version `version`'s tail entry.
    fn parts_of(&self, version: u64) -> u32 {
        (version.checked_sub(self.tail_first))
            .and_then(|at| self.parts.get(at as usize))
            .copied()
            .unwrap_or(0)
    }
}

/// Where a blob is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Location {
    pub pack: PackId,
    pub offset: u64,
    pub len: u64,
}

/// Where each blob that a checkpoint's root names is, and the bytes of
/// blobs of each pack that holds one.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Manifest {
    pub blobs: BTreeMap<u64, Location>,
    pub sizes: BTreeMap<PackId, u64>,
}

impl Manifest {
    pub fn packs(&self) -> BTreeSet<PackId> {
        self.blobs.values().map(|at| at.pack).collect()
    }

    pub fn encode(&self) -> Bytes {
        let packs: Vec<PackId> = self.packs().into_iter().collect();
        let index: HashMap<PackId, usize> =
            packs.iter().enumerate().map(|(i, &p)| (p, i)).collect();
        let mut out = MANIFEST_MAGIC.to_vec();
        put_varint(&mut out, packs.len() as u64);
        for pack in &packs {
            put_varint(&mut out, pack.version);
            put_varint(&mut out, pack.nonce);
            put_varint(&mut out, self.sizes.get(pack).copied().unwrap_or(0));
        }
        put_varint(&mut out, self.blobs.len() as u64);
        let mut previous = 0;
        for (&n, at) in &self.blobs {
            put_varint(&mut out, n - previous);
            previous = n;
            put_varint(&mut out, index[&at.pack] as u64);
            put_varint(&mut out, at.offset);
            put_varint(&mut out, at.len);
        }
        out.into()
    }

    pub fn decode(bytes: &[u8]) -> io::Result<Self> {
        let body = bytes
            .strip_prefix(MANIFEST_MAGIC)
            .ok_or_else(|| invalid("manifest"))?;
        let mut r = Reading::new(body, "manifest");
        let bound = body.len() as u64;
        let count = Some(r.varint()?)
            .filter(|&n| n <= bound)
            .ok_or_else(|| r.invalid())?;
        let mut sizes = BTreeMap::new();
        let packs = (0..count)
            .map(|_| {
                let pack = PackId {
                    version: r.varint()?,
                    nonce: r.varint()?,
                };
                sizes.insert(pack, r.varint()?);
                Ok(pack)
            })
            .collect::<io::Result<Vec<_>>>()?;
        let count = Some(r.varint()?)
            .filter(|&n| n <= bound)
            .ok_or_else(|| r.invalid())?;
        let mut blobs = BTreeMap::new();
        let mut n = 0u64;
        for i in 0..count {
            let delta = r.varint()?;
            n = n
                .checked_add(delta)
                .filter(|_| i == 0 || delta > 0)
                .ok_or_else(|| r.invalid())?;
            let pack = *packs.get(r.varint()? as usize).ok_or_else(|| r.invalid())?;
            let (offset, len) = (r.varint()?, r.varint()?);
            offset.checked_add(len).ok_or_else(|| r.invalid())?;
            blobs.insert(n, Location { pack, offset, len });
        }
        r.end()?;
        Ok(Self { blobs, sizes })
    }
}

/// A tail entry's blobs, in parts of about [`PART_BYTES`].
fn encode_parts(blobs: &[(u64, Bytes)]) -> Vec<Bytes> {
    let mut parts = Vec::new();
    let mut part = Vec::new();
    let mut count = 0u64;
    let mut body = Vec::new();
    let mut close = |count: &mut u64, body: &mut Vec<u8>, part: &mut Vec<u8>| {
        put_varint(part, *count);
        part.append(body);
        parts.push(Bytes::from(std::mem::take(part)));
        *count = 0;
    };
    for (n, blob) in blobs {
        if count > 0 && body.len() + blob.len() > PART_BYTES {
            close(&mut count, &mut body, &mut part);
        }
        put_varint(&mut body, *n);
        put_varint(&mut body, blob.len() as u64);
        body.extend_from_slice(blob);
        count += 1;
    }
    if count > 0 {
        close(&mut count, &mut body, &mut part);
    }
    parts
}

/// The version and part a tail entry's key names.
pub(crate) fn parse_tail_key(key: &str) -> Option<(u64, u32)> {
    let (version, part) = key.strip_prefix(TAIL_PREFIX)?.split_once('/')?;
    Some((version.parse().ok()?, part.parse().ok()?))
}

/// The blobs of a tail entry's part, into `into`.
pub(crate) fn read_part(bytes: &Bytes, into: &mut HashMap<u64, Bytes>) -> io::Result<()> {
    let mut r = Reading::new(bytes, "tail entry");
    let count = r.varint()?;
    for _ in 0..count {
        let n = r.varint()?;
        let blob = r.bytes()?;
        into.insert(n, bytes.slice_ref(blob));
    }
    r.end()
}

/// What a checkpoint did ([`crate::FlushReport::checkpoint`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Checkpointed {
    /// The version it holds the state of.
    pub version: u64,
    /// Blobs it moved out of the tail, into its pack.
    pub blobs: usize,
    /// Bytes of its pack, the manifest included.
    pub pack_bytes: usize,
    /// Bytes of its manifest.
    pub manifest_bytes: usize,
    /// Packs mostly dead, whose live blobs it moved into its own.
    pub compacted: usize,
    /// Packs its garbage collection deleted ([`crate::State::collect_garbage`]).
    pub deleted: usize,
}

/// A flush's writes, until its sync.
#[derive(Default)]
struct Staged {
    blobs: Vec<(u64, Bytes)>,
    root: Option<Bytes>,
    /// A checkpoint the next head names, its pack put.
    checkpoint: Option<(Checkpoint, Arc<Manifest>, Checkpointed)>,
}

#[derive(Default)]
struct Inner {
    head: Head,
    /// Blobs of the tail entries, by number: those this state wrote, and,
    /// once `loaded`, all the head keeps.
    tail: HashMap<u64, Bytes>,
    loaded: bool,
    /// The manifests of the head's checkpoints, as read.
    manifests: HashMap<PackId, Arc<Manifest>>,
    staged: Staged,
    /// What the last flush's checkpoint did, if its head named one.
    adopted: Option<Checkpointed>,
}

/// The store a state reads and flushes through: see the module
/// documentation.
pub(crate) struct Tiered {
    restate: Arc<dyn KvStore>,
    packs: Arc<dyn BlobStore>,
    inner: Mutex<Inner>,
    /// One fetch of the tail at a time.
    loading: AsyncMutex<()>,
}

impl Tiered {
    pub fn new(restate: Arc<dyn KvStore>, packs: Arc<dyn BlobStore>) -> Self {
        Self {
            restate,
            packs,
            inner: Mutex::default(),
            loading: AsyncMutex::new(()),
        }
    }

    fn inner(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap()
    }

    pub fn head(&self) -> Head {
        self.inner().head.clone()
    }

    /// Whether the tail is long enough to checkpoint, and none is staged.
    pub fn due(&self, entries: u64, bytes: u64) -> bool {
        let inner = self.inner();
        let head = &inner.head;
        let since = head.version - head.checkpoint.map_or(0, |c| c.pack.version);
        inner.staged.checkpoint.is_none()
            && head.version > 0
            && ((entries > 0 && since >= entries) || (bytes > 0 && head.tail_bytes >= bytes))
    }

    /// Read the head.
    async fn read_head(&self) -> io::Result<Option<Bytes>> {
        let bytes = self.restate.get(HEAD_KEY).await?;
        let head = bytes
            .as_ref()
            .map(Head::decode)
            .transpose()?
            .unwrap_or_default();
        let root = (head.version > 0).then(|| head.root.clone());
        let mut inner = self.inner();
        if inner.head != head {
            *inner = Inner {
                head,
                ..Inner::default()
            };
        }
        Ok(root)
    }

    /// Every blob of the tail entries since the head's checkpoint, into
    /// memory. (Those it covered, kept for readers that opened before it,
    /// hold no blob the head's root names but the checkpoint does not.)
    async fn load_tail(&self) -> io::Result<()> {
        let _loading = self.loading.lock().await;
        let head = {
            let inner = self.inner();
            if inner.loaded {
                return Ok(());
            }
            inner.head.clone()
        };
        let first = head.checkpoint.map_or(1, |c| c.pack.version + 1);
        let keys: Vec<String> = (first.max(head.tail_first)..=head.version)
            .flat_map(|version| (0..head.parts_of(version)).map(move |p| tail_key(version, p)))
            .collect();
        let parts =
            futures::future::try_join_all(keys.iter().map(|key| self.restate.get(key))).await?;
        let mut tail = HashMap::new();
        for (key, part) in keys.iter().zip(parts) {
            let part = part.ok_or_else(|| io::Error::other(format!("{key} is gone")))?;
            read_part(&part, &mut tail)?;
        }
        let mut inner = self.inner();
        if inner.head == head {
            for (n, blob) in tail {
                inner.tail.entry(n).or_insert(blob);
            }
            inner.loaded = true;
        }
        Ok(())
    }

    /// The manifest of `checkpoint`.
    async fn manifest(&self, checkpoint: &Checkpoint) -> io::Result<Arc<Manifest>> {
        if let Some(manifest) = self.inner().manifests.get(&checkpoint.pack) {
            return Ok(manifest.clone());
        }
        let key = checkpoint.pack.key();
        let bytes = (self.packs.get_range(&key, checkpoint.range()).await?)
            .ok_or_else(|| io::Error::other(format!("{key} is gone")))?;
        let manifest = Arc::new(Manifest::decode(&bytes)?);
        let mut inner = self.inner();
        let head = &inner.head;
        if [head.checkpoint, head.previous].contains(&Some(*checkpoint)) {
            inner.manifests.insert(checkpoint.pack, manifest.clone());
        }
        Ok(manifest)
    }

    /// Blob `n`: from the tail, or the pack a checkpoint's manifest names.
    async fn blob(&self, n: u64) -> io::Result<Option<Bytes>> {
        let (loaded, head) = {
            let inner = self.inner();
            if let Some(blob) = inner.tail.get(&n) {
                return Ok(Some(blob.clone()));
            }
            (inner.loaded, inner.head.clone())
        };
        if !loaded {
            self.load_tail().await?;
            if let Some(blob) = self.inner().tail.get(&n) {
                return Ok(Some(blob.clone()));
            }
        }
        for checkpoint in [head.checkpoint, head.previous].into_iter().flatten() {
            if n >= checkpoint.next_blob {
                continue;
            }
            let manifest = self.manifest(&checkpoint).await?;
            if let Some(at) = manifest.blobs.get(&n) {
                let key = at.pack.key();
                let range = at.offset..at.offset + at.len;
                let blob = self.packs.get_range(&key, range).await?;
                return blob
                    .map(Some)
                    .ok_or_else(|| io::Error::other(format!("{key} is gone")));
            }
        }
        Ok(None)
    }

    /// Move the blobs that `snapshot`, the head's durable state, names into
    /// a new pack, put it, and stage it for the next flush's head to name:
    /// those of the tail, and those of packs less than half alive.
    pub async fn checkpoint(&self, snapshot: &Snapshot) -> io::Result<()> {
        let head = self.head();
        if snapshot.version() != head.version {
            return Err(io::Error::other("the state is not the head's"));
        }
        let named = snapshot.named().await?;
        let mut manifest = match &head.checkpoint {
            Some(checkpoint) => (*self.manifest(checkpoint).await?).clone(),
            None => Manifest::default(),
        };
        manifest.blobs.retain(|n, _| named.contains(n));
        let mut live: HashMap<PackId, u64> = HashMap::new();
        for at in manifest.blobs.values() {
            *live.entry(at.pack).or_default() += at.len;
        }
        let sparse: BTreeSet<PackId> = (live.iter())
            .filter(|&(pack, &live)| live * 2 < manifest.sizes.get(pack).copied().unwrap_or(0))
            .map(|(&pack, _)| pack)
            .collect();
        let from = head.checkpoint.map_or(0, |c| c.next_blob);
        let mut fresh: BTreeSet<u64> = named.range(from..).copied().collect();
        fresh.extend(
            (manifest.blobs.iter())
                .filter(|(_, at)| sparse.contains(&at.pack))
                .map(|(&n, _)| n),
        );
        let fresh: Vec<u64> = fresh.into_iter().collect();
        let blobs = snapshot.blobs(&fresh).await?;
        let pack = PackId {
            version: head.version,
            nonce: cache::random(),
        };
        let mut body = Vec::with_capacity(blobs.iter().map(Bytes::len).sum());
        for (&n, blob) in fresh.iter().zip(&blobs) {
            let (offset, len) = (body.len() as u64, blob.len() as u64);
            manifest.blobs.insert(n, Location { pack, offset, len });
            body.extend_from_slice(blob);
        }
        manifest
            .sizes
            .retain(|pack, _| live.contains_key(pack) && !sparse.contains(pack));
        manifest.sizes.insert(pack, body.len() as u64);
        let encoded = manifest.encode();
        let checkpoint = Checkpoint {
            pack,
            next_blob: snapshot.next_blob(),
            manifest: (body.len() as u64, encoded.len() as u64),
        };
        body.extend_from_slice(&encoded);
        let report = Checkpointed {
            version: head.version,
            blobs: fresh.len(),
            pack_bytes: body.len(),
            manifest_bytes: encoded.len(),
            compacted: sparse.len(),
            deleted: 0,
        };
        self.packs.put(&pack.key(), body.into()).await?;
        let mut inner = self.inner();
        if inner.head == head {
            inner.manifests.insert(pack, Arc::new(manifest.clone()));
            inner.staged.checkpoint = Some((checkpoint, Arc::new(manifest), report));
        }
        Ok(())
    }

    /// The packs the head's checkpoints need.
    pub async fn needed(&self) -> io::Result<BTreeSet<PackId>> {
        let head = self.head();
        let mut needed = BTreeSet::new();
        for checkpoint in [head.checkpoint, head.previous].into_iter().flatten() {
            needed.insert(checkpoint.pack);
            needed.extend(self.manifest(&checkpoint).await?.packs());
        }
        Ok(needed)
    }

    /// Delete the packs no checkpoint the head names needs: see the module
    /// documentation. How many.
    pub async fn collect_garbage(&self) -> io::Result<usize> {
        let head = self.head();
        let Some(previous) = head.previous else {
            return Ok(0);
        };
        let keep = self.needed().await?;
        let listed = self.packs.list(PACK_PREFIX).await?;
        let garbage: Vec<String> = listed
            .into_iter()
            .filter(|key| {
                PackId::parse(key)
                    .is_some_and(|id| id.version < previous.pack.version && !keep.contains(&id))
            })
            .collect();
        let deleted = garbage.len();
        stream::iter(garbage.iter().map(|key| self.packs.delete(key)))
            .buffer_unordered(PARALLEL)
            .try_collect::<()>()
            .await?;
        Ok(deleted)
    }

    /// Write the staged flush: its tail entry, then the head; then sync.
    async fn write(&self) -> io::Result<Option<Checkpointed>> {
        let (next, puts, deletes, blobs, adopted) = {
            let mut inner = self.inner();
            let staged = std::mem::take(&mut inner.staged);
            let Some(root) = staged.root else {
                return Ok(None);
            };
            let head = &inner.head;
            let parts = encode_parts(&staged.blobs);
            let bytes: usize = parts.iter().map(Bytes::len).sum();
            let mut next = Head {
                version: head.version + 1,
                root,
                tail_bytes: head.tail_bytes + bytes as u64,
                ..head.clone()
            };
            next.parts.push(parts.len() as u32);
            let mut deletes = Vec::new();
            let mut adopted = None;
            if let Some((checkpoint, manifest, report)) = staged.checkpoint {
                // The tail the checkpoint before covered goes; the tail this one
                // covers stays, for readers that opened on it.
                if let Some(before) = head.checkpoint {
                    for version in head.tail_first..=before.pack.version {
                        deletes.extend((0..head.parts_of(version)).map(|p| tail_key(version, p)));
                    }
                    next.parts
                        .drain(..(before.pack.version + 1 - head.tail_first) as usize);
                    next.tail_first = before.pack.version + 1;
                }
                next.previous = head.checkpoint;
                next.checkpoint = Some(checkpoint);
                next.tail_bytes = bytes as u64;
                adopted = Some((checkpoint, manifest, report));
            }
            let puts: Vec<(String, Bytes)> = (parts.into_iter().enumerate())
                .map(|(p, part)| (tail_key(next.version, p as u32), part))
                .collect();
            (next, puts, deletes, staged.blobs, adopted)
        };
        for (key, part) in puts {
            self.restate.put(&key, part)?;
        }
        for key in &deletes {
            self.restate.delete(key)?;
        }
        self.restate.put(HEAD_KEY, next.encode())?;
        self.restate.sync().await?;
        let mut inner = self.inner();
        inner.tail.extend(blobs);
        let report = adopted.map(|(checkpoint, manifest, report)| {
            // Blobs of the tail entries dropped are in no head's tail now.
            if let Some(before) = next.previous {
                inner.tail.retain(|&n, _| n >= before.next_blob);
            }
            inner
                .manifests
                .retain(|pack, _| Some(*pack) == next.previous.map(|c| c.pack));
            inner.manifests.insert(checkpoint.pack, manifest);
            report
        });
        inner.head = next;
        Ok(report)
    }

    /// What the last flush's checkpoint did, if its head named one.
    pub fn take_adopted(&self) -> Option<Checkpointed> {
        self.inner().adopted.take()
    }
}

impl KvStore for Tiered {
    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, io::Result<Option<Bytes>>> {
        Box::pin(async move {
            if key == ROOT_KEY {
                return self.read_head().await;
            }
            match parse_blob_key(key) {
                Some(n) => self.blob(n).await,
                None => Err(io::Error::other(format!("no key {key} here"))),
            }
        })
    }

    fn put(&self, key: &str, value: Bytes) -> io::Result<()> {
        let mut inner = self.inner();
        if key == ROOT_KEY {
            inner.staged.root = Some(value);
            return Ok(());
        }
        let n =
            parse_blob_key(key).ok_or_else(|| io::Error::other(format!("no key {key} here")))?;
        inner.staged.blobs.push((n, value));
        Ok(())
    }

    fn delete(&self, key: &str) -> io::Result<()> {
        Err(io::Error::other(format!("{key} is never deleted")))
    }

    fn sync(&self) -> BoxFuture<'_, io::Result<()>> {
        Box::pin(async move {
            if self.inner().staged.root.is_none() {
                self.inner().staged.blobs.clear();
                return self.restate.sync().await;
            }
            let adopted = self.write().await?;
            self.inner().adopted = adopted;
            Ok(())
        })
    }
}

#[cfg(test)]
#[path = "../tests/unit/tail.rs"]
mod tests;
