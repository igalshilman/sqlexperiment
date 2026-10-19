//! A protocol monitor: checks every request the code issues against the
//! invariants of `docs/DESIGN.md`, as `spec/Protocol.tla` states them.
//!
//! The store keeps writes in issue order, and a crash keeps a prefix of them,
//! so a write may rely on every write issued before it. The monitor wraps a
//! store and keeps the state those writes make: each write applied as it is
//! issued, and values it read (after a crash, a read learns what was kept).
//! A write that fails makes its key unknown, and adds its value to a page
//! key's possible values. Each write is checked against that state when it is
//! issued:
//!
//! 1. **Name only what is written**: a commit record names only stored
//!    segments written before it.
//! 2. **Build on the record**: a commit record holds the record's log plus
//!    one transaction, at the next version (its earlier transactions decode to
//!    the same images, inline or sealed); or is that record again.
//! 3. **Checkpoint before recording**: a record that empties the log (the next
//!    epoch) comes only once every page the log changed is at its page key,
//!    with the log's latest image of it.
//! 4. **Overwrite only what the log covers**: a page key gets only an image of
//!    a transaction in the record's log.
//! 5. **Fresh segments, deleted unnamed**: a stored segment is put only at a
//!    number the record has not allocated, and deleted only if it names it no
//!    more; the record lists as garbage only values the record before named.
//! 6. **Decodable**: every record put decodes, its log against the segments
//!    it names.
use crate::{
    format::{COMMIT, LOG_PREFIX, PAGE_PREFIX},
    kv::KvStore,
    log::{self, Commit, Images, Part},
    page_codec,
};
use bytes::Bytes;
use futures::future::BoxFuture;
use std::{
    collections::{BTreeMap, HashMap},
    io,
    ops::Deref,
    sync::Mutex,
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
    /// Stored segments, by number.
    segments: BTreeMap<u64, Bytes>,
    /// The values each page key may hold durably; `None` is a hole.
    pages: HashMap<u64, Vec<Option<Bytes>>>,
    commit: Option<Commit>,
    violations: Vec<String>,
    checked: usize,
}

impl Known {
    fn commit(&self) -> Commit {
        self.commit.clone().unwrap_or_default()
    }

    /// Each transaction's images in `commit`'s log, as replay decodes them;
    /// `None` when a segment it names is unknown or undecodable.
    fn transactions(&self, commit: &Commit) -> Option<Vec<Images>> {
        let mut chain = log::Chain::default();
        let mut transactions = Vec::new();
        for transaction in &commit.log {
            let mut images = Images::new();
            for part in &transaction.parts {
                let segment = match part {
                    Part::Inline(bytes) => bytes.clone(),
                    Part::Stored(n, index) => log::stored_segments(self.segments.get(n)?)
                        .ok()?
                        .get(*index as usize)?
                        .clone(),
                };
                images.extend(log::decode_segment(&segment, &chain).ok()?);
            }
            chain.images.extend(images.clone());
            transactions.push(images);
        }
        Some(transactions)
    }

    /// The values of `commit`'s chain; `None` when one it names is unknown.
    fn chain(&self, commit: &Commit) -> Option<Vec<Bytes>> {
        let value = |part: &Part| match part {
            Part::Inline(bytes) => Some(bytes.clone()),
            Part::Stored(n, index) => log::stored_segments(self.segments.get(n)?)
                .ok()?
                .get(*index as usize)
                .cloned(),
        };
        commit.chain.iter().map(value).collect()
    }

    /// The logical image a page key's value holds; `None` for a hole.
    fn image(value: &Option<Bytes>) -> Option<Option<Bytes>> {
        match value {
            Some(value) => page_codec::decode_bytes(value).ok().map(Some),
            None => Some(None),
        }
    }

    fn violation(&mut self, message: String) {
        self.violations.push(message);
    }

    /// Check a put before it is issued.
    fn check_put(&mut self, key: &str, value: &Bytes) {
        self.checked += 1;
        if let Some(n) = parse(key, LOG_PREFIX) {
            // 5: at a number not yet allocated
            if n < self.commit().next_segment {
                self.violation(format!("{key}: a segment number already allocated"));
            }
        } else if let Some(page) = parse(key, PAGE_PREFIX) {
            self.check_image(key, page, Some(value));
        } else if key == COMMIT {
            self.check_commit(value);
        }
    }

