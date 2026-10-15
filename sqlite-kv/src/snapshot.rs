//! Committed state: checkpointed pages at stable keys plus the log replayed over
//! them, and the save that extends it.
//!
//! # Reads
//!
//! Page *n* is the latest committed image of *n* in the replayed log, otherwise
//! `page/<n>` (absent means a zero-filled hole). There is no page map: a cold
//! read is one get.
//!
//! # Save
//!
//! A save commits its changed pages as one log transaction (see [`crate::log`]):
//!
//! - **Commit:** the transaction's segments at [`Log::next`]; multi-segment
//!   transactions write their commit part after the other parts.
//! - **Checkpoint:** once more than [`StorageOptions::checkpoint_after`]
//!   transactions or [`StorageOptions::checkpoint_log_bytes`] are in the log,
//!   the save then checkpoints the whole log ([`Snapshot::compact`]): the latest
//!   image of every page it changed is put to `page/<n>`. Overwriting in place
//!   is safe: until `meta` moves past the log, replay applies its images over
//!   whatever is there. Holes are deletes. The log's compression chain
//!   restarts once those pages are acknowledged.
//! - **Meta:** `meta.log_start` passes the log only once its pages are
//!   acknowledged.
//! - **Garbage:** segments in `[deleted_below, log_start)` of the durable `meta`
//!   are deleted. They can only be deleted after a durable `meta` has moved past
//!   them, otherwise replay could start at a deleted segment. With a
//!   [`crate::PageCache`], garbage a checkpoint left is deleted in the next
//!   save's round trip, which records it in `meta` in the save after.
//!
//! Every step is idempotent, and a crash at any point leaves state that replay
//! reads correctly. There are no object IDs, no intent and no orphans: pages are
//! overwritten, never allocated.
//!
//! # Open
//!
//! Read `meta`, then replay from its log start. With a cache the first probe batch
//! is read together with `meta`, and if another writer moved `meta`, the
//! segments it checkpointed are read alongside replay to refresh the cache. On backends where a failed put can be visible
//! without being durable ([`KvStore::durable_when_visible`] is false), open
//! re-puts the commit tail and `meta` before exposing them, unless this owner
//! wrote them and had them acknowledged.
use crate::{
    cache::{NamespaceCache, PageKey, Reader, Reuse},
    format::{HINT, META, log_key, page_key},
    hint::{self, Usage},
    kv::KvStore,
    log::{self, Chain, Images, Log, Meta},
    page_codec::{self, Page, PageCompression},
};
use bytes::Bytes;
use futures::{StreamExt, future::BoxFuture, stream::FuturesUnordered};
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
    /// One past the last committed transaction after this save.
    pub(crate) log_next: u64,
}

/// One backend mutation of a save.
enum Mutation {
    Put(String, Bytes),
    Delete(String),
}

