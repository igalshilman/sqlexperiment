//! A protocol monitor: checks every request the code issues to Restate's
//! state against the rules of [`crate::store`] and [`crate::tail`]: a tail
//! entry or a pack is stored before the first head that names it, and
//! dropped only once no head a state may hold needs it.
//!
//! The store keeps writes in issue order, and a crash keeps a prefix of them,
//! so a write may rely on every write issued before it. The monitor wraps a
//! store and keeps the state those writes make: each write applied as it is
//! issued, and values it read (after a crash, a read learns what was kept).
//! A write that fails makes its key unknown until a read learns it; a read
//! of the head makes the tail entries past it unknown, which a crash may
//! have lost. Each write is checked when it is issued:
//!
//! 1. **Name only what is stored**: a head's tail entries past its
//!    checkpoint are stored in full (those before, kept for readers that
//!    opened on the checkpoint before, a crashed flush may have deleted),
//!    and so is a new checkpoint: its manifest, and every range the
//!    manifest names (where the monitor sees the packs: in the store, or
//!    given [`MonitorKv::packs`]).
//! 2. **Build on the head**: a head is the next version, with the same
//!    identity and frame size, and a tail that starts no earlier; a new
//!    checkpoint is newer than the one held, which becomes its previous.
//! 3. **Never overwrite**: a tail entry is put only for the version after
//!    the head's, and a pack's part only once.
//! 4. **Drop only what no head needs**: a tail entry is deleted only once
//!    the head's checkpoint covers it, and a pack in the store only if
//!    older than the head's previous checkpoint, and neither checkpoint
//!    names it.
//! 5. **Decodable**: every head, tail entry and manifest decodes, and so
//!    does every page, frame and value they name: a file's pages and frames
//!    against its golden base (given [`MonitorKv::with_base`]).
use crate::{
    Base, PAGE,
    blobs::KV_PREFIX,
    format::{
        Checkpoint, Entry, HEAD_KEY, Head, Location, Manifest, PackId, Value, parse_tail_key,
    },
    kv::KvStore,
    simulation::MemoryBlobs,
    store::{base_page, decode_page, decode_value, implicit},
    zstd,
};
use bytes::Bytes;
use futures::future::BoxFuture;
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    io,
    ops::{Deref, Range},
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
    /// The parts of tail entries, by key.
    tail: BTreeMap<String, Bytes>,
    /// Tail parts a failed write, or a crash, may or may not have changed.
    unknown: BTreeSet<String>,
    /// The head, unless a failed write left it unknown.
    head: Option<Head>,
    head_unknown: bool,
    /// Packs in the store: their parts, by key.
    packs: BTreeMap<String, BTreeMap<u64, Bytes>>,
    /// Packs elsewhere ([`MonitorKv::packs`]).
    external: Option<MemoryBlobs>,
    manifests: HashMap<PackId, Option<Manifest>>,
    /// Ranges of packs already decoded.
    decoded: HashSet<Location>,
    /// Frames decoded, by location: `None` if they do not decode.
    frames: HashMap<Location, Option<Bytes>>,
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
                let head = value.and_then(|bytes| Head::decode(&bytes).ok());
                let version = head.as_ref().map_or(0, |head| head.version);
                self.head = head;
                self.head_unknown = false;
                let past = |key: &String| parse_tail_key(key).is_some_and(|(v, _)| v > version);
                let lost: Vec<String> = self.tail.keys().filter(|key| past(key)).cloned().collect();
                for key in lost {
                    self.tail.remove(&key);
                    self.unknown.insert(key);
                }
            }
            Key::Tail(_) => {
                self.unknown.remove(at);
                match value {
                    Some(value) => self.tail.insert(at.to_owned(), value),
                    None => self.tail.remove(at),
                };
            }
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

    fn forget(&mut self, at: &str) {
        match key(at) {
            Key::Head => self.head_unknown = true,
            Key::Tail(_) => {
                self.tail.remove(at);
                self.unknown.insert(at.to_owned());
            }
            Key::Pack(..) | Key::Other => {}
        }
    }

    /// The head, unless a failed write left it unknown.
    fn held(&self) -> Option<&Head> {
        self.head.as_ref().filter(|_| !self.head_unknown)
    }

    /// The bytes `range` of the pack `pack`, if the monitor sees it.
    fn pack_range(&self, pack: PackId, range: Range<u64>) -> Option<Bytes> {
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

    /// The manifest of `checkpoint`, if the monitor sees it.
    fn manifest(&mut self, checkpoint: &Checkpoint) -> Option<&Manifest> {
        if !self.manifests.contains_key(&checkpoint.pack) {
            let manifest = (self.pack_range(checkpoint.pack, checkpoint.range()))
                .and_then(|bytes| Manifest::decode(&bytes).ok());
            self.manifests.insert(checkpoint.pack, manifest);
        }
        self.manifests[&checkpoint.pack].as_ref()
    }

    fn check_put(&mut self, at: &str, value: &Bytes) {
        self.checked += 1;
        match key(at) {
            Key::Head => self.check_head(value),
            Key::Tail(version) => {
                let Some(head) = self.held() else {
                    return;
                };
                let expected = head.version + 1;
                if version != expected {
                    self.violation(format!("tail entry {version} put; the next is {expected}"));
                }
            }
            Key::Pack(pack, part) => {
                if self
                    .packs
                    .get(&pack)
                    .is_some_and(|parts| parts.contains_key(&part))
                {
                    self.violation(format!("part {part} of pack {pack} put again"));
                }
            }
            Key::Other => self.violation(format!("a put of {at:?}")),
        }
    }

    fn check_head(&mut self, value: &Bytes) {
        let head = match Head::decode(value) {
            Ok(head) => head,
            Err(error) => return self.violation(format!("a head that does not decode: {error}")),
        };
        let v = head.version;
        let held = self.held().cloned();
        if let Some(held) = &held {
            if head.version != held.version + 1
                || head.id != held.id
                || head.frame_pages != held.frame_pages
                || head.tail_first < held.tail_first
            {
                let held = held.version;
                self.violation(format!("head {v} does not build on head {held}"));
            }
            let new = head.checkpoint != held.checkpoint;
            if new
                && (head.previous != held.checkpoint || head.checkpointed() <= held.checkpointed())
            {
                self.violation(format!(
                    "head {v}: a new checkpoint, not after the one held"
                ));
            }
        }
        for version in head.checkpointed() + 1..=head.version {
            self.check_entry(&head, version);
        }
        let new = held.is_none_or(|held| head.checkpoint != held.checkpoint);
        if let Some(checkpoint) = head.checkpoint.filter(|_| new) {
            self.check_checkpoint(&head, &checkpoint);
        }
    }

    /// The tail entry `version` of `head` is stored, and decodes.
    fn check_entry(&mut self, head: &Head, version: u64) {
        let v = head.version;
        let mut parts = Vec::new();
        for key in head.entry_keys(version) {
            match self.tail.get(&key) {
                Some(part) => parts.push(part.clone()),
                None if self.unknown.contains(&key) => return,
                None => return self.violation(format!("head {v} names {key}, not stored")),
            }
        }
        let entry = match Entry::decode(&parts) {
            Ok(entry) => entry,
            Err(error) => return self.violation(format!("tail entry {version}: {error}")),
        };
        for (name, file) in &entry.files {
            let Some(base) = self.base(name, file.base) else {
                continue;
            };
            for (page, frame) in &file.pages {
                let Some(frame) = frame else {
                    continue;
                };
                let Some(reference) = self.reference(head, name, *page, base.as_ref()) else {
                    continue;
                };
                if let Err(error) = decode_page(frame, &reference) {
                    let error = format!("tail entry {version}: page {page} of {name}: {error}");
                    return self.violation(error);
                }
            }
        }
    }

    /// What a tail entry of `head` compresses page `page` of the file
    /// `name` against: the page in its checkpoint's frame, else its base's
    /// page; `None` where the monitor does not see the checkpoint.
    fn reference(
        &mut self,
        head: &Head,
        name: &str,
        page: u64,
        base: Option<&Arc<dyn Base>>,
    ) -> Option<Bytes> {
        let based = || Bytes::copy_from_slice(base_page(base, page));
        let Some(checkpoint) = head.checkpoint else {
            return Some(based());
        };
        let index = page / head.frame_pages;
        let stored = (self.manifest(&checkpoint)?.files.get(name))
            .and_then(|file| file.frames.get(index as usize).copied().flatten());
        let Some(at) = stored else {
            return Some(based());
        };
        let frame = self.frame(head.frame_pages, at, base, index)?;
        let at = ((page % head.frame_pages) * PAGE) as usize;
        Some(frame.slice(at..at + PAGE as usize))
    }

    /// Frame `index` of a file with `base`, stored at `at`, decoded; `None`
    /// if it is not stored or does not decode.
    fn frame(
        &mut self,
        frame_pages: u64,
        at: Location,
        base: Option<&Arc<dyn Base>>,
        index: u64,
    ) -> Option<Bytes> {
        if let Some(frame) = self.frames.get(&at) {
            return frame.clone();
        }
        let size = (frame_pages * PAGE) as usize;
        let frame = self.pack_range(at.pack, at.range()).and_then(|raw| {
            let prefix = match base {
                Some(_) => implicit(base, frame_pages, index),
                None => Vec::new(),
            };
            let mut image = Vec::with_capacity(size);
            zstd::decompress(&mut image, &raw, &prefix).ok()?;
            (image.len() == size).then(|| Bytes::from(image))
        });
        self.frames.insert(at, frame.clone());
        frame
    }

    /// The base the file `name` is made with, if the monitor knows it:
    /// `Some(None)` without one.
    fn base(&mut self, name: &str, id: Option<u64>) -> Option<Option<Arc<dyn Base>>> {
        let Some(id) = id else {
            return Some(None);
        };
        let base = self.bases.get(name)?.clone();
        if base.id() != id {
            self.violation(format!("{name} is made with base {id}, not {}", base.id()));
            return None;
        }
        Some(Some(base))
    }

    /// A new checkpoint's manifest is stored and decodes, and so does every
    /// range it names.
    fn check_checkpoint(&mut self, head: &Head, checkpoint: &Checkpoint) {
        let (v, frame_pages) = (head.version, head.frame_pages);
        let Some(manifest) = self.manifest(checkpoint).cloned() else {
            return self.violation(format!("head {v}: its checkpoint's manifest is unreadable"));
        };
        for (name, file) in &manifest.files {
            let base = self.base(name, file.base);
            for (index, at) in file.frames.iter().enumerate() {
                let what = format!("frame {index} of {name}");
                let (Some(at), Some(base)) = (at, &base) else {
                    continue;
                };
                if self.stored(v, *at, &what).is_none() {
                    continue;
                }
                if self
                    .frame(frame_pages, *at, base.as_ref(), index as u64)
                    .is_none()
                {
                    self.violation(format!("head {v}: {what} does not decode"));
                }
            }
        }
        let records =
            (manifest.records.iter()).map(|(name, value)| (format!("record {name}"), value));
        let logs = manifest.logs.iter().flat_map(|(name, log)| {
            let first = log.first;
            (log.values.iter().enumerate())
                .map(move |(i, value)| (format!("entry {} of log {name}", first + i as u64), value))
        });
        for (what, value) in records.chain(logs) {
            let Value::Stored(at) = value else {
                continue;
            };
            if let Some(raw) = self.stored(v, *at, &what)
                && let Err(error) = decode_value(&raw)
            {
                self.violation(format!("head {v}: {what}: {error}"));
            }
        }
    }

    /// The bytes at `at`, the first time a head names them; a violation if
    /// they are not stored.
    fn stored(&mut self, v: u64, at: Location, what: &str) -> Option<Bytes> {
        if !self.decoded.insert(at) {
            return None;
        }
        let raw = self.pack_range(at.pack, at.range());
        if raw.is_none() {
            let pack = at.pack.key();
            self.violation(format!("head {v}: {what} is in {pack}, not stored"));
        }
        raw
    }

    fn check_delete(&mut self, at: &str) {
        self.checked += 1;
        let Some(head) = self.held().cloned() else {
            return;
        };
        match key(at) {
            Key::Tail(version) => {
                if version > head.checkpointed() {
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
                let older = head.previous.is_some_and(|p| id.version < p.version());
                if !older || needed.contains(&id) {
                    self.violation(format!("pack {pack} deleted, still needed"));
                }
            }
            Key::Head | Key::Other => self.violation(format!("a delete of {at:?}")),
        }
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
            // The head last: it forgets the tail entries past it.
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

    /// Decode the pages and frames of the file `name` against `base`, the
    /// one it is made with (see [`crate::Base`]).
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

    fn keys(&self) -> BoxFuture<'_, io::Result<Vec<String>>> {
        self.inner.keys()
    }
}
