//! Committed state: the commit record, the log it holds replayed over the
//! checkpointed pages, and the save that extends it.
//!
//! # Reads
//!
//! Page *n* is the latest committed image of *n* in the replayed log, otherwise
//! `page/<n>` (absent means a zero-filled hole). There is no page map: a cold
//! read is one get.
//!
//! # Save
//!
//! A save commits its changed pages as one transaction (see [`crate::log`]).
//! It queues its writes in this order, then one sync makes them durable:
//!
//! 1. **Garbage:** deletes of the stored values the durable record lists as
//!    garbage (it names them no more).
//! 2. **Stored values:** a transaction larger than the record's inline budget
//!    ([`StorageOptions::inline_log_bytes`]), a large chain value, or a large
//!    record, goes to stored values; past the budget, the record's inline log
//!    and chain parts are sealed into one.
//! 3. **Commit:** the record: the log so far plus the new transaction, naming
//!    those values, and listing as garbage the ones it names no more.
//! 4. **Checkpoint:** once more than [`StorageOptions::checkpoint_after`]
//!    transactions or [`StorageOptions::checkpoint_log_bytes`] are in the log,
//!    the latest image of every page it changed to `page/<n>` (holes are
//!    deletes), the working-set hint, and a record with an empty log and the
//!    next epoch, whose garbage takes the log's stored values.
//!
//! The store keeps writes in issue order, and a crash keeps a prefix of them
//! (see [`crate::kv`]). So a record is stored only with every value it names,
//! a checkpoint record only with the pages it moves past, and a delete only
//! with the record that stopped naming its value. A crash at any point leaves
//! a record whose log replays over whatever pages it kept: a checkpoint
//! overwrites only pages of the log of the record before it. There are no
//! object IDs, no intent and no orphans: pages are overwritten, never
//! allocated.
//!
//! # Open
//!
//! Read the commit record, then the stored segments it names, if any, and
//! replay its log. With a cache that saw the same record, nothing changed
//! since. A cold open (no cached pages) fetches a small database whole, or the
//! pages the working-set hint lists, alongside.
use crate::{
    cache::{NamespaceCache, PageKey, Reader},
    format::{COMMIT, HINT, log_key, page_key},
    hint::{self, Usage},
    kv::KvStore,
    log::{self, Commit, Entry, Images, Log, Logged, Part, Records, Sealed},
    page_codec::{self, Page, PageCompression},
};
use bytes::Bytes;
use std::{
    collections::{BTreeMap, HashMap},
    io,
    sync::{Arc, Mutex},
};

/// Logical page replacements from the session; `None` is a hole.
type Changes = BTreeMap<u64, Option<Page>>;

/// Default [`StorageOptions::prefetch_pages`].
pub const PREFETCH_PAGES: u64 = 16;
/// Default [`StorageOptions::hint_pages`].
pub const HINT_PAGES: usize = 32;
/// Default [`StorageOptions::max_private_bytes`].
pub const MAX_PRIVATE_BYTES: usize = 256 << 20;
/// Default [`StorageOptions::max_result_bytes`].
pub const MAX_RESULT_BYTES: usize = 64 << 20;
const PAGE: u64 = crate::io::BLOCK_SIZE as u64;
/// Consecutive page reads that start sequential readahead.
const READAHEAD_AFTER: u32 = 3;
/// Largest readahead window, in pages.
const MAX_READAHEAD: u64 = 64;
/// Default [`StorageOptions::checkpoint_after`].
pub const CHECKPOINT_AFTER: u32 = 32;
/// Default [`StorageOptions::checkpoint_log_bytes`].
pub const CHECKPOINT_LOG_BYTES: usize = 64 * 1024;
/// Default [`StorageOptions::inline_log_bytes`].
pub const INLINE_LOG_BYTES: usize = 1024;

