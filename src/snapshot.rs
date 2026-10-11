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
//! A save commits its changed pages as one log transaction (see [`crate::log`])
//! and, in the same round trip, does a bounded slice of checkpoint work:
//!
//! - **Commit:** the transaction's segments at [`Log::next`]; multi-segment
//!   transactions write their commit part after the other parts.
//! - **Checkpoint:** when more than [`StorageOptions::checkpoint_after`]
//!   transactions are in the log, the latest images of the pages changed by the
//!   oldest transactions (about [`CHECKPOINT_PAGES`]) are put to `page/<n>`.
//!   Overwriting in place is safe: until `meta` moves past those transactions,
//!   replay applies their images over whatever is there. Holes are deletes.
//! - **Meta:** `meta.log_start` may only pass a transaction once its pages are
//!   acknowledged. With a [`crate::PageCache`], the owner remembers acknowledged
//!   checkpoints and records them in the next save's round trip. Without one, a
//!   checkpointing save writes `meta` in a second round trip.
//! - **Garbage:** segments in `[deleted_below, log_start)` of the durable `meta`
//!   are deleted. They can only be deleted after a durable `meta` has moved past
//!   them, otherwise replay could start at a deleted segment.
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
    log::{self, Images, Log, Meta},
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

/// Upper bound on concurrent backend mutations of each save group: enough for a
/// commit, a checkpoint slice of whole transactions and garbage in one round trip.
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
/// Pages a save checkpoints, rounded up to whole transactions.
pub const CHECKPOINT_PAGES: usize = 12;
/// Logged bytes that trigger checkpointing; a larger save checkpoints itself.
pub const CHECKPOINT_LOG_BYTES: usize = 256 * 1024;
/// Logged bytes beyond which a checkpoint slice covers up to
/// [`CATCH_UP_PAGES`], so a log grown by failed checkpoints shrinks again and
/// replay at open stays bounded.
const CATCH_UP_LOG_BYTES: usize = 16 * CHECKPOINT_LOG_BYTES;
/// Pages of a catch-up checkpoint slice: a few round trips of mutations.
const CATCH_UP_PAGES: usize = 4 * MAX_MUTATIONS_IN_FLIGHT;

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
    /// Transactions kept in the log before saves start checkpointing the oldest.
    pub checkpoint_after: u32,
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
    /// Pages written in place by this save's checkpoint slice.
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
        let batch = log::batch(u64::from(options.checkpoint_after));
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
        let reuse = cache.map(|c| c.validate(meta_bytes.as_deref(), meta.log_start));
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
            page_codec::decode_bytes(image)?
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

    /// Oldest transactions to checkpoint before this save commits.
    ///
    /// Once more than `checkpoint_after` transactions or [`CHECKPOINT_LOG_BYTES`]
    /// are logged, take whole transactions from the oldest until about
    /// [`CHECKPOINT_PAGES`] pages are covered. This often empties the log, so the
    /// next saves only append.
    fn checkpoint_plan(&self) -> Option<Checkpoint> {
        let transactions = &self.log.transactions;
        if transactions.len() <= self.options.checkpoint_after as usize
            && self.log.bytes <= CHECKPOINT_LOG_BYTES
        {
            return None;
        }
        // A page's image is its latest; another open session may only see it
        // if that transaction ends within the session's log position.
        let limit = self.checkpoint_limit();
        let mut latest = HashMap::new();
        for transaction in transactions {
            for &page in &transaction.pages {
                latest.insert(page, transaction.end);
            }
        }
        let budget = match self.log.bytes > CATCH_UP_LOG_BYTES {
            true => CATCH_UP_PAGES,
            false => CHECKPOINT_PAGES,
        };
        let mut images = Images::new();
        let mut covered = 0;
        for transaction in transactions {
            // Pages already in the slice cost nothing more: their latest image
            // is written once, whichever transactions it covers.
            let new = transaction
                .pages
                .iter()
                .filter(|page| !images.contains_key(*page));
            if covered > 0 && images.len() + new.count() > budget {
                break;
            }
            if transaction.pages.iter().any(|page| latest[page] > limit) {
                break;
            }
            for &page in &transaction.pages {
                images.insert(page, self.log.images[&page].clone());
            }
            covered += 1;
        }
        if covered == 0 {
            return None;
        }
        let last = &transactions[covered - 1];
        Some(Checkpoint {
            log_start: last.end,
            size: last.size,
            images,
        })
    }

    /// Commit `dirty` pages as one log transaction and advance checkpointing.
    ///
    /// Requires exclusive ownership until every request settles, even on
    /// cancellation. After an error, reopen; never reuse this snapshot.
    #[cfg_attr(not(test), allow(dead_code))]
    pub async fn publish(&self, dirty: Changes, size: u64) -> io::Result<SaveReport> {
        Ok(self.commit(dirty, size).await?.0)
    }

    /// [`Self::publish`], also returning the committed view afterwards, which a
    /// session may continue on. This snapshot is not used again.
    pub(crate) async fn commit(&self, dirty: Changes, size: u64) -> io::Result<(SaveReport, Self)> {
        if dirty.is_empty() && size == self.size() {
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
        let mut images = Images::new();
        for (index, page) in dirty {
            let image = match page {
                Some(page) => Some(Bytes::from(page_codec::encode(
                    &page,
                    self.options.page_compression,
                )?)),
                None => None,
            };
            images.insert(index, image);
        }
        let segments = log::encode_transaction(self.log.next, size, &images);
        let logged_bytes = segments.iter().map(Vec::len).sum::<usize>();
        let large = logged_bytes > CHECKPOINT_LOG_BYTES;
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

        // Round trip of the commit: checkpoint pages, garbage and a pending claim.
        let cache = self.options.cache.as_ref();
        let plan = if large { None } else { self.checkpoint_plan() };
        let mut maintenance = plan.as_ref().map_or_else(Vec::new, Checkpoint::mutations);
        let garbage = self.meta.deleted_below..self.meta.log_start;
        let collect = !garbage.is_empty() && (cache.is_some() || plan.is_some() || large);
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
        // A checkpointing save also refreshes the working-set hint, if it changed.
        let hint = match plan.is_some() || large {
            true => self.hint(),
            false => None,
        };
        if let Some(hint) = &hint {
            maintenance.push(Mutation::Put(HINT.to_owned(), hint.clone().into()));
        }
        let store = &*self.store;
        let commit = async {
            concurrently(store, parts).await?;
            apply(store, commit_part).await
        };
        let (committed, mut cleanup) = futures::join!(commit, concurrently(store, maintenance));
        if let Err(error) = committed {
            if let Some(cache) = cache {
                cache.forget();
            }
            return Err(error);
        }

        // A large transaction is checkpointed right away, with the whole log, so
        // later opens do not replay it.
        let alone = self.checkpoint_limit() == u64::MAX;
        let plan = match (large, cleanup.is_ok() && alone) {
            (true, true) => {
                let mut all = self.log.images.clone();
                all.extend(images.clone());
                let checkpoint = Checkpoint {
                    log_start: next,
                    size,
                    images: all,
                };
                cleanup = concurrently(store, checkpoint.mutations()).await;
                Some(checkpoint)
            }
            _ => plan,
        };
        let checkpointed_pages = plan.as_ref().map_or(0, |plan| plan.images.len());
        let committed = (next, size, images, logged_bytes, tail);
        if let Err(error) = cleanup {
            if let Some(cache) = cache {
                cache.forget_meta(next);
            }
            let report = SaveReport {
                checkpointed_pages,
                cleanup_error: Some(error),
                log_next: next,
            };
            // Keep what was durable before: the log still covers every page
            // the failed checkpoint may have written.
            let mut after = self.continued(self.meta, self.meta_bytes.clone(), self.claim);
            after.extend(committed, None);
            return Ok((report, after.registered()));
        }

        // Everything acknowledged is durable. Work out what `meta` may say: past
        // the checkpointed transactions, and past the deleted garbage.
        let (durable, durable_bytes) = match early_meta {
            Some(claim) => (claim, Some(claim.encode().into())),
            None => (self.meta, self.meta_bytes.clone()),
        };
        let deleted_below = if collect {
            garbage.end.max(durable.deleted_below)
        } else {
            durable.deleted_below
        };
        let target = match &plan {
            Some(plan) => Meta {
                log_start: plan.log_start,
                deleted_below,
                size: plan.size,
            },
            None => Meta {
                deleted_below,
                ..durable
            },
        };
        // An owner with a cache records the claim in its next save's round trip.
        // Without one, or after a large save that already took extra round trips,
        // write it now so that no other process replays a bulk load.
        let mut claim = None;
        let mut cleanup_error = None;
        let (mut durable, mut durable_bytes) = (durable, durable_bytes);
        if target != durable {
            if cache.is_some() && !large {
                claim = Some(target);
            } else {
                match store.put(META, target.encode().into()).await {
                    Ok(()) => (durable, durable_bytes) = (target, Some(target.encode().into())),
                    Err(error) => cleanup_error = Some(error),
                }
            }
        }
        if let Some(cache) = cache {
            if let Some(plan) = &plan {
                for (&index, image) in &plan.images {
                    match image {
                        Some(image) => cache.replace(index, image.clone()),
                        None => cache.remove(index),
                    }
                }
            }
            if cleanup_error.is_some() {
                cache.forget_meta(next);
            } else {
                cache.record(durable_bytes.clone(), claim, next);
                if let Some(hint) = &hint {
                    cache.wrote_hint(hint.clone());
                }
            }
        }
        // The log now starts at the recorded or claimed checkpoint. If `meta`
        // could not be written, it keeps the checkpointed transactions.
        let mut after = self.continued(durable, durable_bytes, claim);
        after.extend(committed, hint.map(Bytes::from));
        after.log.drop_before(after.base().log_start);
        let report = SaveReport {
            checkpointed_pages,
            cleanup_error,
            log_next: next,
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

    /// End this view's registration: its engine is parked, not reading.
    pub(crate) fn release_reader(&self) {
        self.reader.lock().unwrap().take();
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
    fn extend(
        &mut self,
        (end, size, images, bytes, tail): (u64, u64, Images, usize, Bytes),
        hint: Option<Bytes>,
    ) {
        let fetched = self.fetched.get_mut().unwrap();
        for page in images.keys() {
            fetched.remove(page);
        }
        self.log.append(end, size, images, bytes, tail);
        if hint.is_some() {
            self.hint = hint;
        }
    }
}

/// Pages to write in place and the log position they make redundant.
struct Checkpoint {
    log_start: u64,
    size: u64,
    images: Images,
}

impl Checkpoint {
    fn mutations(&self) -> Vec<Mutation> {
        self.images
            .iter()
            .map(|(&index, image)| match image {
                Some(image) => Mutation::Put(page_key(index), image.clone()),
                None => Mutation::Delete(page_key(index)),
            })
            .collect()
    }
}

#[cfg(all(test, feature = "simulation"))]
#[path = "../tests/unit/snapshot.rs"]
mod tests;
