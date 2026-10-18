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
//! A save commits its changed pages as one transaction (see [`crate::log`]):
//!
//! - **Commit:** one put of the commit record, the log so far plus the new
//!   transaction. A transaction too large to hold inline
//!   ([`StorageOptions::max_inline_bytes`]) is put first at stored segments,
//!   and the record, put once they are acknowledged, names them.
//! - **Checkpoint:** once more than [`StorageOptions::checkpoint_after`]
//!   transactions or [`StorageOptions::checkpoint_log_bytes`] are in the log,
//!   the save then checkpoints it ([`Snapshot::compact`]): the latest image
//!   of every page it changed is put to `page/<n>`, and once those are
//!   acknowledged, a record with an empty log and the next epoch. Overwriting
//!   in place is safe: until that record lands, the log applies its images over
//!   whatever is there. Holes are deletes.
//! - **Garbage:** stored segments the log no longer names are listed in the
//!   record, and deleted in the next save's round trip.
//!
//! Nothing depends on the order the store applies writes in: a write is relied
//! on only once acknowledged. A crash at any point leaves state that open reads
//! correctly. There are no object IDs, no intent and no orphans: pages are
//! overwritten, never allocated.
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
    log::{self, Commit, Images, Log, Logged, Part},
    page_codec::{self, Page, PageCompression},
};
use bytes::Bytes;
use futures::{StreamExt, stream::FuturesUnordered};
use std::{
    collections::{BTreeMap, HashMap},
    io,
    sync::{Arc, Mutex},
};

/// Logical page replacements from the session; `None` is a hole.
type Changes = BTreeMap<u64, Option<Page>>;

/// Upper bound on concurrent backend mutations of each save group.
pub(crate) const MAX_MUTATIONS_IN_FLIGHT: usize = 64;
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
/// Default [`StorageOptions::checkpoint_log_bytes`].
pub const CHECKPOINT_LOG_BYTES: usize = 256 * 1024;
/// Default [`StorageOptions::max_inline_bytes`].
pub const MAX_INLINE_BYTES: usize = 64 * 1024;

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
    /// A transaction whose segments take more is put at stored segments, which
    /// the commit record names, rather than inline in it: a save then costs a
    /// round trip more, and the log's later saves do not rewrite it. So is any
    /// that would take the record's inline log past
    /// [`Self::checkpoint_log_bytes`].
    pub max_inline_bytes: usize,
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
            checkpoint_after: 4,
            checkpoint_log_bytes: CHECKPOINT_LOG_BYTES,
            max_inline_bytes: MAX_INLINE_BYTES,
            prefetch_pages: PREFETCH_PAGES,
            hint_pages: HINT_PAGES,
            durability: Durability::Save,
            max_private_bytes: MAX_PRIVATE_BYTES,
            max_result_bytes: MAX_RESULT_BYTES,
        }
    }
}

/// `Ok` means the transaction committed. A cleanup error is checkpoint or
/// garbage work that failed after the commit; a later save retries it.
#[derive(Debug, Default)]
pub struct SaveReport {
    /// Pages written in place by this save's checkpoint; 0 when it did not
    /// checkpoint, or its checkpoint failed.
    pub checkpointed_pages: usize,
    /// Committed, but checkpoint work failed. Do not replay SQL.
    pub cleanup_error: Option<io::Error>,
    /// The database's version after this save.
    pub(crate) version: u64,
}

/// One backend mutation of a save.
enum Mutation {
    Put(String, Bytes),
    Delete(String),
}

/// Apply mutations of distinct keys, at most [`MAX_MUTATIONS_IN_FLIGHT`] at once.
/// After the first error no new mutation starts; running ones are awaited.
///
/// Each request is issued when it is queued, not when the stream first polls it,
/// so a window of requests really is in flight together.
async fn concurrently(store: &dyn KvStore, mut mutations: Vec<Mutation>) -> io::Result<()> {
    let mut queued = mutations.iter_mut().map(|mutation| match mutation {
        Mutation::Put(key, value) => store.put(key, std::mem::take(value)),
        Mutation::Delete(key) => store.delete(key),
    });
    let mut running = FuturesUnordered::new();
    let mut error = None;
    loop {
        while error.is_none() && running.len() < MAX_MUTATIONS_IN_FLIGHT {
            let Some(request) = queued.next() else { break };
            running.push(request);
        }
        match running.next().await {
            Some(Ok(())) => {}
            Some(Err(e)) => {
                error.get_or_insert(e);
            }
            None => return error.map_or(Ok(()), Err),
        }
    }
}

