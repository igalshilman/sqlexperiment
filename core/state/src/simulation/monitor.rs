//! A protocol monitor: checks every request the code issues against the
//! rules of [`crate::store`]: a blob is written before the first root that
//! names it, never changes, and is deleted only by a sweep, once no root
//! names it.
//!
//! The store keeps writes in issue order, and a crash keeps a prefix of them,
//! so a write may rely on every write issued before it. The monitor wraps a
//! store and keeps the state those writes make: each write applied as it is
//! issued, and values it read (after a crash, a read learns what was kept).
//! A write that fails makes its key unknown until a read learns it. A read
//! of the root makes every blob it does not name uncertain, since a crash
//! may have lost it, until a put, a read or a listing settles it. Each write
//! is checked when it is issued:
//!
//! 1. **Name only what is written**: a root names only blobs that are there.
//! 2. **Build on the root**: a root is the next version of the root, with
//!    the same identity, chunk size and fanout.
//! 3. **Never change a name**: a blob put where one is holds the same bytes.
//! 4. **Names are hashes**: every blob a root names is named by its hash
//!    ([`BlobId::of`]), against what it is compressed against.
//! 5. **Revive before naming**: a root names none of the sweep's candidates
//!    (a flush takes those it names off, before the root).
//! 6. **Delete only candidates**: a blob is deleted only if the sweep lists
//!    it as a candidate, and the root names it no more.
//! 7. **Leak nothing**: a sweep's root comes with every blob it does not
//!    name either deleted or a candidate.
//! 8. **Decodable**: every root and sweep decodes, and every map node, chunk
//!    and record and log entry blob a root names decodes: a chunk's base
//!    against its file's golden pages, its delta against its base.
//!
//! What a root names it learns by walking each file's map through the node
//! blobs it knows; a node a failed write left unknown is not walked.
use crate::{
    Base, PAGE,
    kv::KvStore,
    map::{Map, Node, Nodes, Shape, Visit},
    store::{
        Against, BlobId, Chunk, Kind, MAX_RECORD_BYTES, ROOT_KEY, Root, SWEEP_KEY, Sweep, Value,
        decode_chunk, parse_blob_key, reference,
    },
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
    blobs: BTreeMap<BlobId, Bytes>,
    /// Blobs a failed write may or may not have changed.
    unknown: BTreeSet<BlobId>,
    /// Blobs a crash may have lost: in `blobs`, but not named by the root
    /// read since.
    uncertain: BTreeSet<BlobId>,
    /// The root, unless a failed write left it unknown.
    root: Option<Root>,
    root_unknown: bool,
    /// The sweep, unless a failed write left it unknown.
    sweep: Option<Sweep>,
    sweep_unknown: bool,
    /// The version of the root a sweep just put its sweep for.
    swept: Option<u64>,
    violations: Vec<String>,
    /// The bases of files made with one, by name.
    bases: BTreeMap<String, Arc<dyn Base>>,
    checked: usize,
}

impl Known {
    fn violation(&mut self, message: String) {
        self.violations.push(message);
    }

    fn root(&self) -> Option<&Root> {
        self.root.as_ref().filter(|_| !self.root_unknown)
    }

    fn sweep(&self) -> Option<&Sweep> {
        match self.sweep_unknown {
            true => None,
            false => Some(self.sweep.as_ref().unwrap_or(&EMPTY)),
        }
    }

    /// `value` read at `key`.
    fn read(&mut self, key: &str, value: Option<Bytes>) {
        self.learn(key, value);
        self.swept = None;
        if key == ROOT_KEY {
            let named = match &self.root {
                Some(root) => self.named(root),
                None => BTreeSet::new(),
            };
            let unnamed = self.blobs.keys().filter(|blob| !named.contains(blob));
            self.uncertain = unnamed.copied().collect();
        }
    }

