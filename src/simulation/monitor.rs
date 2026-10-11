//! A protocol monitor: checks every request the code issues against the
//! invariants of `docs/DESIGN.md`, as `spec/Protocol.tla` states them.
//!
//! The monitor wraps a store and keeps what an observer knows is durable: the
//! values of acknowledged puts and deletes (and, on a store whose visible
//! values are durable, values it read). A failed or cancelled request makes a
//! log key unknown, and adds its value to a page key's possible values. Each
//! put or delete is then checked when it is issued:
//!
//! 1. **Commit**: a commit part is sent only once the transaction's earlier
//!    parts are known.
//! 2. **Build on durable**: a transaction starts where a known commit ends (or
//!    at the known log start), unless it re-puts a known segment.
//! 3. **Checkpoint before recording**: `meta` passes a transaction only when
//!    every page it changed is known at its page key, with that image or a
//!    newer committed one.
//! 4. **Delete behind a durable meta**: a segment is deleted only below the
//!    known log start.
//! 5. **Overwrite only what the log covers**: a page key gets only an image of
//!    a known committed transaction at or after the known log start.
use crate::{
    format::{LOG_PREFIX, META, PAGE_PREFIX},
    kv::KvStore,
    log::{self, Images, Meta},
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

/// What is known to be durable.
#[derive(Default)]
struct Known {
    log: BTreeMap<u64, Bytes>,
    /// The values each page key may hold durably; `None` is a hole.
    pages: HashMap<u64, Vec<Option<Bytes>>>,
    meta: Option<Meta>,
    violations: Vec<String>,
    checked: usize,
}

/// A committed transaction among known segments.
struct Transaction {
    end: u64,
    images: Images,
}

impl Known {
    fn meta(&self) -> Meta {
        self.meta.unwrap_or_default()
    }

    fn segment(&self, seq: u64) -> Option<log::Segment> {
        self.log
            .get(&seq)
            .and_then(|bytes| log::decode(bytes, seq).ok())
    }

    /// Known committed transactions from `start`, in order.
    fn committed(&self, start: u64) -> Vec<Transaction> {
        let mut transactions = Vec::new();
        let (mut seq, mut first, mut images) = (start, start, Images::new());
        while let Some(segment) = self.segment(seq) {
            if segment.first != first {
                break;
            }
            images.extend(segment.images);
            seq += 1;
            if segment.commit {
                transactions.push(Transaction {
                    end: seq,
                    images: std::mem::take(&mut images),
                });
                first = seq;
            }
        }
        transactions
    }

    fn violation(&mut self, message: String) {
        self.violations.push(message);
    }

    /// Check a put before it is issued.
    fn check_put(&mut self, key: &str, value: &Bytes) {
        self.checked += 1;
        if let Some(seq) = parse(key, LOG_PREFIX) {
            let Ok(segment) = log::decode(value, seq) else {
                return self.violation(format!("{key}: undecodable segment"));
            };
            if self.log.get(&seq) == Some(value) {
                return; // a re-put of a known value: confirmation
            }
            // 1: earlier parts of its transaction are known
            for part in segment.first..seq {
                if self
                    .segment(part)
                    .is_none_or(|known| known.first != segment.first)
                {
                    self.violation(format!(
                        "{key}: part {part} of its transaction is not known"
                    ));
                }
            }
            // 2: a new transaction starts where a known commit ends
            if segment.first == seq {
                let previous_commit = seq > 1 && self.segment(seq - 1).is_some_and(|s| s.commit);
                if seq != self.meta().log_start && !previous_commit {
                    self.violation(format!("{key}: starts after no known commit"));
                }
            }
        } else if let Some(page) = parse(key, PAGE_PREFIX) {
            self.check_image(key, page, Some(value));
        } else if key == META {
            let Ok(meta) = Meta::decode(value) else {
                return self.violation(format!("{key}: undecodable"));
            };
            let known = self.meta();
            if meta.log_start <= known.log_start {
                return;
            }
            // 3: every page of the passed transactions is known at its key
            let transactions = self.committed(known.log_start);
            let Some(last) = transactions.iter().position(|t| t.end == meta.log_start) else {
                return self.violation(format!(
                    "meta to {}: not at a known commit after {}",
                    meta.log_start, known.log_start
                ));
            };
            let mut changed: Vec<u64> = transactions[..=last]
                .iter()
                .flat_map(|t| t.images.keys().copied())
                .collect();
            changed.sort_unstable();
            changed.dedup();
            for page in changed {
                // the image of the last passed transaction to change it, or newer
                let from = (0..=last)
                    .rev()
                    .find(|&i| transactions[i].images.contains_key(&page))
                    .expect("changed by some transaction");
                let allowed: Vec<&Option<Bytes>> = transactions[from..]
                    .iter()
                    .filter_map(|t| t.images.get(&page))
                    .collect();
                let held = self.pages.get(&page).map_or(&[][..], Vec::as_slice);
                if held.is_empty() || !held.iter().all(|value| allowed.contains(&value)) {
                    self.violation(format!(
                        "meta to {}: page {page} not known at its key",
                        meta.log_start
                    ));
                }
            }
        }
    }

    /// 5: a page key gets an image of a known committed transaction still in
    /// the log.
    fn check_image(&mut self, key: &str, page: u64, value: Option<&Bytes>) {
        let covered = self
            .committed(self.meta().log_start)
            .iter()
            .filter_map(|t| t.images.get(&page))
            .any(|image| image.as_ref() == value);
        if !covered {
            self.violation(format!("{key}: image of no known committed transaction"));
        }
    }

    fn check_delete(&mut self, key: &str) {
        self.checked += 1;
        if let Some(seq) = parse(key, LOG_PREFIX) {
            // 4: only below the known log start
            if seq >= self.meta().log_start {
                self.violation(format!("{key}: deleted at or after the known log start"));
            }
        } else if let Some(page) = parse(key, PAGE_PREFIX) {
            self.check_image(key, page, None);
        }
    }

    /// Record a value now known durable; `None`: known absent.
    fn learn(&mut self, key: &str, value: Option<Bytes>) {
        if let Some(seq) = parse(key, LOG_PREFIX) {
            match value {
                Some(value) => self.log.insert(seq, value),
                None => self.log.remove(&seq),
            };
        } else if let Some(page) = parse(key, PAGE_PREFIX) {
            self.pages.insert(page, vec![value]);
        } else if key == META {
            self.meta = value.and_then(|bytes| Meta::decode(&bytes).ok());
        }
    }

    /// A request with an unknown outcome: its key may hold either value.
    fn forget(&mut self, key: &str, value: Option<&Bytes>) {
        if let Some(seq) = parse(key, LOG_PREFIX) {
            self.log.remove(&seq);
        } else if let Some(page) = parse(key, PAGE_PREFIX)
            && let Some(held) = self.pages.get_mut(&page)
        {
            held.push(value.cloned());
        }
        // A meta may have moved forward or not: the known one is the older, so
        // checks against it stay conservative.
    }
}

fn parse(key: &str, prefix: &str) -> Option<u64> {
    u64::from_str_radix(key.strip_prefix(prefix)?, 16).ok()
}

/// Forgets a key whose request did not complete.
struct Outstanding<'a> {
    known: &'a Mutex<Known>,
    key: &'a str,
    value: Option<Bytes>,
    done: bool,
}