/// A change to the host's chain, committed with a transaction
/// ([`crate::Session::commit_turn`]). The chain is the host's: a sequence of
/// opaque values that each apply over the ones before (a VM's page batches,
/// say), committed in the record with the SQL, kept by checkpoints, and
/// restarted when the host chooses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChainUpdate {
    /// Add a value at the end.
    Append(Bytes),
    /// Replace the chain with this value: it no longer needs the ones before.
    Restart(Bytes),
    /// Empty the chain.
    Clear,
}

/// When a session's SQL changes become durable.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Durability {
    /// At [`crate::Session::save`], as one log transaction for the whole
    /// session. SQL `COMMIT` is private, and dropping the session discards it.
    #[default]
    Save,
    /// At every SQL commit: a statement that ends a write transaction (an
    /// autocommit statement or `COMMIT`) returns once it is durable, one log
    /// transaction per SQL transaction. Dropping the session discards only an
    /// open transaction.
    Commit,
}

/// Per-session storage options. They never change the stored format.
#[derive(Clone)]
pub struct StorageOptions {
    /// Encoding of newly written pages; stored tags decide how pages are read.
    pub page_compression: PageCompression,
    /// This namespace's handle on the process-wide [`crate::PageCache`].
    pub cache: Option<NamespaceCache>,
    /// Transactions kept in the log: a save that logs more checkpoints the
    /// whole log. Replay at open reads them all.
    pub checkpoint_after: u32,
    /// Encoded bytes kept in the log: a save that logs more checkpoints the
    /// whole log.
    pub checkpoint_log_bytes: usize,
    /// Bytes of log the commit record holds inline, which every commit
    /// rewrites. A transaction that takes more goes to stored values, one per
    /// segment. Once the inline log would pass it, a commit seals the inline
    /// parts into one stored value, which the record names.
    pub inline_log_bytes: usize,
    /// A cold open (no warm cache pages) fetches every page of a database of
    /// at most this many pages, alongside replay.
    pub prefetch_pages: u64,
    /// Pages a checkpointing save lists in the working-set hint, which a cold
    /// open of a larger database fetches alongside replay (at most
    /// [`crate::MAX_HINT_PAGES`]); 0 neither writes nor reads it.
    pub hint_pages: usize,
    /// When SQL changes become durable.
    pub durability: Durability,
    /// Most bytes of uncommitted changes and private engine WAL a session may
    /// hold; a write beyond it fails its statement.
    pub max_private_bytes: usize,
    /// Most bytes of result rows one statement may collect; beyond it the
    /// statement fails.
    pub max_result_bytes: usize,
}

impl Default for StorageOptions {
    fn default() -> Self {
        Self {
            page_compression: PageCompression::default(),
            cache: None,
            checkpoint_after: CHECKPOINT_AFTER,
            checkpoint_log_bytes: CHECKPOINT_LOG_BYTES,
            inline_log_bytes: INLINE_LOG_BYTES,
            prefetch_pages: PREFETCH_PAGES,
            hint_pages: HINT_PAGES,
            durability: Durability::Save,
            max_private_bytes: MAX_PRIVATE_BYTES,
            max_result_bytes: MAX_RESULT_BYTES,
        }
    }
}

/// `Ok` means the transaction is durable, with this save's checkpoint, if any.
#[derive(Debug, Default)]
pub struct SaveReport {
    /// Pages written in place by this save's checkpoint; 0 when it did not
    /// checkpoint.
    pub checkpointed_pages: usize,
    /// The database's version after this save.
    pub(crate) version: u64,
}