    fn learn(&mut self, key: &str, value: Option<Bytes>) {
        if key == ROOT_KEY {
            self.root = value.and_then(|bytes| Root::decode(&bytes).ok());
            self.root_unknown = false;
        } else if key == SWEEP_KEY {
            self.sweep = value.and_then(|bytes| Sweep::decode(&bytes).ok());
            self.sweep_unknown = false;
        } else if let Some(blob) = parse_blob_key(key) {
            self.unknown.remove(&blob);
            self.uncertain.remove(&blob);
            match value {
                Some(value) => self.blobs.insert(blob, value),
                None => self.blobs.remove(&blob),
            };
        }
    }

    /// The blobs stored are `listed`, of those under `prefix`.
    fn listed(&mut self, prefix: &str, listed: &[String]) {
        let listed: BTreeSet<BlobId> = listed.iter().filter_map(|k| parse_blob_key(k)).collect();
        let gone: Vec<BlobId> = (self.uncertain.iter().chain(&self.unknown))
            .filter(|blob| crate::store::blob_key(**blob).starts_with(prefix))
            .filter(|blob| !listed.contains(blob))
            .copied()
            .collect();
        for blob in gone {
            self.learn(&crate::store::blob_key(blob), None);
        }
        self.uncertain.retain(|blob| !listed.contains(blob));
    }

    fn forget(&mut self, key: &str) {
        if key == ROOT_KEY {
            self.root_unknown = true;
        } else if key == SWEEP_KEY {
            self.sweep_unknown = true;
            self.swept = None;
        } else if let Some(blob) = parse_blob_key(key) {
            self.unknown.insert(blob);
        }
    }

    fn check_put(&mut self, key: &str, value: &Bytes) {
        self.checked += 1;
        if let Some(blob) = parse_blob_key(key) {
            if (self.blobs.get(&blob)).is_some_and(|held| held != value) {
                self.violation(format!("blob {blob} changed"));
            }
            return;
        }
        if key == SWEEP_KEY {
            return self.check_sweep(value);
        }
        if key != ROOT_KEY {
            return self.violation(format!("a put of {key:?}: no root, sweep or blob"));
        }
        let root = match Root::decode(value) {
            Ok(root) => root,
            Err(error) => return self.violation(format!("a root that does not decode: {error}")),
        };
        let version = root.version;
        let named = self.named(&root);
        if let Some(held) = self.root()
            && (root.version != held.version + 1
                || root.id != held.id
                || root.chunk_pages != held.chunk_pages
                || root.fanout != held.fanout)
        {
            let held = held.version;
            self.violation(format!("root {version} does not build on root {held}"));
        }
        for blob in &named {
            let written = self.blobs.contains_key(blob) && !self.uncertain.contains(blob);
            if !written && !self.unknown.contains(blob) {
                self.violation(format!("root {version} names blob {blob}, not written"));
            }
        }
        if let Some(sweep) = self.sweep() {
            let revived = sweep.candidates.intersection(&named);
            let mut violations: Vec<String> = revived
                .map(|blob| format!("root {version} names candidate {blob}"))
                .collect();
            // A sweep's root: every blob it does not name is a candidate
            // (known only if every map node is).
            if self.swept == Some(version) && self.walks_fully(&root) {
                let leaked = (self.blobs.keys())
                    .filter(|blob| !self.uncertain.contains(blob) && !self.unknown.contains(blob))
                    .filter(|blob| !named.contains(blob) && !sweep.candidates.contains(blob));
                violations.extend(leaked.map(|blob| format!("sweep {version} leaks blob {blob}")));
            }
            self.violations.extend(violations);
        }
        if let Err(error) = self.decode(&root) {
            self.violation(format!("root {version}: {error}"));
        }
        self.swept = None;
    }

