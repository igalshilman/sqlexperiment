//! A protocol monitor: checks every request the code issues to Restate's
//! state against the rules of [`crate::store`] and [`crate::tail`]: a blob
//! is stored before the first head that names it, and dropped only once no
//! head a state may hold needs it.
//!
//! The store keeps writes in issue order, and a crash keeps a prefix of them,
//! so a write may rely on every write issued before it. The monitor wraps a
//! store and keeps the state those writes make: each write applied as it is
//! issued, and values it read (after a crash, a read learns what was kept).
//! A write that fails makes its key unknown until a read learns it; a read
//! of the head forgets the blobs at or past its root's next blob, which a
//! crash may have lost. Each write is checked when it is issued:
//!
//! 1. **Name only what is stored**: a head's root names only blobs in the
//!    tail entries it keeps, or, below its checkpoint's next blob, in its
//!    checkpoint's manifest.
//! 2. **Build on the head**: a head is the next version, with the same
//!    identity, chunk size and fanout, a next blob never lower, and a tail
//!    that starts no earlier; a new checkpoint's previous is the one held.
//! 3. **Never overwrite**: a tail entry is put only for the version after
//!    the head's, its blobs at or past the root's next blob.
//! 4. **A checkpoint holds what it names**: when a head first names a
//!    checkpoint, every blob its root names below the checkpoint's next blob
//!    is in the manifest, in a pack, with the bytes the tail held (where the
//!    monitor sees the packs: in the store, or given [`MonitorKv::packs`]).
//! 5. **Drop only what no head needs**: a tail entry is deleted only once
//!    the head's checkpoint covers it, and a pack in the store only if
//!    older than the head's previous checkpoint, and neither checkpoint
//!    names it.
//! 6. **Decodable**: every head and root decodes, and every map node, chunk
//!    and record and log entry blob it names decodes: a chunk's base against
//!    its file's golden pages, its delta against its base.
//!
//! What a root names it learns by walking each file's map through the node
//! blobs it knows; a node a failed write left unknown is not walked.
use crate::{
    Base, PAGE,
    blobs::KV_PREFIX,
    kv::KvStore,
    map::{Map, Node, Nodes, Shape, Visit},
    simulation::MemoryBlobs,
    store::{Chunk, Kind, MAX_RECORD_BYTES, Root, Value, decode_chunk, reference},
    tail::{HEAD_KEY, Head, Manifest, PackId, parse_tail_key, read_part},
    zstd,
};
use bytes::Bytes;
use futures::future::BoxFuture;
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    io,
    ops::Deref,
    sync::{Arc, Mutex},
};

/// A store wrapper that checks the protocol invariants on every request.
pub struct MonitorKv<S> {
    pub inner: S,
    known: Mutex<Known>,
}

impl<S> Deref for MonitorKv<S> {
    type Target = S;
    fn deref(&self) -> &S {
        &self.inner
    }
}

/// The state the writes issued so far make.
#[derive(Default)]
struct Known {
    /// Blobs the stored state holds: in tail entries, or moved to packs.
    blobs: BTreeMap<u64, Bytes>,
    /// Blobs a failed write may or may not have changed.
    unknown: BTreeSet<u64>,
    /// The head and its root, unless a failed write left them unknown.
    head: Option<(Head, Root)>,
    head_unknown: bool,
    /// The blobs of each tail entry, by version.
    tail: BTreeMap<u64, BTreeSet<u64>>,
    /// Packs in the store: their parts, by key.
    packs: BTreeMap<String, BTreeMap<u64, Bytes>>,
    /// Packs elsewhere ([`MonitorKv::packs`]).
    external: Option<MemoryBlobs>,
    manifests: HashMap<PackId, Option<Manifest>>,
    violations: Vec<String>,
    /// The bases of files made with one, by name.
    bases: BTreeMap<String, Arc<dyn Base>>,
    checked: usize,
}

/// A key of the store, as the monitor tells them apart.
enum Key {
    Head,
    Tail(u64),
    Pack(String, u64),
    Other,
}