impl Drop for Outstanding<'_> {
    fn drop(&mut self) {
        if !self.done {
            self.known
                .lock()
                .unwrap()
                .forget(self.key, self.value.as_ref());
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
            if self.inner.durable_when_visible() {
                self.known.lock().unwrap().learn(key, value.clone());
            }
            Ok(value)
        })
    }

    fn put<'a>(&'a self, key: &'a str, value: Bytes) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(async move {
            self.known.lock().unwrap().check_put(key, &value);
            let mut outstanding = Outstanding {
                known: &self.known,
                key,
                value: Some(value.clone()),
                done: false,
            };
            let result = self.inner.put(key, value.clone()).await;
            outstanding.done = true;
            let mut known = self.known.lock().unwrap();
            match &result {
                Ok(()) => known.learn(key, Some(value)),
                Err(_) => known.forget(key, Some(&value)),
            }
            result
        })
    }

    fn delete<'a>(&'a self, key: &'a str) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(async move {
            self.known.lock().unwrap().check_delete(key);
            let mut outstanding = Outstanding {
                known: &self.known,
                key,
                value: None,
                done: false,
            };
            let result = self.inner.delete(key).await;
            outstanding.done = true;
            let mut known = self.known.lock().unwrap();
            match &result {
                Ok(()) => known.learn(key, None),
                Err(_) => known.forget(key, None),
            }
            result
        })
    }

    fn durable_when_visible(&self) -> bool {
        self.inner.durable_when_visible()
    }
}