/// The committed view a session reads and extends.
pub(crate) struct Snapshot {
    store: Arc<dyn KvStore>,
    options: StorageOptions,
    /// The durable commit record, and its exact bytes (`None` when absent).
    commit: Commit,
    commit_bytes: Option<Bytes>,
    pub(crate) log: Log,
    /// The host's chain, each value's bytes.
    chain: Vec<Bytes>,
    /// The records' values, by name.
    records: Records,
    /// Nothing changed since this owner last saw the namespace: the cache
    /// saw the same record, at this owner's last commit.
    pub(crate) unchanged: bool,
    /// The last decoded page, for repeated reads of one page.
    page: Mutex<Option<(u64, Page)>>,
    /// Page values fetched ahead of use (prefetch and readahead), each used once.
    fetched: Mutex<HashMap<u64, Fetched>>,
    readahead: Mutex<Readahead>,
    /// The working-set hint read at open, and the pages this session read.
    hint: Option<Bytes>,
    usage: Mutex<Usage>,
    /// This view's registration with the owner cache while a session uses it.
    reader: Mutex<Option<Reader>>,
}

/// A stored page value, with the cache key taken before it was read (see
/// [`NamespaceCache::insert_read`]).
type Fetched = (Option<PageKey>, Option<Bytes>);

/// Detects sequential reads of page keys.
#[derive(Default)]
struct Readahead {
    last: Option<u64>,
    run: u32,
    window: u64,
}

impl Readahead {
    /// Record a page read from the backend or from pages fetched ahead.
    fn observe(&mut self, index: u64) {
        self.run = match self.last {
            Some(last) if last + 1 == index => self.run + 1,
            _ => 1,
        };
        self.last = Some(index);
        if self.run < READAHEAD_AFTER {
            self.window = 0;
        }
    }

    /// Pages to read ahead with a get that missed; doubles while reads stay
    /// sequential.
    fn window(&mut self) -> u64 {
        if self.run < READAHEAD_AFTER {
            return 0;
        }
        self.window = (self.window * 2).clamp(4, MAX_READAHEAD);
        self.window
    }
}

impl Snapshot {
    pub async fn open_with_options(
        store: Arc<dyn KvStore>,
        options: StorageOptions,
    ) -> io::Result<Self> {
        let cache = options.cache.as_ref();
        // An owner that knows nothing reads the working-set hint with the record.
        let read_hint = async {
            match cache.is_none_or(|cache| !cache.knows_pages()) && options.hint_pages > 0 {
                true => store.get(HINT).await,
                false => Ok(None),
            }
        };
        let (commit_bytes, hint) = futures::try_join!(store.get(COMMIT), read_hint)?;
        let commit = match &commit_bytes {
            Some(bytes) => Commit::decode(bytes)?,
            None => Commit::default(),
        };
        let known = cache.is_some_and(|c| c.validate(commit_bytes.as_deref(), commit.epoch));
        // Without warm pages, fetch a small database whole, or the pages the
        // working-set hint lists, alongside the log's stored segments.
        let pages = commit.size.div_ceil(PAGE);
        let cold = cache.is_none_or(|cache| !cache.knows_epoch(commit.epoch));
        let logged = commit.inline_pages();
        let wanted: Vec<u64> = match (cold, &hint) {
            (false, _) => Vec::new(),
            _ if pages <= options.prefetch_pages => (0..pages).collect(),
            (true, Some(hint)) => hint::decode(hint, options.hint_pages).unwrap_or_default(),
            (true, None) => Vec::new(),
        };
        let wanted: Vec<u64> = wanted.into_iter().filter(|n| !logged.contains(n)).collect();
        let prefetch = async {
            let tokens: Vec<_> = wanted.iter().map(|&n| cache.map(|c| c.token(n))).collect();
            let keys: Vec<_> = wanted.iter().copied().map(page_key).collect();
            let values = futures::future::try_join_all(keys.iter().map(|k| store.get(k))).await?;
            let fetched = tokens.into_iter().zip(values);
            Ok::<_, io::Error>(wanted.iter().copied().zip(fetched).collect::<Vec<_>>())
        };
        let named = commit.stored_values();
        let held = cache.map_or_else(log::Values::new, |cache| cache.values(&named));
        let (replayed, prefetched_pages) =
            futures::try_join!(log::replay(&*store, &commit, held), prefetch)?;
        let log::Replayed {
            log,
            chain,
            records,
            values,
        } = replayed;
        if let Some(cache) = cache {
            cache.keep_values(&named, values);
        }
        let unchanged = known && cache.and_then(NamespaceCache::committed) == Some(commit.version);
        if let Some(cache) = cache {
            let committed = (!known).then_some(commit.version);
            cache.record(commit_bytes.clone(), commit.epoch, committed);
        }
        let reader = cache.map(|cache| cache.reader(commit.version));
        Ok(Self {
            store,
            options,
            commit,
            commit_bytes,
            log,
            chain,
            records,
            unchanged,
            page: Mutex::default(),
            fetched: Mutex::default(),
            readahead: Mutex::default(),
            hint,
            usage: Mutex::default(),
            reader: Mutex::new(reader),
        }
        .with_fetched(prefetched_pages))
    }