fn key(key: &str) -> Key {
    if key == HEAD_KEY {
        return Key::Head;
    }
    if let Some((version, _)) = parse_tail_key(key) {
        return Key::Tail(version);
    }
    let pack = (key.strip_prefix(KV_PREFIX)).and_then(|rest| rest.rsplit_once('/'));
    match pack.and_then(|(pack, part)| Some((pack.to_owned(), part.parse().ok()?))) {
        Some((pack, part)) => Key::Pack(pack, part),
        None => Key::Other,
    }
}

impl Known {
    fn violation(&mut self, message: String) {
        self.violations.push(message);
    }

    fn learn(&mut self, at: &str, value: Option<Bytes>) {
        match key(at) {
            Key::Head => {
                let head = value.and_then(|bytes| {
                    let head = Head::decode(&bytes).ok()?;
                    let root = Root::decode(&head.root).ok()?;
                    Some((head, root))
                });
                let (next, version) = head
                    .as_ref()
                    .map_or((0, 0), |(head, root)| (root.next_blob, head.version));
                self.head = head;
                self.head_unknown = false;
                self.blobs.retain(|&n, _| n < next);
                self.unknown.retain(|&n| n < next);
                self.tail.retain(|&v, _| v <= version);
            }
            Key::Tail(version) => match value {
                Some(value) => {
                    let mut blobs = HashMap::new();
                    if read_part(&value, &mut blobs).is_ok() {
                        for (n, blob) in blobs {
                            self.unknown.remove(&n);
                            self.tail.entry(version).or_default().insert(n);
                            self.blobs.insert(n, blob);
                        }
                    }
                }
                None => {
                    // Its blobs the checkpoint holds stay; the others go
                    // (all stay where the monitor cannot read the manifest).
                    let moved = self.checkpointed();
                    for n in self.tail.remove(&version).unwrap_or_default() {
                        if moved.as_ref().is_some_and(|moved| !moved.contains(&n)) {
                            self.blobs.remove(&n);
                        }
                    }
                }
            },
            Key::Pack(pack, part) => {
                let parts = self.packs.entry(pack.clone()).or_default();
                match value {
                    Some(value) => {
                        parts.insert(part, value);
                    }
                    None => {
                        parts.remove(&part);
                        if parts.is_empty() {
                            self.packs.remove(&pack);
                        }
                    }
                }
            }
            Key::Other => {}
        }
    }

    fn forget(&mut self, at: &str, value: Option<&Bytes>) {
        match key(at) {
            Key::Head => self.head_unknown = true,
            Key::Tail(_) => {
                let mut blobs = HashMap::new();
                if let Some(value) = value
                    && read_part(value, &mut blobs).is_ok()
                {
                    self.unknown.extend(blobs.into_keys());
                }
            }
            Key::Pack(..) | Key::Other => {}
        }
    }

    /// The head and its root, unless a failed write left them unknown.
    fn held(&self) -> Option<&(Head, Root)> {
        self.head.as_ref().filter(|_| !self.head_unknown)
    }

    /// The bytes `range` of the pack `pack`, if the monitor sees it.
    fn pack_range(&self, pack: PackId, range: std::ops::Range<u64>) -> Option<Bytes> {
        let key = pack.key();
        if let Some(external) = &self.external {
            return external.peek(&key, range);
        }
        let parts = self.packs.get(&key)?;
        let mut whole = Vec::new();
        for (expected, (&part, bytes)) in parts.iter().enumerate() {
            if part != expected as u64 {
                return None;
            }
            whole.extend_from_slice(bytes);
        }
        let range = range.start as usize..range.end as usize;
        whole.get(range).map(Bytes::copy_from_slice)
    }

    /// The manifest of the checkpoint `checkpoint`, if the monitor sees it.
    fn manifest(&mut self, checkpoint: &crate::tail::Checkpoint) -> Option<&Manifest> {
        if !self.manifests.contains_key(&checkpoint.pack) {
            let (at, len) = checkpoint.manifest;
            let manifest = self
                .pack_range(checkpoint.pack, at..at + len)
                .and_then(|bytes| Manifest::decode(&bytes).ok());
            self.manifests.insert(checkpoint.pack, manifest);
        }
        self.manifests[&checkpoint.pack].as_ref()
    }