    fn check_commit(&mut self, value: &Bytes) {
        let Ok(commit) = Commit::decode(value) else {
            return self.violation("commit: undecodable".into());
        };
        let known = self.commit();
        // 1, 6: it names known segments, and its log decodes
        if self.transactions(&commit).is_none() || self.chain(&commit).is_none() {
            return self.violation(format!(
                "commit {}: names an unknown segment, or does not decode",
                commit.version
            ));
        }
        if commit == known {
            return;
        }
        let same = |a: &Commit, b: &Commit| {
            (a.size, a.next_segment, &a.effects) == (b.size, b.next_segment, &b.effects)
        };
        if commit.version == known.version + 1 {
            // 2: the known log plus one transaction (or none, for a commit
            // without SQL changes), whether its earlier transactions stay
            // inline or are sealed into a stored value; the known chain, plus
            // a value, restarted with one, or cleared
            let sizes = |c: &Commit| c.log.iter().map(|t| t.size).collect::<Vec<_>>();
            let earlier = self.transactions(&commit).map(|mut t| {
                t.truncate(known.log.len());
                t
            });
            let added = commit.log.len().checked_sub(known.log.len());
            let (chain, known_chain) = (self.chain(&commit), self.chain(&known));
            let chain_follows = match (chain, known_chain) {
                (Some(chain), Some(known)) => {
                    chain == known
                        || chain.len() <= 1
                        || (chain.len() == known.len() + 1 && chain[..known.len()] == known[..])
                }
                _ => false,
            };
            let (named, known_named) = (commit.stored_values(), known.stored_values());
            let extends = commit.epoch == known.epoch
                && (added == Some(1) || (added == Some(0) && commit.size == known.size))
                && sizes(&commit)[..known.log.len()] == sizes(&known)[..]
                && earlier.is_some()
                && earlier == self.transactions(&known)
                && chain_follows
                // garbage: values the known record named, and this one no more
                && commit
                    .garbage
                    .iter()
                    .all(|n| known_named.contains(n) && !named.contains(n));
            if !extends {
                self.violation(format!(
                    "commit {}: does not extend the known record",
                    commit.version
                ));
            }
        } else if commit.version == known.version
            && commit.epoch == known.epoch + 1
            && commit.log.is_empty()
            && same(&commit, &known)
            && self.chain(&commit) == self.chain(&known)
        {
            // 3: every page of the known log is known at its key
            let Some(known_log) = self.transactions(&known) else {
                return self.violation("checkpoint: the known log does not decode".into());
            };
            let mut latest = Images::new();
            for images in known_log {
                latest.extend(images);
            }
            for (page, image) in latest {
                let held = self.pages.get(&page).map_or(&[][..], Vec::as_slice);
                let ok = !held.is_empty()
                    && held
                        .iter()
                        .all(|value| Self::image(value) == Some(image.clone()));
                if !ok {
                    self.violation(format!(
                        "checkpoint to epoch {}: page {page} not known at its key",
                        commit.epoch
                    ));
                }
            }
        } else {
            self.violation(format!(
                "commit {} (epoch {}): neither a commit nor a checkpoint after {} (epoch {})",
                commit.version, commit.epoch, known.version, known.epoch
            ));
        }
    }

    /// 4: a page key gets an image of a transaction in the known log.
    fn check_image(&mut self, key: &str, page: u64, value: Option<&Bytes>) {
        let Some(value) = Self::image(&value.cloned()) else {
            return self.violation(format!("{key}: undecodable page"));
        };
        let known = self.commit();
        let covered = self
            .transactions(&known)
            .unwrap_or_default()
            .iter()
            .filter_map(|images| images.get(&page))
            .any(|image| *image == value);
        if !covered {
            self.violation(format!("{key}: image of no transaction in the known log"));
        }
    }

    fn check_delete(&mut self, key: &str) {
        self.checked += 1;
        if let Some(n) = parse(key, LOG_PREFIX) {
            // 5: only a segment the known record does not name
            if self.commit().stored_values().contains(&n) {
                self.violation(format!("{key}: deleted while the known record names it"));
            }
        } else if let Some(page) = parse(key, PAGE_PREFIX) {
            self.check_image(key, page, None);
        } else if key == COMMIT {
            self.violation("commit: deleted".into());
        }
    }

    /// Record a value written or read; `None`: absent.
    fn learn(&mut self, key: &str, value: Option<Bytes>) {
        if let Some(n) = parse(key, LOG_PREFIX) {
            match value {
                Some(value) => self.segments.insert(n, value),
                None => self.segments.remove(&n),
            };
        } else if let Some(page) = parse(key, PAGE_PREFIX) {
            self.pages.insert(page, vec![value]);
        } else if key == COMMIT {
            self.commit = value.and_then(|bytes| Commit::decode(&bytes).ok());
        }
    }

    /// A request with an unknown outcome: its key may hold either value.
    fn forget(&mut self, key: &str, value: Option<&Bytes>) {
        if let Some(n) = parse(key, LOG_PREFIX) {
            self.segments.remove(&n);
        } else if let Some(page) = parse(key, PAGE_PREFIX)
            && let Some(held) = self.pages.get_mut(&page)
        {
            held.push(value.cloned());
        }
        // A record may have moved forward or not: the known one is the older,
        // so checks against it stay conservative until a read learns which.
        // (A write fails only as its session ends.)
    }
}

fn parse(key: &str, prefix: &str) -> Option<u64> {
    u64::from_str_radix(key.strip_prefix(prefix)?, 16).ok()
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
}