    /// Whether the store still holds the state this snapshot opened on, as far
    /// as its reads go: the commit record unchanged. A checkpoint overwrites
    /// only pages of the log a record already holds (which this snapshot
    /// replays over them, if it is the same record), and then moves the
    /// record, so if it is unchanged, every page this snapshot read was the
    /// one it opened on. A reader that is not the owner (alongside it, without
    /// a cache) checks this once it has read.
    pub(crate) async fn is_current(&self) -> io::Result<bool> {
        Ok(self.store.get(COMMIT).await? == self.commit_bytes)
    }

    /// Keep page values fetched at open, except pages the log overrides.
    fn with_fetched(self, pages: Vec<(u64, Fetched)>) -> Self {
        let mut fetched = self.fetched.lock().unwrap();
        for (index, (token, value)) in pages {
            if self.log.images.contains_key(&index) {
                continue;
            }
            // The cache keeps values; holes, and pages without a cache, stay here.
            if let (Some(cache), Some(token), Some(value)) = (&self.options.cache, &token, &value) {
                match cache.is_decoded() {
                    true => {
                        if let Ok(page) = page_codec::decode_bytes(value) {
                            cache.insert_read(token.clone(), page);
                            continue;
                        }
                    }
                    false => {
                        cache.insert_read(token.clone(), value.clone());
                        continue;
                    }
                }
            }
            fetched.insert(index, (token, value));
        }
        drop(fetched);
        self
    }

    pub(crate) fn options(&self) -> &StorageOptions {
        &self.options
    }

    /// The database's version: the transactions ever committed.
    pub fn version(&self) -> u64 {
        self.commit.version
    }

    /// Logical database size after replaying the log.
    pub fn size(&self) -> u64 {
        self.commit.size
    }

    /// The records' values, by name (see [`crate::Session::record`]).
    pub fn records(&self) -> &Records {
        &self.records
    }

    /// The host's chain, each value's bytes, in order.
    pub fn chain(&self) -> &[Bytes] {
        &self.chain
    }

    /// Decode a logical page; `None` means a hole. The log's latest image wins,
    /// then the cache, then the checkpointed value at `page/<n>`.
    pub async fn block(&self, index: u64) -> io::Result<Option<Page>> {
        if let Some((cached, page)) = &*self.page.lock().unwrap()
            && *cached == index
        {
            return Ok(Some(page.clone()));
        }
        let cache = self.options.cache.as_ref();
        let page = if let Some(image) = self.log.images.get(&index) {
            let Some(image) = image else { return Ok(None) };
            image.clone()
        } else if let Some((cache, value)) = match cache {
            Some(cache) => cache.get(index).await.map(|value| (cache, value)),
            None => None,
        } {
            match cache.is_decoded() {
                true => value,
                false => page_codec::decode_bytes(&value)?,
            }
        } else {
            let (token, Some(image)) = self.fetch(index).await? else {
                return Ok(None);
            };
            let page = page_codec::decode_bytes(&image)?;
            if let (Some(cache), Some(token)) = (cache, token) {
                let value = if cache.is_decoded() {
                    page.clone()
                } else {
                    image
                };
                cache.insert_read(token, value);
            }
            page
        };
        match cache {
            Some(cache) => cache.record_use(index, &page),
            None => self.usage.lock().unwrap().record(index, &page),
        }
        *self.page.lock().unwrap() = Some((index, page.clone()));
        Ok(Some(page))
    }