    /// The blobs the held head's checkpoint holds, if the monitor sees its
    /// manifest.
    fn checkpointed(&mut self) -> Option<BTreeSet<u64>> {
        let Some(checkpoint) = self.held().and_then(|(head, _)| head.checkpoint) else {
            return Some(BTreeSet::new());
        };
        let manifest = self.manifest(&checkpoint)?;
        Some(manifest.blobs.keys().copied().collect())
    }

    fn check_put(&mut self, at: &str, value: &Bytes) {
        self.checked += 1;
        match key(at) {
            Key::Head => self.check_head(value),
            Key::Tail(version) => {
                let mut blobs = HashMap::new();
                if let Err(error) = read_part(value, &mut blobs) {
                    return self.violation(format!("tail entry {version}: {error}"));
                }
                let Some((head, root)) = self.held() else {
                    return;
                };
                let (expected, next) = (head.version + 1, root.next_blob);
                if version != expected {
                    self.violation(format!("tail entry {version} put; the next is {expected}"));
                }
                for n in blobs.into_keys().filter(|&n| n < next) {
                    self.violation(format!("blob {n} put, below the root's next blob"));
                }
            }
            Key::Pack(..) => {}
            Key::Other => self.violation(format!("a put of {at:?}")),
        }
    }

    fn check_head(&mut self, value: &Bytes) {
        let decoded = Head::decode(value).and_then(|head| {
            let root = Root::decode(&head.root)?;
            Ok((head, root))
        });
        let (head, root) = match decoded {
            Ok(decoded) => decoded,
            Err(error) => return self.violation(format!("a head that does not decode: {error}")),
        };
        let v = head.version;
        if let Some((held, held_root)) = self.held().cloned() {
            if head.version != held.version + 1
                || root.version != head.version
                || root.id != held_root.id
                || root.chunk_pages != held_root.chunk_pages
                || root.fanout != held_root.fanout
                || root.next_blob < held_root.next_blob
                || head.tail_first < held.tail_first
            {
                let held_v = held.version;
                self.violation(format!("head {v} does not build on head {held_v}"));
            }
            if head.checkpoint != held.checkpoint && head.previous != held.checkpoint {
                self.violation(format!(
                    "head {v}: a new checkpoint, not after the one held"
                ));
            }
        }
        let (named, _) = self.named(&root);
        let manifest = match head.checkpoint {
            Some(checkpoint) => {
                let visible =
                    self.external.is_some() || self.packs.contains_key(&checkpoint.pack.key());
                match self.manifest(&checkpoint).cloned() {
                    Some(manifest) => Some((checkpoint, manifest)),
                    None if visible => {
                        let error = format!("head {v}: its checkpoint's manifest is unreadable");
                        return self.violation(error);
                    }
                    None => None,
                }
            }
            None => None,
        };
        for &n in &named {
            let packed = manifest.as_ref().filter(|(c, _)| n < c.next_blob);
            match packed {
                Some((_, manifest)) => match manifest.blobs.get(&n) {
                    None => {
                        self.violation(format!("head {v} names blob {n}, not in its checkpoint"))
                    }
                    Some(at) => {
                        let stored = self.pack_range(at.pack, at.offset..at.offset + at.len);
                        let held = self.blobs.get(&n);
                        if stored.is_none() {
                            self.violation(format!("head {v} names blob {n}, its pack gone"));
                        } else if held.is_some() && stored.as_ref() != held {
                            self.violation(format!("head {v}: blob {n} differs in its pack"));
                        }
                    }
                },
                None if !self.blobs.contains_key(&n) && !self.unknown.contains(&n) => {
                    self.violation(format!("head {v} names blob {n}, not stored"));
                }
                None => {}
            }
        }
        if let Err(error) = self.decode(&root) {
            self.violation(format!("root {v}: {error}"));
        }
    }

