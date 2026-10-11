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
//!   acknowledged. With a [`PageCache`], the owner remembers acknowledged
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
//! is read together with `meta`. On backends where a failed put can be visible
//! without being durable ([`KvStore::durable_when_visible`] is false), open
//! re-puts the commit tail and `meta` before exposing them, unless this owner
//! wrote them and had them acknowledged.
use crate::{
    cache::PageCache,
    format::{META, log_key, page_key},
    kv::KvStore,
    log::{self, Images, Log, Meta},
    page_codec::{self, Page, PageCompression},
};
use futures::{StreamExt, future::BoxFuture, stream::FuturesUnordered};
use std::{collections::BTreeMap, io, sync::Arc, sync::Mutex};

/// Logical page replacements from the session; `None` is a hole.
type Changes = BTreeMap<u64, Option<Vec<u8>>>;

/// Upper bound on concurrent backend mutations of each save group.
pub(crate) const MAX_MUTATIONS_IN_FLIGHT: usize = 16;
/// Pages a save checkpoints, rounded up to whole transactions.
pub const CHECKPOINT_PAGES: usize = 12;
/// Logged bytes that trigger checkpointing; a larger save checkpoints itself.
pub const CHECKPOINT_LOG_BYTES: usize = 256 * 1024;

/// Per-session storage options. They never change the stored format.
#[derive(Clone)]
pub struct StorageOptions {
    /// Encoding of newly written pages; stored tags decide how pages are read.
    pub page_compression: PageCompression,
    /// Owner-process cache; it must be dedicated to this namespace.
    pub cache: Option<PageCache>,
    /// Transactions kept in the log before saves start checkpointing the oldest.
    pub checkpoint_after: u32,
}

impl Default for StorageOptions {
    fn default() -> Self {
        Self {
            page_compression: PageCompression::default(),
            cache: None,
            checkpoint_after: 4,
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
}

/// One backend mutation of a save.
enum Mutation {
    Put(String, Vec<u8>),
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
    meta_bytes: Option<Vec<u8>>,
    /// An acknowledged checkpoint this owner has not recorded in `meta` yet.
    claim: Option<Meta>,
    pub(crate) log: Log,
    /// The last decoded page, for repeated reads of one page.
    page: Mutex<Option<(u64, Page)>>,
}

impl Snapshot {
    pub async fn open_with_options(
        store: Arc<dyn KvStore>,
        options: StorageOptions,
    ) -> io::Result<Self> {
        let cache = options.cache.as_ref();
        let batch = log::batch(u64::from(options.checkpoint_after));
        let expected = cache.and_then(PageCache::expected_start);
        let (meta_bytes, prefetched) = match expected {
            Some(start) => {
                let (meta, probe) =
                    futures::join!(store.get(META), log::probe(&*store, start, batch));
                (meta?, Some((start, probe)))
            }
            None => (store.get(META).await?, None),
        };
        let meta = match &meta_bytes {
            Some(bytes) => Meta::decode(bytes)?,
            None => Meta::default(),
        };
        let (claim, committed, meta_known) = match cache.map(|c| c.validate(meta_bytes.as_deref()))
        {
            Some(Some((claim, committed))) => (claim, committed, true),
            _ => (None, None, false),
        };
        let start = claim.map_or(meta.log_start, |claim| claim.log_start);
        let prefetched = match prefetched {
            Some((at, probe)) if at == start => Some(probe?),
            _ => None,
        };
        let log = log::replay(&*store, start, batch, prefetched).await?;

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
        Ok(Self {
            store,
            options,
            meta,
            meta_bytes,
            claim,
            log,
            page: Mutex::default(),
        })
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
            Arc::new(page_codec::decode(image)?)
        } else if let Some(value) = cache.and_then(|cache| cache.get(index)) {
            match cache.is_some_and(PageCache::is_decoded) {
                true => value,
                false => Arc::new(page_codec::decode(&value)?),
            }
        } else {
            let Some(image) = self.store.get(&page_key(index)).await? else {
                return Ok(None);
            };
            let page = Arc::new(page_codec::decode(&image)?);
            if let Some(cache) = cache {
                let value = if cache.is_decoded() {
                    page.clone()
                } else {
                    Arc::new(image)
                };
                cache.insert(index, value);
            }
            page
        };
        *self.page.lock().unwrap() = Some((index, page.clone()));
        Ok(Some(page))
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
        let mut images = Images::new();
        let mut covered = 0;
        for transaction in transactions {
            if covered > 0 && images.len() + transaction.pages.len() > CHECKPOINT_PAGES {
                break;
            }
            for &page in &transaction.pages {
                images.insert(page, self.log.images[&page].clone());
            }
            covered += 1;
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
    pub async fn publish(&self, dirty: Changes, size: u64) -> io::Result<SaveReport> {
        if dirty.is_empty() && size == self.size() {
            return Ok(SaveReport::default());
        }
        let mut images = Images::new();
        for (index, page) in dirty {
            let image = match page {
                Some(page) => Some(Arc::new(page_codec::encode(
                    &page,
                    self.options.page_compression,
                )?)),
                None => None,
            };
            images.insert(index, image);
        }
        let segments = log::encode_transaction(self.log.next, size, &images);
        let large = segments.iter().map(Vec::len).sum::<usize>() > CHECKPOINT_LOG_BYTES;
        let next = self.log.next + segments.len() as u64;
        let mut parts: Vec<_> = segments
            .into_iter()
            .enumerate()
            .map(|(n, bytes)| Mutation::Put(log_key(self.log.next + n as u64), bytes))
            .collect();
        let commit_part = parts.pop().expect("a transaction has a commit part");

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
            maintenance.push(Mutation::Put(META.to_owned(), claim.encode()));
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
        let plan = match (large, cleanup.is_ok()) {
            (true, true) => {
                let mut all = self.log.images.clone();
                all.extend(images);
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
        if let Err(error) = cleanup {
            if let Some(cache) = cache {
                cache.forget();
            }
            return Ok(SaveReport {
                checkpointed_pages,
                cleanup_error: Some(error),
            });
        }

        // Everything acknowledged is durable. Work out what `meta` may say: past
        // the checkpointed transactions, and past the deleted garbage.
        let (durable, durable_bytes) = match early_meta {
            Some(claim) => (claim, Some(claim.encode())),
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
        let mut durable_bytes = durable_bytes;
        if target != durable {
            if cache.is_some() && !large {
                claim = Some(target);
            } else {
                match store.put(META, target.encode()).await {
                    Ok(()) => durable_bytes = Some(target.encode()),
                    Err(error) => cleanup_error = Some(error),
                }
            }
        }
        if let Some(cache) = cache {
            if let Some(plan) = &plan {
                for (&index, image) in &plan.images {
                    match image {
                        Some(image) if cache.is_decoded() => match page_codec::decode(image) {
                            Ok(page) => cache.insert(index, Arc::new(page)),
                            Err(_) => cache.remove(index),
                        },
                        Some(image) => cache.insert(index, image.clone()),
                        None => cache.remove(index),
                    }
                }
            }
            if cleanup_error.is_some() {
                cache.forget();
            } else {
                cache.record(durable_bytes, claim, next);
            }
        }
        Ok(SaveReport {
            checkpointed_pages,
            cleanup_error,
        })
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
                Some(image) => Mutation::Put(page_key(index), Vec::clone(image)),
                None => Mutation::Delete(page_key(index)),
            })
            .collect()
    }
}

#[cfg(all(test, feature = "simulation"))]
#[path = "../tests/unit/snapshot.rs"]
mod tests;