    /// The stored value of a page outside the log: fetched ahead, or one get.
    /// Sequential gets start reading ahead, in the same round trip.
    async fn fetch(&self, index: u64) -> io::Result<Fetched> {
        self.readahead.lock().unwrap().observe(index);
        if let Some(value) = self.fetched.lock().unwrap().remove(&index) {
            return Ok(value);
        }
        let ahead = self.readahead.lock().unwrap().window();
        let last = self.size().div_ceil(PAGE);
        let ahead: Vec<u64> = {
            let fetched = self.fetched.lock().unwrap();
            (index + 1..(index + 1 + ahead).min(last))
                .filter(|n| !self.log.images.contains_key(n) && !fetched.contains_key(n))
                .collect()
        };
        let pages: Vec<_> = std::iter::once(index).chain(ahead).collect();
        let cache = self.options.cache.as_ref();
        let tokens: Vec<_> = pages.iter().map(|&n| cache.map(|c| c.token(n))).collect();
        let keys: Vec<_> = pages.iter().copied().map(page_key).collect();
        let values = futures::future::try_join_all(keys.iter().map(|k| self.store.get(k))).await?;
        let mut values = tokens.into_iter().zip(values);
        let value = values.next().expect("the page itself");
        let mut fetched = self.fetched.lock().unwrap();
        fetched.extend(pages[1..].iter().copied().zip(values));
        Ok(value)
    }

    /// The working-set hint a checkpointing save writes, unless it is unchanged
    /// or empty.
    fn hint(&self) -> Option<Vec<u8>> {
        let limit = self.options.hint_pages;
        if limit == 0 {
            return None;
        }
        let hint = match &self.options.cache {
            Some(cache) => cache.hint(limit)?,
            None => {
                let hint = hint::encode(&self.usage.lock().unwrap().top(limit));
                (self.hint.as_deref() != Some(&hint[..])).then_some(hint)?
            }
        };
        (hint::decode(&hint, limit).is_some_and(|pages| !pages.is_empty())).then_some(hint)
    }

    /// Whether the log is long enough to checkpoint.
    fn wants_checkpoint(&self) -> bool {
        !self.log.transactions.is_empty() && self.too_long(0, 0)
    }

    /// Whether the log, with `transactions` more of `bytes`, passes a
    /// checkpoint threshold.
    fn too_long(&self, transactions: usize, bytes: usize) -> bool {
        let log = &self.log;
        log.transactions.len() + transactions > self.options.checkpoint_after as usize
            || log.bytes + bytes > self.options.checkpoint_log_bytes
    }

    /// Commit `dirty` pages as one transaction and advance checkpointing.
    ///
    /// Requires exclusive ownership until every request settles, even on
    /// cancellation. After an error, reopen; never reuse this snapshot.
    #[cfg_attr(not(test), allow(dead_code))]
    pub async fn publish(&self, dirty: Changes, size: u64) -> io::Result<SaveReport> {
        Ok(self.commit(dirty, size, Records::new(), None).await?.0)
    }