    fn check_delete(&mut self, at: &str) {
        self.checked += 1;
        let Some((head, _)) = self.held().cloned() else {
            return;
        };
        match key(at) {
            Key::Tail(version) => {
                if head.checkpoint.is_none_or(|c| version > c.pack.version) {
                    self.violation(format!("tail entry {version} deleted, not checkpointed"));
                }
            }
            Key::Pack(pack, _) => {
                let Some(id) = PackId::parse(&pack) else {
                    return self.violation(format!("a delete of {at:?}"));
                };
                let mut needed = BTreeSet::new();
                for checkpoint in [head.checkpoint, head.previous].into_iter().flatten() {
                    needed.insert(checkpoint.pack);
                    if let Some(manifest) = self.manifest(&checkpoint) {
                        needed.extend(manifest.packs());
                    }
                }
                let older = head.previous.is_some_and(|p| id.version < p.pack.version);
                if !older || needed.contains(&id) {
                    self.violation(format!("pack {pack} deleted, still needed"));
                }
            }
            Key::Head | Key::Other => self.violation(format!("a delete of {at:?}")),
        }
    }

    /// Walk `map`, of `shape`, through the node blobs known, decoding them
    /// as the walk needs them: what it meets, the nodes that do not decode,
    /// and how many it lacked.
    fn walk(&self, map: &Map, shape: Shape) -> (Vec<Visit>, Vec<String>, usize) {
        let mut decoded: HashMap<u64, Arc<Node>> = HashMap::new();
        let mut lacking = BTreeSet::new();
        let mut errors = Vec::new();
        loop {
            let mut met = Vec::new();
            let missing = {
                let get = |n: u64| decoded.get(&n).cloned();
                let nodes = Nodes::new(&get);
                map.walk(shape, &nodes, &mut |visit| met.push(visit));
                nodes.missing()
            };
            let missing: Vec<_> = (missing.into_iter())
                .filter(|need| !lacking.contains(&need.blob))
                .collect();
            if missing.is_empty() {
                return (met, errors, lacking.len());
            }
            for need in missing {
                let node = self
                    .blobs
                    .get(&need.blob)
                    .map(|f| Node::from_frame(f, need));
                match node {
                    Some(Ok(node)) => {
                        decoded.insert(need.blob, Arc::new(node));
                    }
                    Some(Err(error)) => {
                        errors.push(format!("map node {}: {error}", need.blob));
                        lacking.insert(need.blob);
                    }
                    None => {
                        lacking.insert(need.blob);
                    }
                }
            }
        }
    }

    /// Every blob `root` names, through the map nodes known, and whether
    /// it walked every map whole.
    fn named(&self, root: &Root) -> (BTreeSet<u64>, bool) {
        let mut named = BTreeSet::new();
        let mut whole = true;
        for entry in root.directory.values() {
            match &entry.kind {
                Kind::Record(Value::Blob(n)) => {
                    named.insert(*n);
                }
                Kind::Record(Value::Inline(_)) => {}
                Kind::Log { entries, .. } => named.extend(entries),
                Kind::Paged { size, map, .. } => {
                    let (met, errors, lacking) = self.walk(map, root.shape(*size));
                    whole &= errors.is_empty() && lacking == 0;
                    for visit in met {
                        match visit {
                            Visit::Node(n) => {
                                named.insert(n);
                            }
                            Visit::Chunk(_, chunk) => named.extend(chunk.blobs()),
                        }
                    }
                }
            }
        }
        (named, whole)
    }