    /// A sweep put: the next sweep, of the root's next version, whose
    /// candidates the root does not name; or the sweep with candidates
    /// taken off.
    fn check_sweep(&mut self, value: &Bytes) {
        let sweep = match Sweep::decode(value) {
            Ok(sweep) => sweep,
            Err(error) => return self.violation(format!("a sweep that does not decode: {error}")),
        };
        let (Some(root), Some(held)) = (self.root(), self.sweep()) else {
            return;
        };
        let next = root.version + 1;
        let revived = sweep.marked == held.marked && sweep.candidates.is_subset(&held.candidates);
        if !revived && sweep.marked == next {
            let named = self.named(root);
            let named: Vec<BlobId> = sweep.candidates.intersection(&named).copied().collect();
            for blob in named {
                self.violation(format!(
                    "sweep {next} lists blob {blob}, which the root names"
                ));
            }
            self.swept = Some(next);
        } else if !revived {
            let marked = sweep.marked;
            self.violation(format!("sweep {marked}: neither the next one nor revivals"));
        }
    }

    fn check_delete(&mut self, key: &str) {
        self.checked += 1;
        let Some(blob) = parse_blob_key(key) else {
            return self.violation(format!("a delete of {key:?}, not a blob"));
        };
        if let Some(root) = self.root()
            && self.named(root).contains(&blob)
        {
            self.violation(format!("blob {blob} deleted, which the root names"));
        }
        if let Some(sweep) = self.sweep()
            && !sweep.candidates.contains(&blob)
        {
            self.violation(format!("blob {blob} deleted, not a candidate"));
        }
    }