    /// [`Self::publish`], also returning the committed view afterwards, which a
    /// session may continue on. This snapshot is not used again. Each of
    /// `records` replaces that record's value, and with `chain`, the host's
    /// chain changes, in the same put. Without SQL changes the log gains no
    /// transaction; without any change nothing is committed. A commit that
    /// leaves the log long also checkpoints it, before the one sync.
    pub(crate) async fn commit(
        &self,
        dirty: Changes,
        size: u64,
        mut records: Records,
        chain: Option<ChainUpdate>,
    ) -> io::Result<(SaveReport, Self)> {
        // A record never set reads as empty.
        records.retain(|name, value| {
            self.records
                .get(name)
                .map_or(!value.is_empty(), |v| v != value)
        });
        if let Some((name, value)) = records
            .iter()
            .find(|(_, v)| v.len() > log::MAX_RECORD_BYTES)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "record {name:?} of {} bytes exceeds {}",
                    value.len(),
                    log::MAX_RECORD_BYTES
                ),
            ));
        }
        if let Some(name) = records.keys().find(|name| name.len() > log::MAX_NAME_BYTES) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("record name {name:?} exceeds {} bytes", log::MAX_NAME_BYTES),
            ));
        }
        let changed = !dirty.is_empty() || size != self.size();
        if !changed && records.is_empty() && chain.is_none() {
            let report = SaveReport {
                version: self.version(),
                ..SaveReport::default()
            };
            let after = self.continued(self.commit.clone(), self.commit_bytes.clone());
            return Ok((report, after.registered()));
        }
        // One writer at a time: another session's commit since this view was
        // taken would be overwritten.
        let cache = self.options.cache.as_ref();
        if let Some(cache) = cache
            && cache.committed() != Some(self.version())
        {
            return Err(io::Error::other(
                "another session committed since this one opened; reopen",
            ));
        }
        let images: Images = dirty;
        let segments = match changed {
            true => log::encode_transaction(&images, &self.log.chain)?,
            false => Vec::new(),
        };
        let logged_bytes = segments.iter().map(Bytes::len).sum::<usize>();
        let mut commit = self.commit.clone();
        // Stored values this record names, for the cache.
        let mut named = log::Values::new();
        let named_before = commit.stored_values();
        let budget = self.options.inline_log_bytes;
        // Every write is queued: a failure leaves the commit's outcome unknown.
        let write = |key: &str, value: Option<Bytes>| {
            let queued = match value {
                Some(value) => self.store.put(key, value),
                None => self.store.delete(key),
            };
            queued.map_err(|error| self.failed(error))
        };
        // 1. The durable record's garbage: this record no longer lists it.
        for &n in &commit.garbage {
            write(&log_key(n), None)?;
        }
        commit.garbage.clear();
        // 2. A transaction past the budget, a chain value or a record goes to
        // a stored value of its own, before the record that names it.
        let mut store_value = |commit: &mut Commit, parts: &[Bytes]| {
            let n = commit.next_segment;
            commit.next_segment += 1;
            let value = log::stored_value(parts);
            named.insert(n, value.clone());
            write(&log_key(n), Some(value)).map(|()| n)
        };
        let large = logged_bytes > budget;
        let parts: Vec<Part> = match large {
            true => (segments.iter())
                .map(|segment| {
                    store_value(&mut commit, std::slice::from_ref(segment))
                        .map(|n| Part::Stored(n, 0))
                })
                .collect::<io::Result<_>>()?,
            false => segments.into_iter().map(Part::Inline).collect(),
        };
        let mut values = self.chain.clone();
        let (restart, value) = match &chain {
            None => (false, None),
            Some(ChainUpdate::Append(value)) => (false, Some(value)),
            Some(ChainUpdate::Restart(value)) => (true, Some(value)),
            Some(ChainUpdate::Clear) => (true, None),
        };
        if restart {
            commit.chain.clear();
            values.clear();
        }
        // Past its budget, the record's inline parts are sealed into a stored
        // value, which the record names in their place.
        let added = if large { 0 } else { logged_bytes };
        let added = added
            + value
                .filter(|value| value.len() <= budget)
                .map_or(0, Bytes::len);
        let (log_inline, chain_inline) = commit.inline_parts();
        if (!log_inline.is_empty() || !chain_inline.is_empty())
            && commit.inline_bytes() + added > budget
        {
            let parts = [&log_inline[..], &chain_inline[..]].concat();
            let n = store_value(&mut commit, &parts)?;
            commit.name(&Sealed {
                n,
                log: log_inline.len(),
                chain: chain_inline.len(),
            });
        }
        if let Some(value) = value {
            if value.len() > log::MAX_CHAIN_VALUE_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "chain value too large",
                ));
            }
            let part = match value.len() > budget {
                true => Part::Stored(store_value(&mut commit, std::slice::from_ref(value))?, 0),
                false => Part::Inline(value.clone()),
            };
            commit.chain.push(part);
            values.push(value.clone());
        }
        let mut after_records = self.records.clone();
        for (name, value) in records {
            let id = match commit.record_id(&name) {
                Some(id) => id,
                None => {
                    let id = commit.next_file;
                    if id >= log::MAX_FILES {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "too many records",
                        ));
                    }
                    commit.next_file = id + 1;
                    id
                }
            };
            let record = match value.len() > budget {
                true => Part::Stored(store_value(&mut commit, std::slice::from_ref(&value))?, 0),
                false => Part::Inline(value.clone()),
            };
            commit.directory.insert(
                id,
                Entry {
                    name: name.clone(),
                    record,
                },
            );
            after_records.insert(name, value);
        }
        // 3. The record, listing as garbage the values it names no more.
        commit.version += 1;
        if changed {
            commit.size = size;
            commit.log.push(Logged { size, parts });
        }
        commit.garbage = &named_before - &commit.stored_values();
        let bytes = commit.encode();
        write(COMMIT, Some(bytes.clone()))?;
        let version = commit.version;
        let mut after = self.continued(commit, Some(bytes));
        after.chain = values;
        after.records = after_records;
        if changed {
            after.extend(size, images, logged_bytes);
        }
        // 4. A long log is checkpointed in the same sync. This view is not
        // used again: it must not hold the checkpoint back.
        drop(self.reader.lock().unwrap().take());
        let checkpoint = match after.wants_checkpoint() && after.checkpoint_limit() == u64::MAX {
            true => Some(after.checkpoint(&write)?),
            false => None,
        };
        if let Err(error) = self.store.sync().await {
            return Err(self.failed(error));
        }
        if let Some(cache) = cache {
            let (bytes, epoch) = (
                after.commit_bytes.clone().unwrap_or_default(),
                after.commit.epoch,
            );
            match checkpoint {
                Some(_) => cache.checkpointed(bytes, epoch, version),
                None => cache.record(Some(bytes), epoch, Some(version)),
            }
            cache.keep_values(&after.commit.stored_values(), named);
        }
        let mut report = SaveReport {
            checkpointed_pages: 0,
            version,
        };
        if let Some(checkpoint) = checkpoint {
            report.checkpointed_pages = checkpoint.images.len();
            if let Some(cache) = cache {
                for (index, image) in &checkpoint.images {
                    match image {
                        Some(image) => cache.replace(*index, image.clone()),
                        None => cache.remove(*index),
                    }
                }
                if let Some(hint) = &after.hint {
                    cache.wrote_hint(hint.to_vec());
                }
            }
        }
        Ok((report, after.registered()))
    }

    /// A commit's outcome is unknown: forget what the cache knows.
    fn failed(&self, error: io::Error) -> io::Error {
        if let Some(cache) = &self.options.cache {
            cache.forget();
        }
        error
    }

    /// Queue a checkpoint of the whole log with `write`, and move this view
    /// past it: each page's latest image once, the working-set hint, then a
    /// record with an empty log at the next epoch. The stored values the log
    /// named become garbage, which the next commit deletes.
    fn checkpoint(
        &mut self,
        write: &impl Fn(&str, Option<Bytes>) -> io::Result<()>,
    ) -> io::Result<Checkpoint> {
        let checkpoint = Checkpoint::new(&self.log.images, self.options.page_compression)?;
        for (index, image) in &checkpoint.images {
            write(&page_key(*index), image.clone())?;
        }
        let hint = self.hint();
        if let Some(hint) = &hint {
            write(HINT, Some(Bytes::from(hint.clone())))?;
        }
        let mut commit = Commit {
            epoch: self.commit.epoch + 1,
            log: Vec::new(),
            ..self.commit.clone()
        };
        // The log's stored values, but those the chain or a record names too.
        commit.garbage =
            &self.commit.garbage | &(&self.commit.stored_values() - &commit.stored_values());
        let bytes = commit.encode();
        write(COMMIT, Some(bytes.clone()))?;
        self.commit = commit;
        self.commit_bytes = Some(bytes);
        self.log = Log::new();
        if let Some(hint) = hint {
            self.hint = Some(Bytes::from(hint));
        }
        Ok(checkpoint)
    }

    /// This view under another commit record, for a session that continues
    /// after a save. The log, pages fetched ahead and usage carry over.
    fn continued(&self, commit: Commit, commit_bytes: Option<Bytes>) -> Self {
        let log = Log {
            transactions: self
                .log
                .transactions
                .iter()
                .map(|t| log::Transaction {
                    size: t.size,
                    pages: t.pages.clone(),
                    bytes: t.bytes,
                })
                .collect(),
            images: self.log.images.clone(),
            bytes: self.log.bytes,
            chain: self.log.chain.clone(),
        };
        Self {
            store: self.store.clone(),
            options: self.options.clone(),
            commit,
            commit_bytes,
            log,
            chain: self.chain.clone(),
            records: self.records.clone(),
            unchanged: false,
            page: Mutex::default(),
            fetched: Mutex::new(std::mem::take(&mut *self.fetched.lock().unwrap())),
            readahead: Mutex::default(),
            hint: self.hint.clone(),
            usage: Mutex::new(std::mem::take(&mut *self.usage.lock().unwrap())),
            reader: Mutex::default(),
        }
    }

    /// An empty view at the same version, for a parked engine: no log images,
    /// pages fetched ahead, usage or reader registration.
    pub(crate) fn idle(&self) -> Self {
        let mut idle = self.continued(self.commit.clone(), self.commit_bytes.clone());
        idle.log = Log::new();
        idle.fetched = Mutex::default();
        idle.usage = Mutex::default();
        idle
    }

    /// Register this view, at its version, with the owner cache.
    fn registered(self) -> Self {
        let reader = self
            .options
            .cache
            .as_ref()
            .map(|cache| cache.reader(self.version()));
        *self.reader.lock().unwrap() = reader;
        self
    }

    /// The latest version a checkpoint may write images of: no other open
    /// session may read a page key that holds an image it has not replayed.
    fn checkpoint_limit(&self) -> u64 {
        let cache = self.options.cache.as_ref();
        cache
            .and_then(|cache| cache.oldest_reader(self.version()))
            .unwrap_or(u64::MAX)
    }

    /// Add this owner's acknowledged transaction to the log. Values fetched
    /// ahead for its pages are stale once a checkpoint drops it from the log.
    fn extend(&mut self, size: u64, images: Images, bytes: usize) {
        let fetched = self.fetched.get_mut().unwrap();
        for page in images.keys() {
            fetched.remove(page);
        }
        self.log.append(size, images, bytes);
    }
}

/// A checkpoint's page values: each page's latest image, encoded.
struct Checkpoint {
    images: Vec<(u64, Option<Bytes>)>,
}

impl Checkpoint {
    fn new(images: &Images, compression: PageCompression) -> io::Result<Self> {
        let images = images
            .iter()
            .map(|(&index, image)| {
                let value = match image {
                    Some(page) => Some(Bytes::from(page_codec::encode(page, compression)?)),
                    None => None,
                };
                Ok((index, value))
            })
            .collect::<io::Result<_>>()?;
        Ok(Self { images })
    }
}

#[cfg(all(test, feature = "simulation"))]
#[path = "../tests/unit/snapshot.rs"]
mod tests;