/// The committed view a session reads and extends.
pub(crate) struct Snapshot {
    store: Arc<dyn KvStore>,
    options: StorageOptions,
    /// The durable commit record, and its exact bytes (`None` when absent).
    commit: Commit,
    commit_bytes: Option<Bytes>,
    pub(crate) log: Log,
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
        let wanted: Vec<u64> = match (cold, &hint) {
            (false, _) => Vec::new(),
            _ if pages <= options.prefetch_pages => (0..pages).collect(),
            (true, Some(hint)) => hint::decode(hint, options.hint_pages).unwrap_or_default(),
            (true, None) => Vec::new(),
        };
        let prefetch = async {
            let tokens: Vec<_> = wanted.iter().map(|&n| cache.map(|c| c.token(n))).collect();
            let keys: Vec<_> = wanted.iter().copied().map(page_key).collect();
            let values = futures::future::try_join_all(keys.iter().map(|k| store.get(k))).await?;
            let fetched = tokens.into_iter().zip(values);
            Ok::<_, io::Error>(wanted.iter().copied().zip(fetched).collect::<Vec<_>>())
        };
        let (log, prefetched_pages) = futures::try_join!(log::replay(&*store, &commit), prefetch)?;
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

    /// The commit record's effects (see [`crate::Session::commit_with`]).
    pub fn effects(&self) -> &Bytes {
        &self.commit.effects
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
        let log = &self.log;
        !log.transactions.is_empty()
            && (log.transactions.len() > self.options.checkpoint_after as usize
                || log.bytes > self.options.checkpoint_log_bytes)
    }

    /// Commit `dirty` pages as one transaction and advance checkpointing.
    ///
    /// Requires exclusive ownership until every request settles, even on
    /// cancellation. After an error, reopen; never reuse this snapshot.
    #[cfg_attr(not(test), allow(dead_code))]
    pub async fn publish(&self, dirty: Changes, size: u64) -> io::Result<SaveReport> {
        Ok(self.commit(dirty, size, None).await?.0)
    }

