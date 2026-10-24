//! A protocol monitor: checks every request the code issues against the
//! rule of [`crate::store`]: a blob is written before the first root that
//! names it, and deleted only once no root names it.
//!
//! The store keeps writes in issue order, and a crash keeps a prefix of them,
//! so a write may rely on every write issued before it. The monitor wraps a
//! store and keeps the state those writes make: each write applied as it is
//! issued, and values it read (after a crash, a read learns what was kept).
//! A write that fails makes its key unknown until a read learns it; a read
//! of the root forgets the blobs at or past its next blob, which a crash may
//! have lost (a flush issues its blobs and root with nothing between). Each
//! write is checked when it is issued:
//!
//! 1. **Name only what is written**: a root names only blobs that are there.
//! 2. **Build on the root**: a root is the next version of the root, with
//!    the same identity and chunk size, and a next blob never lower.
//! 3. **Never overwrite**: a blob is put only at a number the root has not
//!    allocated.
//! 4. **Delete only garbage**: a blob is deleted only if the root lists it
//!    as garbage, and names it no more.
//! 5. **Leak nothing**: a blob the root named and the next one does not is
//!    its garbage, and stays garbage until deleted.
//! 6. **Decodable**: every root decodes, and every map node, chunk and
//!    record blob it names decodes: a chunk's base against its file's golden
//!    pages, its delta against its base.
//!
//! What a root names it learns by walking each file's map through the node
//! blobs it knows; a node a failed write left unknown is not walked.
use crate::{
    Base, PAGE,
    kv::KvStore,
    map::{Map, Node, Nodes, Shape, Visit},
    store::{
        Chunk, Kind, MAX_RECORD_BYTES, ROOT, Root, Value, decode_chunk, parse_blob_key, reference,
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
    blobs: BTreeMap<u64, Bytes>,
    /// Blobs a failed write may or may not have changed.
    unknown: BTreeSet<u64>,
    /// The root, unless a failed write left it unknown.
    root: Option<Root>,
    root_unknown: bool,
    violations: Vec<String>,
    /// The bases of files made with one, by name.
    bases: BTreeMap<String, Arc<dyn Base>>,
    checked: usize,
}

impl Known {
    fn violation(&mut self, message: String) {
        self.violations.push(message);
    }

    fn learn(&mut self, key: &str, value: Option<Bytes>) {
        if key == ROOT {
            self.root = value.and_then(|bytes| Root::decode(&bytes).ok());
            self.root_unknown = false;
            let next = self.root.as_ref().map_or(0, |root| root.next_blob);
            self.blobs.retain(|&n, _| n < next);
            self.unknown.retain(|&n| n < next);
        } else if let Some(n) = parse_blob_key(key) {
            self.unknown.remove(&n);
            match value {
                Some(value) => self.blobs.insert(n, value),
                None => self.blobs.remove(&n),
            };
        }
    }

    fn forget(&mut self, key: &str) {
        if key == ROOT {
            self.root_unknown = true;
        } else if let Some(n) = parse_blob_key(key) {
            self.unknown.insert(n);
        }
    }

    fn check_put(&mut self, key: &str, value: &Bytes) {
        self.checked += 1;
        if let Some(n) = parse_blob_key(key) {
            if let Some(root) = self.root.as_ref().filter(|_| !self.root_unknown)
                && n < root.next_blob
            {
                self.violation(format!("blob {n} put, below the root's next blob"));
            }
            return;
        }
        if key != ROOT {
            return self.violation(format!("a put of {key:?}, neither root nor blob"));
        }
        let root = match Root::decode(value) {
            Ok(root) => root,
            Err(error) => return self.violation(format!("a root that does not decode: {error}")),
        };
        let named = self.named(&root);
        if let Some(held) = self.root.clone().filter(|_| !self.root_unknown) {
            if root.version != held.version + 1
                || root.id != held.id
                || root.chunk_pages != held.chunk_pages
                || root.fanout != held.fanout
                || root.next_blob < held.next_blob
            {
                let (v, held_v) = (root.version, held.version);
                self.violation(format!("root {v} does not build on root {held_v}"));
            }
            let dropped = (self.named(&held).into_iter()).filter(|n| !named.contains(n));
            let kept = held
                .garbage
                .keys()
                .copied()
                .filter(|n| self.blobs.contains_key(n));
            let leaked: Vec<u64> = dropped
                .chain(kept)
                .filter(|n| !root.garbage.contains_key(n))
                .collect();
            for n in leaked {
                self.violation(format!("root {} leaks blob {n}", root.version));
            }
        }
        for n in named {
            if !self.blobs.contains_key(&n) && !self.unknown.contains(&n) {
                self.violation(format!("root {} names blob {n}, not written", root.version));
            }
        }
        if let Err(error) = self.decode(&root) {
            self.violation(format!("root {}: {error}", root.version));
        }
    }

    fn check_delete(&mut self, key: &str) {
        self.checked += 1;
        let Some(n) = parse_blob_key(key) else {
            return self.violation(format!("a delete of {key:?}, not a blob"));
        };
        if let Some(root) = self.root.as_ref().filter(|_| !self.root_unknown)
            && (self.named(root).contains(&n) || !root.garbage.contains_key(&n))
        {
            self.violation(format!("blob {n} deleted, not the root's garbage"));
        }
    }

    /// Walk `map`, of `shape`, through the node blobs known, decoding them
    /// as the walk needs them: what it meets, and the nodes that do not
    /// decode.
    fn walk(&self, map: &Map, shape: Shape) -> (Vec<Visit>, Vec<String>) {
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

    /// Every blob `root` names, through the map nodes known.
    fn named(&self, root: &Root) -> BTreeSet<u64> {
        let mut named = BTreeSet::new();
        for entry in root.directory.values() {
            match &entry.kind {
                Kind::Record(Value::Blob(n)) => {
                    named.insert(*n);
                }
                Kind::Record(Value::Inline(_)) => {}
                Kind::Paged { size, map, .. } => {
                    for visit in self.walk(map, root.shape(*size)).0 {
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
        named
    }

    /// Decode every chunk and record blob `root` names (those a failed write
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
            };
            let (met, errors) = self.walk(map, root.shape(bytes));
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
}