fn apply(store: &dyn KvStore, mutation: Mutation) -> BoxFuture<'_, io::Result<()>> {
    Box::pin(async move {
        match mutation {
            Mutation::Put(key, value) => store.put(&key, value).await,
            Mutation::Delete(key) => store.delete(&key).await,
        }
    })
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
    /// Durable `meta` and its exact bytes (`None` when absent), possibly behind `claim`.
    meta: Meta,
    meta_bytes: Option<Bytes>,
    /// An acknowledged checkpoint this owner has not recorded in `meta` yet.
    claim: Option<Meta>,
    pub(crate) log: Log,
    /// Nothing changed since this owner last saw the namespace: the cache's
    /// `meta` matched and the log ends where this owner left it.
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
        // A log of unknown length: replay starts small and doubles.
        let batch = log::batch(0);
        let expected = cache.and_then(NamespaceCache::expected_start);
        let first_round = async {
            match expected {
                Some(start) => {
                    // The owner knows where its log ended: probe exactly that far.
                    let first = match cache.and_then(NamespaceCache::committed) {
                        Some(end) if end >= start => log::batch((end - start).saturating_sub(1)),
                        _ => batch,
                    };
                    let (meta, probe) =
                        futures::join!(store.get(META), log::probe(&*store, start, first));
                    Ok((meta?, Some((start, probe))))
                }
                None => Ok::<_, io::Error>((store.get(META).await?, None)),
            }
        };
        // An owner that knows nothing reads the working-set hint with meta.
        let read_hint = async {
            match expected.is_none() && options.hint_pages > 0 {
                true => store.get(HINT).await,
                false => Ok(None),
            }
        };
        let (first_round, hint) = futures::join!(first_round, read_hint);
        let ((meta_bytes, prefetched), hint) = (first_round?, hint?);
        let meta = match &meta_bytes {
            Some(bytes) => Meta::decode(bytes)?,
            None => Meta::default(),
        };
        let reuse = cache.map(|c| c.validate(meta_bytes.as_deref(), meta.id, meta.log_start));
        let (claim, committed, meta_known, since) = match reuse {
            Some(Reuse::Known(claim, committed)) => (claim, committed, true, None),
            Some(Reuse::Changed(since)) => (None, None, false, since),
            None => (None, None, false, None),
        };
        let start = claim.map_or(meta.log_start, |claim| claim.log_start);
        // The speculative probe serves replay, or else the changed range.
        let (prefetched, prefetched_changes) = match prefetched {
            Some((at, probe)) if at == start => (Some(probe?), None),
            Some((at, probe)) if Some(at) == since => (None, Some(probe?)),
            _ => (None, None),
        };
        let changes = async {
            match since {
                Some(since) => log::changes(&*store, since..meta.log_start, prefetched_changes)
                    .await
                    .map(Some),
                None => Ok(None),
            }
        };
        // Without warm pages, fetch a small database whole, or the pages the
        // working-set hint lists, alongside replay.
        let pages = meta.size.div_ceil(PAGE);
        let cold = !meta_known && since.is_none() && claim.is_none();
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
        let (log, changes, prefetched_pages) = futures::try_join!(
            log::replay(&*store, start, batch, prefetched),
            changes,
            prefetch
        )?;
        if let (Some(cache), Some(changes)) = (cache, changes) {
            cache.refresh(changes.as_ref());
        }
        let unchanged = meta_known && committed == Some(log.next);

        if !store.durable_when_visible() {
            let mut confirm = Vec::new();
            if let Some(tail) = &log.tail
                && committed != Some(log.next)
            {
                confirm.push(Mutation::Put(log_key(log.next - 1), tail.clone()));
            }
            if let Some(bytes) = &meta_bytes
                && !meta_known
            {
                confirm.push(Mutation::Put(META.to_owned(), bytes.clone()));
            }
            concurrently(&*store, confirm).await?;
        }
        if let Some(cache) = cache {
            cache.record(meta_bytes.clone(), claim, log.next);
        }
        let reader = cache.map(|cache| cache.reader(log.next));
        Ok(Self {
            store,
            options,
            meta,
            meta_bytes,
            claim,
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

    /// The checkpoint the log is replayed from.
    fn base(&self) -> Meta {
        self.claim.unwrap_or(self.meta)
    }

    /// Logical database size after replaying the log.
    pub fn size(&self) -> u64 {
        self.log.size().unwrap_or(self.base().size)
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

    /// Commit `dirty` pages as one log transaction and advance checkpointing.
    ///
    /// Requires exclusive ownership until every request settles, even on
    /// cancellation. After an error, reopen; never reuse this snapshot.
    #[cfg_attr(not(test), allow(dead_code))]
    pub async fn publish(&self, dirty: Changes, size: u64) -> io::Result<SaveReport> {
        Ok(self.commit(dirty, size, false).await?.0)
    }

    /// [`Self::publish`], also returning the committed view afterwards, which a
    /// session may continue on. This snapshot is not used again. Without
    /// changes nothing is logged, unless `record`: then an empty transaction
    /// is, and the log (the version) grows all the same. A commit that leaves
    /// the log long checkpoints it ([`Self::compact`]).
    pub(crate) async fn commit(
        &self,
        dirty: Changes,
        size: u64,
        record: bool,
    ) -> io::Result<(SaveReport, Self)> {
        if dirty.is_empty() && size == self.size() && !record {
            let report = SaveReport {
                log_next: self.log.next,
                ..SaveReport::default()
            };
            let after = self.continued(self.meta, self.meta_bytes.clone(), self.claim);
            return Ok((report, after.registered()));
        }
        // One writer at a time: another session's commit since this view was
        // taken would be overwritten at the same log position.
        if let Some(cache) = &self.options.cache
            && cache.committed() != Some(self.log.next)
        {
            return Err(io::Error::other(
                "another session committed since this one opened; reopen",
            ));
        }
        let images: Images = dirty;
        let segments = log::encode_transaction(self.log.next, size, &images, &self.log.chain)?;
        let logged_bytes = segments.iter().map(Vec::len).sum::<usize>();
        let next = self.log.next + segments.len() as u64;
        let mut parts: Vec<_> = segments
            .into_iter()
            .enumerate()
            .map(|(n, bytes)| Mutation::Put(log_key(self.log.next + n as u64), bytes.into()))
            .collect();
        let commit_part = parts.pop().expect("a transaction has a commit part");
        let tail = match &commit_part {
            Mutation::Put(_, bytes) => bytes.clone(),
            Mutation::Delete(_) => unreachable!("a commit part is a put"),
        };

        // Round trip of the commit: with a cache, garbage and a pending claim.
        let cache = self.options.cache.as_ref();
        let mut maintenance = Vec::new();
        let garbage = self.meta.deleted_below..self.meta.log_start;
        let collect = !garbage.is_empty() && cache.is_some();
        if collect {
            maintenance.extend(garbage.clone().map(|seq| Mutation::Delete(log_key(seq))));
        }
        let early_meta = match (cache, self.claim) {
            (Some(_), Some(claim)) if claim != self.meta => Some(claim),
            _ => None,
        };
        if let Some(claim) = early_meta {
            maintenance.push(Mutation::Put(META.to_owned(), claim.encode().into()));
        }
        let store = &*self.store;
        let commit = async {
            concurrently(store, parts).await?;
            apply(store, commit_part).await
        };
        let (committed, cleanup) = futures::join!(commit, concurrently(store, maintenance));
        if let Err(error) = committed {
            if let Some(cache) = cache {
                cache.forget();
            }
            return Err(error);
        }
        let committed = (next, size, images, logged_bytes, tail);
        if let Err(error) = cleanup {
            if let Some(cache) = cache {
                cache.forget_meta(next);
            }
            let report = SaveReport {
                checkpointed_pages: 0,
                cleanup_error: Some(error),
                log_next: next,
            };
            let mut after = self.continued(self.meta, self.meta_bytes.clone(), self.claim);
            after.extend(committed);
            return Ok((report, after.registered()));
        }

        // Everything acknowledged is durable: the claim, and the deleted garbage,
        // which the next save's round trip records.
        let (durable, durable_bytes) = match early_meta {
            Some(claim) => (claim, Some(claim.encode().into())),
            None => (self.meta, self.meta_bytes.clone()),
        };
        let target = Meta {
            deleted_below: match collect {
                true => garbage.end.max(durable.deleted_below),
                false => durable.deleted_below,
            },
            ..durable
        };
        let claim = (target != durable).then_some(target);
        if let Some(cache) = cache {
            cache.record(durable_bytes.clone(), claim, next);
        }
        let mut after = self.continued(durable, durable_bytes, claim);
        after.extend(committed);
        let after = after.registered();
        let report = SaveReport {
            checkpointed_pages: 0,
            cleanup_error: None,
            log_next: next,
        };
        if !after.wants_checkpoint() {
            return Ok((report, after));
        }
        // This view is not used again: it must not hold the checkpoint back.
        drop(self.reader.lock().unwrap().take());
        let (checkpoint, after) = after.compact().await?;
        Ok((
            SaveReport {
                log_next: next,
                ..checkpoint
            },
            after,
        ))
    }

    /// Checkpoint the whole log, and continue on the result: each page's latest
    /// image once, then `meta` past the log, then the segments it covered,
    /// then `meta` past those. Only puts and deletes, each round after the one
    /// before is acknowledged, so a crash anywhere leaves a state that replay
    /// reads correctly, and on an ordered store nothing waits for a reply.
    /// Once the pages are acknowledged the compression chain restarts at the
    /// log's end, before `meta` may pass it.
    /// Nothing to do without a log, or while another session's view still
    /// needs it. A failure is a [`SaveReport::cleanup_error`]: the log keeps
    /// covering the pages, and a later save retries.
    pub(crate) async fn compact(&self) -> io::Result<(SaveReport, Self)> {
        let unchanged = |cleanup_error, restart: bool| {
            let report = SaveReport {
                cleanup_error,
                log_next: self.log.next,
                ..SaveReport::default()
            };
            let mut after = self.continued(self.meta, self.meta_bytes.clone(), self.claim);
            if restart {
                after.log.chain = Chain::new(self.log.next);
            }
            (report, after.registered())
        };
        let cache = self.options.cache.as_ref();
        let own = cache.is_none_or(|cache| cache.committed() == Some(self.log.next));
        if self.log.transactions.is_empty() || !own || self.checkpoint_limit() != u64::MAX {
            return Ok(unchanged(None, false));
        }
        let store = &*self.store;
        let checkpoint = Checkpoint::new(&self.log.images, self.options.page_compression)?;
        let hint = self.hint();
        let mut pages = checkpoint.mutations();
        if let Some(hint) = &hint {
            pages.push(Mutation::Put(HINT.to_owned(), hint.clone().into()));
        }
        let failed = |error, restart| {
            if let Some(cache) = cache {
                cache.forget_meta(self.log.next);
            }
            Ok(unchanged(Some(error), restart))
        };
        if let Err(error) = concurrently(store, pages).await {
            return failed(error, false);
        }
        // `meta` passes the log only now that its pages are acknowledged.
        let moved = Meta {
            log_start: self.log.next,
            deleted_below: self.meta.deleted_below,
            size: self.size(),
            // The first checkpoint names the database.
            id: match self.meta.id {
                0 => crate::log::random_id(),
                id => id,
            },
        };
        let moved_bytes: Bytes = moved.encode().into();
        if let Err(error) = store.put(META, moved_bytes.clone()).await {
            // It may have landed: later segments must not need the log.
            return failed(error, true);
        }
        // Then the segments below it, and a `meta` that says so.
        let garbage = moved.deleted_below..moved.log_start;
        let deletes = garbage.map(|seq| Mutation::Delete(log_key(seq))).collect();
        let collected = Meta {
            deleted_below: moved.log_start,
            ..moved
        };
        let collected_bytes: Bytes = collected.encode().into();
        let (durable, durable_bytes, cleanup_error) = match concurrently(store, deletes).await {
            Ok(()) => match store.put(META, collected_bytes.clone()).await {
                Ok(()) => (collected, collected_bytes, None),
                Err(error) => (moved, moved_bytes, Some(error)),
            },
            Err(error) => (moved, moved_bytes, Some(error)),
        };
        if let Some(cache) = cache {
            for (index, image) in &checkpoint.images {
                match image {
                    Some(image) => cache.replace(*index, image.clone()),
                    None => cache.remove(*index),
                }
            }
            cache.record(Some(durable_bytes.clone()), None, self.log.next);
            if let Some(hint) = &hint {
                cache.wrote_hint(hint.clone());
            }
        }
        let mut after = self.continued(durable, Some(durable_bytes), None);
        if let Some(hint) = hint {
            after.hint = Some(Bytes::from(hint));
        }
        after.log.drop_before(durable.log_start);
        after.log.chain = Chain::new(self.log.next);
        let report = SaveReport {
            checkpointed_pages: checkpoint.images.len(),
            cleanup_error,
            log_next: self.log.next,
        };
        Ok((report, after.registered()))
    }

    /// This committed view under a new checkpoint record, for a session that
    /// continues after a save. Pages fetched ahead and usage carry over.
    fn continued(&self, meta: Meta, meta_bytes: Option<Bytes>, claim: Option<Meta>) -> Self {
        let log = Log {
            transactions: self
                .log
                .transactions
                .iter()
                .map(|t| log::Transaction {
                    end: t.end,
                    size: t.size,
                    pages: t.pages.clone(),
                    bytes: t.bytes,
                })
                .collect(),
            images: self.log.images.clone(),
            next: self.log.next,
            tail: self.log.tail.clone(),
            bytes: self.log.bytes,
            chain: self.log.chain.clone(),
        };
        Self {
            store: self.store.clone(),
            options: self.options.clone(),
            meta,
            meta_bytes,
            claim,
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

    /// An empty view at the same position, for a parked engine: no log images,
    /// pages fetched ahead, usage or reader registration.
    pub(crate) fn idle(&self) -> Self {
        let mut idle = self.continued(self.meta, self.meta_bytes.clone(), self.claim);
        idle.log = Log::new(self.log.next);
        idle.fetched = Mutex::default();
        idle.usage = Mutex::default();
        idle
    }

    /// Register this view, at its log position, with the owner cache.
    fn registered(self) -> Self {
        let reader = self
            .options
            .cache
            .as_ref()
            .map(|cache| cache.reader(self.log.next));
        *self.reader.lock().unwrap() = reader;
        self
    }

    /// The latest log position a checkpoint may write images of: no other open
    /// session may read a page key that holds an image it has not replayed.
    fn checkpoint_limit(&self) -> u64 {
        let cache = self.options.cache.as_ref();
        cache
            .and_then(|cache| cache.oldest_reader(self.log.next))
            .unwrap_or(u64::MAX)
    }

    /// Add this owner's acknowledged transaction to the log. Values fetched
    /// ahead for its pages are stale once a checkpoint drops it from the log.
    fn extend(&mut self, (end, size, images, bytes, tail): (u64, u64, Images, usize, Bytes)) {
        let fetched = self.fetched.get_mut().unwrap();
        for page in images.keys() {
            fetched.remove(page);
        }
        self.log.append(end, size, images, bytes, tail);
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