    /// Decode every chunk, record and log entry blob `root` names (those a failed write
    /// left unknown aside).
    fn decode(&self, root: &Root) -> io::Result<()> {
        let size = (root.chunk_pages * PAGE) as usize;
        let blob = |n: u64| self.blobs.get(&n);
        for entry in root.directory.values() {
            let (bytes, map, base) = match &entry.kind {
                Kind::Paged { size, map, base } => (*size, map, base),
                Kind::Record(Value::Blob(n)) => {
                    if let Some(frame) = blob(*n) {
                        zstd::decompress_sized(frame, &[], MAX_RECORD_BYTES)?;
                    }
                    continue;
                }
                Kind::Record(Value::Inline(_)) => continue,
                Kind::Log { entries, .. } => {
                    for &n in entries {
                        if let Some(frame) = blob(n) {
                            zstd::decompress_sized(frame, &[], MAX_RECORD_BYTES)?;
                        }
                    }
                    continue;
                }
            };
            let (met, errors, _) = self.walk(map, root.shape(bytes));
            if let Some(error) = errors.into_iter().next() {
                return Err(io::Error::other(error));
            }
            let golden = self.bases.get(&entry.name);
            if base.is_some() && golden.is_none() {
                continue;
            }
            let chunks = met.into_iter().filter_map(|visit| match visit {
                Visit::Chunk(chunk, stored) => Some((chunk, stored)),
                Visit::Node(_) => None,
            });
            for (chunk, stored) in chunks {
                let stored: Chunk = stored;
                if stored.base.is_none() && stored.delta.is_some() {
                    return Err(io::Error::other(format!(
                        "chunk {chunk}: a delta without a base"
                    )));
                }
                let Some(frame) = stored.base.and_then(blob) else {
                    continue;
                };
                let prefix = reference(golden, root.chunk_pages, chunk);
                let base = decode_chunk(frame, &prefix, size)?;
                if let Some(frame) = stored.delta.and_then(blob) {
                    decode_chunk(frame, &base.pages, size)?;
                }
            }
        }
        Ok(())
    }
}

impl<S: KvStore> MonitorKv<S> {
    pub fn new(inner: S) -> Self {
        Self {
            inner,
            known: Mutex::default(),
        }
    }

    /// A monitor that knows `values` are durable, such as a store's contents
    /// before a campaign starts.
    pub fn seeded(inner: S, values: impl IntoIterator<Item = (String, Bytes)>) -> Self {
        let monitor = Self::new(inner);
        {
            let mut known = monitor.known.lock().unwrap();
            let mut values: Vec<(String, Bytes)> = values.into_iter().collect();
            // The head last: it forgets the blobs past its root's next.
            values.sort_by_key(|(key, _)| key == HEAD_KEY);
            for (key, value) in values {
                known.learn(&key, Some(value));
            }
        }
        monitor
    }

    /// Read checkpoints' packs from `packs`, where the state keeps them
    /// ([`crate::Open::checkpoints`]).
    pub fn packs(self, packs: &MemoryBlobs) -> Self {
        self.known.lock().unwrap().external = Some(packs.clone());
        self
    }

    /// Decode the chunks of the file `name` against `base`, the one it is
    /// made with (see [`crate::Base`]).
    pub fn with_base(self, name: &str, base: Arc<dyn Base>) -> Self {
        self.known.lock().unwrap().bases.insert(name.into(), base);
        self
    }

    /// Invariant violations seen so far.
    pub fn violations(&self) -> Vec<String> {
        self.known.lock().unwrap().violations.clone()
    }

    /// Requests checked so far.
    pub fn checked(&self) -> usize {
        self.known.lock().unwrap().checked
    }
}

impl<S: KvStore> KvStore for MonitorKv<S> {
    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, io::Result<Option<Bytes>>> {
        Box::pin(async move {
            let value = self.inner.get(key).await?;
            self.known.lock().unwrap().learn(key, value.clone());
            Ok(value)
        })
    }

    fn put(&self, key: &str, value: Bytes) -> io::Result<()> {
        let mut known = self.known.lock().unwrap();
        known.check_put(key, &value);
        let result = self.inner.put(key, value.clone());
        match &result {
            Ok(()) => known.learn(key, Some(value)),
            Err(_) => known.forget(key, Some(&value)),
        }
        result
    }

    fn delete(&self, key: &str) -> io::Result<()> {
        let mut known = self.known.lock().unwrap();
        known.check_delete(key);
        let result = self.inner.delete(key);
        match &result {
            Ok(()) => known.learn(key, None),
            Err(_) => known.forget(key, None),
        }
        result
    }

    fn sync(&self) -> BoxFuture<'_, io::Result<()>> {
        self.inner.sync()
    }

    fn keys(&self) -> BoxFuture<'_, io::Result<Vec<String>>> {
        self.inner.keys()
    }
}
