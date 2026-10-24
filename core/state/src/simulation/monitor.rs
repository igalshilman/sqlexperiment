//! A protocol monitor: checks every request the code issues against the
//! rule of [`crate::store`]: a blob is written before the first root that
//! names it, and deleted only once no root names it.
//!
//! The store keeps writes in issue order, and a crash keeps a prefix of them,
//! so a write may rely on every write issued before it. The monitor wraps a
//! store and keeps the state those writes make: each write applied as it is
//! issued, and values it read (after a crash, a read learns what was kept).
//! A write that fails makes its key unknown until a read learns it. Each
//! write is checked when it is issued:
//!
//! 1. **Name only what is written**: a root names only blobs that are there.
//! 2. **Build on the root**: a root is the next version of the root, with
//!    the same chunk size and no blob number reused.
//! 3. **Never overwrite**: a blob is put only at a number the root has not
//!    allocated.
//! 4. **Delete only garbage**: a blob is deleted only if the root lists it
//!    as garbage, and names it no more.
//! 5. **Decodable**: every root decodes, and every chunk it names decodes to
//!    its pages: a base against its file's golden pages, a delta against its
//!    base.
use crate::{
    Base, PAGE,
    kv::KvStore,
    store::{Kind, ROOT, Root, parse_blob_key},
    zstd,
};
use bytes::Bytes;
use futures::future::BoxFuture;
use std::{
    collections::{BTreeMap, BTreeSet},
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
        if let Some(held) = self.root.as_ref().filter(|_| !self.root_unknown)
            && (root.version != held.version + 1
                || root.chunk_pages != held.chunk_pages
                || root.next_blob < held.next_blob)
        {
            let (v, held_v) = (root.version, held.version);
            self.violation(format!("root {v} does not build on root {held_v}"));
        }
        for n in root.named() {
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
            && (root.named().contains(&n) || !root.garbage.contains_key(&n))
        {
            self.violation(format!("blob {n} deleted, not the root's garbage"));
        }
    }

    /// Decode every chunk and record blob `root` names.
    fn decode(&self, root: &Root) -> io::Result<()> {
        let size = (root.chunk_pages * PAGE) as usize;
        let blob = |n: u64| self.blobs.get(&n);
        for entry in root.directory.values() {
            let Kind::Paged { chunks, base, .. } = &entry.kind else {
                continue;
            };
            let golden = self.bases.get(&entry.name);
            if base.is_some() && golden.is_none() {
                continue;
            }
            for (chunk, stored) in chunks.iter().enumerate() {
                let mut prefix = vec![0; size];
                if let Some(golden) = golden {
                    for (i, page) in prefix.chunks_mut(PAGE as usize).enumerate() {
                        if let Some(image) = golden.page(chunk as u64 * root.chunk_pages + i as u64)
                        {
                            page.copy_from_slice(image);
                        }
                    }
                }
                if stored.base.is_none() && stored.delta.is_some() {
                    return Err(io::Error::other(format!(
                        "chunk {chunk}: a delta without a base"
                    )));
                }
                if let Some(frame) = stored.base.and_then(blob) {
                    // Against the golden pages, if the file has them.
                    let reference = if golden.is_some() { &prefix[..] } else { &[] };
                    let mut pages = Vec::with_capacity(size);
                    zstd::decompress(&mut pages, frame, reference)?;
                    prefix = pages;
                }
                if let Some(frame) = stored.delta.and_then(blob) {
                    let mut pages = Vec::with_capacity(size);
                    zstd::decompress(&mut pages, frame, &prefix)?;
                    prefix = pages;
                }
                if prefix.len() != size {
                    return Err(io::Error::other(format!(
                        "chunk {chunk} of {} bytes",
                        prefix.len()
                    )));
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