    /// [`Self::publish`], also returning the committed view afterwards, which a
    /// session may continue on. This snapshot is not used again. With
    /// `effects`, the record's effects become them, in the same put. Without
    /// changes nothing is committed, unless the effects change: then an empty
    /// transaction is. A commit that leaves the log long checkpoints it
    /// ([`Self::compact`]).
    pub(crate) async fn commit(
        &self,
        dirty: Changes,
        size: u64,
        effects: Option<Bytes>,
    ) -> io::Result<(SaveReport, Self)> {
        let effects = effects.filter(|effects| *effects != self.commit.effects);
        if dirty.is_empty() && size == self.size() && effects.is_none() {
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
        let segments = log::encode_transaction(&images, &self.log.chain)?;
        let logged_bytes = segments.iter().map(Bytes::len).sum::<usize>();
        let store = &*self.store;
        let mut commit = self.commit.clone();
        // A large transaction goes to stored segments, durable before the
        // record that names them; so does any once the record holds a log's
        // worth inline (its checkpoints failing), which keeps it bounded.
        let inline = commit.inline_bytes() + logged_bytes;
        let budget = self
            .options
            .checkpoint_log_bytes
            .max(self.options.max_inline_bytes);
        let parts = if logged_bytes > self.options.max_inline_bytes || inline > budget {
            let first = commit.next_segment;
            commit.next_segment += segments.len() as u64;
            let puts = (segments.iter().enumerate())
                .map(|(n, segment)| {
                    let key = log_key(first + n as u64);
                    Mutation::Put(key, log::stored_segment(segment))
                })
                .collect();
            if let Err(error) = concurrently(store, puts).await {
                return Err(self.failed(error));
            }
            (first..commit.next_segment).map(Part::Stored).collect()
        } else {
            segments.into_iter().map(Part::Inline).collect()
        };
        commit.version += 1;
        commit.size = size;
        commit.log.push(Logged { size, parts });
        if let Some(effects) = effects {
            commit.effects = effects;
        }
        let bytes = commit.encode();
        // Garbage goes in the commit's round trip; the record after this one
        // forgets it, once deleted.
        let garbage = commit.garbage.clone();
        let deletes = garbage
            .clone()
            .map(|n| Mutation::Delete(log_key(n)))
            .collect();
        let (committed, collected) = futures::join!(
            store.put(COMMIT, bytes.clone()),
            concurrently(store, deletes)
        );
        if let Err(error) = committed {
            return Err(self.failed(error));
        }
        let version = commit.version;
        if let Some(cache) = cache {
            cache.record(Some(bytes.clone()), commit.epoch, Some(version));
        }
        let mut after = self.continued(commit, Some(bytes));
        after.extend(size, images, logged_bytes);
        if collected.is_ok() {
            after.commit.garbage = garbage.end..garbage.end;
        }
        let after = after.registered();
        let report = SaveReport {
            checkpointed_pages: 0,
            cleanup_error: collected.err(),
            version,
        };
        if !after.wants_checkpoint() {
            return Ok((report, after));
        }
        // This view is not used again: it must not hold the checkpoint back.
        drop(self.reader.lock().unwrap().take());
        let (mut checkpoint, after) = after.compact().await?;
        checkpoint.cleanup_error = checkpoint.cleanup_error.or(report.cleanup_error);
        Ok((checkpoint, after))
    }

    /// A commit's outcome is unknown: forget what the cache knows.
    fn failed(&self, error: io::Error) -> io::Error {
        self.forget();
        error
    }

    fn forget(&self) {
        if let Some(cache) = &self.options.cache {
            cache.forget();
        }
    }

    /// Checkpoint the whole log, and continue on the result: each page's latest
    /// image once, then, once those are acknowledged, a record with an empty
    /// log and the next epoch. The stored segments the log named become
    /// garbage. Nothing to do without a log, or while another session's view
    /// still needs it. A failure is a [`SaveReport::cleanup_error`]: the log
    /// keeps covering the pages, and a later save retries.
    pub(crate) async fn compact(&self) -> io::Result<(SaveReport, Self)> {
        let unchanged = |cleanup_error| {
            let report = SaveReport {
                cleanup_error,
                version: self.version(),
                ..SaveReport::default()
            };
            let after = self.continued(self.commit.clone(), self.commit_bytes.clone());
            (report, after.registered())
        };
        let cache = self.options.cache.as_ref();
        let own = cache.is_none_or(|cache| cache.committed() == Some(self.version()));
        if self.log.transactions.is_empty() || !own || self.checkpoint_limit() != u64::MAX {
            return Ok(unchanged(None));
        }
        let store = &*self.store;
        let checkpoint = Checkpoint::new(&self.log.images, self.options.page_compression)?;
        let hint = self.hint();
        let mut pages = checkpoint.mutations();
        if let Some(hint) = &hint {
            pages.push(Mutation::Put(HINT.to_owned(), hint.clone().into()));
        }
        if let Err(error) = concurrently(store, pages).await {
            return Ok(unchanged(Some(error)));
        }
        // The record passes the log only now that its pages are acknowledged.
        let commit = Commit {
            epoch: self.commit.epoch + 1,
            garbage: self.commit.garbage.start..self.commit.next_segment,
            log: Vec::new(),
            ..self.commit.clone()
        };
        let bytes = commit.encode();
        if let Err(error) = store.put(COMMIT, bytes.clone()).await {
            // It may have landed: the next open sees which epoch did.
            self.forget();
            return Ok(unchanged(Some(error)));
        }
        if let Some(cache) = cache {
            cache.record(Some(bytes.clone()), commit.epoch, Some(commit.version));
            for (index, image) in &checkpoint.images {
                match image {
                    Some(image) => cache.replace(*index, image.clone()),
                    None => cache.remove(*index),
                }
            }
            if let Some(hint) = &hint {
                cache.wrote_hint(hint.clone());
            }
        }
        let mut after = self.continued(commit, Some(bytes));
        if let Some(hint) = hint {
            after.hint = Some(Bytes::from(hint));
        }
        after.log = Log::new();
        let report = SaveReport {
            checkpointed_pages: checkpoint.images.len(),
            cleanup_error: None,
            version: self.version(),
        };
        Ok((report, after.registered()))
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

    fn mutations(&self) -> Vec<Mutation> {
        self.images
            .iter()
            .map(|(index, image)| match image {
                Some(image) => Mutation::Put(page_key(*index), image.clone()),
                None => Mutation::Delete(page_key(*index)),
            })
            .collect()
    }
}

#[cfg(all(test, feature = "simulation"))]
#[path = "../tests/unit/snapshot.rs"]
mod tests;