    /// Walk `map`, of `shape`, through the node blobs known, decoding them
    /// as the walk needs them: what it meets, and the nodes that do not
    /// decode.
    fn walk(&self, map: &Map, shape: Shape) -> (Vec<Visit>, Vec<String>) {
        let mut decoded: HashMap<BlobId, Arc<Node>> = HashMap::new();
        let mut lacking = BTreeSet::new();
        let mut errors = Vec::new();
        loop {
            let mut met = Vec::new();
            let missing = {
                let get = |blob: BlobId| decoded.get(&blob).cloned();
                let nodes = Nodes::new(&get);
                map.walk(shape, &nodes, &mut |visit| met.push(visit));
                nodes.missing()
            };
            let missing: Vec<_> = (missing.into_iter())
                .filter(|need| !lacking.contains(&need.blob))
                .collect();
            if missing.is_empty() {
                return (met, errors);
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

    /// Whether every map node `root` names is known.
    fn walks_fully(&self, root: &Root) -> bool {
        root.directory.values().all(|entry| match &entry.kind {
            Kind::Paged { size, map, .. } => {
                let (met, errors) = self.walk(map, root.shape(*size));
                errors.is_empty()
                    && met.iter().all(|visit| match visit {
                        Visit::Node(blob) => self.blobs.contains_key(blob),
                        Visit::Chunk(..) => true,
                    })
            }
            _ => true,
        })
    }

    /// Every blob `root` names, through the map nodes known.
    fn named(&self, root: &Root) -> BTreeSet<BlobId> {
        let mut named = BTreeSet::new();
        for entry in root.directory.values() {
            match &entry.kind {
                Kind::Record(Value::Blob(blob)) => {
                    named.insert(*blob);
                }
                Kind::Record(Value::Inline(_)) => {}
                Kind::Log { entries, .. } => named.extend(entries),
                Kind::Paged { size, map, .. } => {
                    for visit in self.walk(map, root.shape(*size)).0 {
                        match visit {
                            Visit::Node(blob) => {
                                named.insert(blob);
                            }
                            Visit::Chunk(_, chunk) => named.extend(chunk.blobs()),
                        }
                    }
                }
            }
        }
        named
    }

    /// Check the name of `blob`, compressed `against` a reference, if known:
    /// its frame.
    fn hashed(&self, root: &Root, blob: BlobId, against: Against) -> io::Result<Option<&Bytes>> {
        let Some(frame) = self.blobs.get(&blob) else {
            return Ok(None);
        };
        match BlobId::of(root.id, against, frame) == blob {
            true => Ok(Some(frame)),
            false => Err(io::Error::other(format!(
                "blob {blob} is not named by its hash"
            ))),
        }
    }

    /// Check the names of, and decode, every map node, chunk, record and log
    /// entry blob `root` names (those a failed write left unknown aside).
    fn decode(&self, root: &Root) -> io::Result<()> {
        let size = (root.chunk_pages * PAGE) as usize;
        let value = |blob: BlobId| -> io::Result<()> {
            if let Some(frame) = self.hashed(root, blob, Against::Nothing)? {
                zstd::decompress_sized(frame, &[], MAX_RECORD_BYTES)?;
            }
            Ok(())
        };
        for entry in root.directory.values() {
            let (bytes, map, base) = match &entry.kind {
                Kind::Paged { size, map, base } => (*size, map, *base),
                Kind::Record(Value::Blob(blob)) => {
                    value(*blob)?;
                    continue;
                }
                Kind::Record(Value::Inline(_)) => continue,
                Kind::Log { entries, .. } => {
                    for &blob in entries {
                        value(blob)?;
                    }
                    continue;
                }
            };
            let (met, errors) = self.walk(map, root.shape(bytes));
            if let Some(error) = errors.into_iter().next() {
                return Err(io::Error::other(error));
            }
            let golden = self.bases.get(&entry.name);
            for visit in met {
                let (chunk, stored): (u64, Chunk) = match visit {
                    Visit::Chunk(chunk, stored) => (chunk, stored),
                    Visit::Node(blob) => {
                        self.hashed(root, blob, Against::Nothing)?;
                        continue;
                    }
                };
                if stored.base.is_none() && stored.delta.is_some() {
                    return Err(io::Error::other(format!(
                        "chunk {chunk}: a delta without a base"
                    )));
                }
                let Some(blob) = stored.base else { continue };
                let against = match base {
                    Some(base) => Against::Base { base, chunk },
                    None => Against::Nothing,
                };
                let frame = self.hashed(root, blob, against)?;
                let delta = match stored.delta {
                    Some(delta) => self.hashed(root, delta, Against::Blob(blob))?,
                    None => None,
                };
                // A file made with a base decodes only against it.
                let (Some(frame), false) = (frame, base.is_some() && golden.is_none()) else {
                    continue;
                };
                let prefix = reference(golden, root.chunk_pages, chunk);
                let pages = decode_chunk(frame, &prefix, size)?;
                if let Some(frame) = delta {
                    decode_chunk(frame, &pages.pages, size)?;
                }
            }
        }
        Ok(())
    }
}

/// The sweep of a store that holds none.
static EMPTY: Sweep = Sweep {
    marked: 0,
    candidates: BTreeSet::new(),
};

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
            for (key, value) in values {
                known.learn(&key, Some(value));
            }
        }
        monitor
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
            self.known.lock().unwrap().read(key, value.clone());
            Ok(value)
        })
    }

    fn put(&self, key: &str, value: Bytes) -> io::Result<()> {
        let mut known = self.known.lock().unwrap();
        known.check_put(key, &value);
        let result = self.inner.put(key, value.clone());
        match &result {
            Ok(()) => known.learn(key, Some(value)),
            Err(_) => known.forget(key),
        }
        result
    }

    fn delete(&self, key: &str) -> io::Result<()> {
        let mut known = self.known.lock().unwrap();
        known.check_delete(key);
        let result = self.inner.delete(key);
        match &result {
            Ok(()) => known.learn(key, None),
            Err(_) => known.forget(key),
        }
        result
    }

    fn sync(&self) -> BoxFuture<'_, io::Result<()>> {
        self.inner.sync()
    }
    fn list<'a>(&'a self, prefix: &'a str) -> BoxFuture<'a, io::Result<Vec<String>>> {
        Box::pin(async move {
            let listed = self.inner.list(prefix).await?;
            self.known.lock().unwrap().listed(prefix, &listed);
            Ok(listed)
        })
    }
}
