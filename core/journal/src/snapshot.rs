//! Committed state: the commit record, the log it holds replayed over the
//! checkpointed pages, and the commit that extends it.
//!
//! # Reads
//!
//! Page *n* is the latest committed image of *n* in the replayed log, otherwise
//! `page/<n>` (absent means a zero-filled hole). There is no page map: a cold
//! read is one get.
//!
//! # Commit
//!
//! A commit logs its changed pages as one transaction (see [`crate::log`]).
//! It queues its writes in this order, then one sync makes them durable:
//!
//! 1. **Garbage:** deletes of the stored values the durable record lists as
//!    garbage (it names them no more).
//! 2. **Stored values:** a transaction larger than the record's inline budget
//!    ([`StorageOptions::inline_log_bytes`]), or a large record, goes to
//!    stored values; past the budget, the record's inline log is sealed into
//!    one.
//! 3. **Record:** the commit record: the log so far plus the new transaction, naming
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
//! A checkpoint overwrites page keys that another open session of the
//! namespace reads without the log that covers them. So each snapshot
//! registers with the owner's cache while a session uses it
//! ([`NamespaceCache::reader`]), and a commit checkpoints only when no other
//! session is open; the log grows meanwhile.
//!
//! # Open
//!
//! Read the commit record, then the stored segments it names, if any, and
//! replay its log. With a cache that saw the same record, nothing changed
//! since. An owner whose last commit had an unknown outcome, or that has no
//! cache to tell, syncs alongside, so that what it reads is durable: a
//! commit, even one that changed nothing, then never acknowledges state a
//! crash can still take back. A cold open (no cached pages)
//! fetches a small database whole, or the pages the working-set hint lists,
//! alongside.
use crate::{
    PAGE,
    cache::{NamespaceCache, PageKey, Reader},
    format::{COMMIT, HINT, log_key, page_key},
    hint::{self, Usage},
    kv::KvStore,
    log::{
        self, Base, Bases, Change, Commit, Entry, Images, Kind, Log, Logged, Part, Records,
        file_of, page_id, page_of,
    },
    page_codec::{self, Page, PageCompression},
};
use bytes::Bytes;
use std::{
    collections::{BTreeMap, HashMap},
    io,
    sync::{Arc, Mutex},
};

/// Logical page replacements of one file, by page number; `None` is a hole.
pub(crate) type Changes = BTreeMap<u64, Option<Page>>;

/// What a commit writes to one paged file: its size after it, and the pages
/// it changed.
#[derive(Clone, Debug, Default)]
pub(crate) struct Written {
    pub size: u64,
    pub pages: Changes,
}

/// What a commit writes to paged files, by name.
pub(crate) type Writes = BTreeMap<String, Written>;

/// Default [`StorageOptions::prefetch_pages`].
pub const PREFETCH_PAGES: u64 = 16;
/// Default [`StorageOptions::hint_pages`].
pub const HINT_PAGES: usize = 32;
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

/// Per-session storage options. They never change the stored format.
#[derive(Clone)]
pub struct StorageOptions {
    /// Encoding of newly written pages; stored tags decide how pages are read.
    pub page_compression: PageCompression,
    /// This namespace's handle on the process-wide [`crate::PageCache`].
    pub cache: Option<NamespaceCache>,
    /// Transactions kept in the log: a commit that logs more checkpoints the
    /// whole log. Replay at open reads them all.
    pub checkpoint_after: u32,
    /// Encoded bytes kept in the log: a commit that logs more checkpoints the
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
    /// Pages a checkpointing commit lists in the working-set hint, which a cold
    /// open of a larger database fetches alongside replay (at most
    /// [`crate::MAX_HINT_PAGES`]); 0 neither writes nor reads it.
    pub hint_pages: usize,
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
        }
    }
}

/// What a commit wrote: its log and record bytes, and the pages its checkpoint, if any, wrote.
#[derive(Clone, Debug, Default)]
pub struct CommitReport {
    /// Pages written in place by this commit's checkpoint; 0 when it did not
    /// checkpoint.
    pub checkpointed_pages: usize,
    /// Bytes of its transaction's log segments, every file's: about what its
    /// pages changed.
    pub logged_bytes: usize,
    /// Bytes of the commit record it put; 0 when nothing changed.
    pub record_bytes: usize,
}

/// The committed view a session reads and extends.
pub(crate) struct Snapshot {
    store: Arc<dyn KvStore>,
    options: StorageOptions,
    /// The durable commit record, and its exact bytes (`None` when absent).
    commit: Commit,
    commit_bytes: Option<Bytes>,
    pub(crate) log: Log,
    /// The records' values, by name.
    records: Records,
    /// The bases files were opened with, by name.
    bases: BTreeMap<String, Arc<dyn Base>>,
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
    /// Open the namespace: see the module documentation. A `reader` writes
    /// nothing, and so syncs nothing either.
    pub async fn open(
        store: Arc<dyn KvStore>,
        options: StorageOptions,
        reader: bool,
    ) -> io::Result<Self> {
        let cache = options.cache.as_ref();
        // An owner that knows nothing reads the working-set hint with the record.
        let read_hint = async {
            match cache.is_none_or(|cache| !cache.knows_pages()) && options.hint_pages > 0 {
                true => store.get(HINT).await,
                false => Ok(None),
            }
        };
        // After a commit with an unknown outcome, what open reads may be this
        // owner's writes that a crash can still lose: a sync makes them
        // durable before the session (and the cache) trusts anything of them.
        // An owner without a cache cannot tell, and always syncs.
        let unsettled = cache.and_then(NamespaceCache::unsettled);
        let settle = !reader && (cache.is_none() || unsettled.is_some());
        let settle = async {
            match settle {
                true => store.sync().await,
                false => Ok(()),
            }
        };
        let (commit_bytes, hint, ()) = futures::try_join!(store.get(COMMIT), read_hint, settle)?;
        if let (Some(cache), Some(unknown)) = (cache, unsettled) {
            cache.settled(unknown);
        }
        let commit = match &commit_bytes {
            Some(bytes) => Commit::decode(bytes)?,
            None => Commit::default(),
        };
        let known = cache.is_some_and(|c| c.validate(commit_bytes.as_deref(), commit.epoch));
        // Without warm pages, fetch each small file whole, and the pages the
        // working-set hint lists, alongside the log's stored segments.
        let cold = cache.is_none_or(|cache| !cache.knows_epoch(commit.epoch));
        let logged = commit.inline_pages();
        let mut wanted = std::collections::BTreeSet::new();
        if cold {
            let mut large = false;
            for (file, size) in commit.paged() {
                let pages = size.div_ceil(PAGE);
                match pages <= options.prefetch_pages {
                    true => wanted.extend((0..pages).map(|page| page_id(file, page))),
                    false => large = true,
                }
            }
            if let (true, Some(hint)) = (large, &hint) {
                wanted.extend(hint::decode(hint, options.hint_pages).unwrap_or_default());
            }
        }
        let wanted: Vec<u64> = wanted.into_iter().filter(|n| !logged.contains(n)).collect();
        let prefetch = async {
            let fetched = get_pages(&*store, cache, &wanted).await?;
            Ok::<_, io::Error>(wanted.iter().copied().zip(fetched).collect::<Vec<_>>())
        };
        let named = commit.stored_values();
        let held = cache.map_or_else(log::Values::new, |cache| cache.values(&named));
        let (replayed, prefetched_pages) =
            futures::try_join!(log::replay(&*store, &commit, held), prefetch)?;
        let log::Replayed {
            log,
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
            records,
            bases: BTreeMap::new(),
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
        for (index, (cache_key, value)) in pages {
            if self.log.images.contains_key(&index) {
                continue;
            }
            // The cache keeps values; holes, and pages without a cache, stay here.
            if let (Some(cache), Some(key), Some(value)) = (&self.options.cache, &cache_key, &value)
            {
                match cache.is_decoded() {
                    true => {
                        if let Ok(page) = page_codec::decode_bytes(value) {
                            cache.insert_read(key.clone(), page);
                            continue;
                        }
                    }
                    false => {
                        cache.insert_read(key.clone(), value.clone());
                        continue;
                    }
                }
            }
            fetched.insert(index, (cache_key, value));
        }
        drop(fetched);
        self
    }

    /// The database's version: the transactions ever committed.
    pub fn version(&self) -> u64 {
        self.commit.version
    }

    /// The size of the paged file `name` after replaying the log; 0 if it
    /// has none.
    pub fn file_size(&self, name: &str) -> u64 {
        (self.commit.file_id(name)).map_or(0, |id| self.commit.size(id))
    }

    /// Page `page` of the file `name`: see [`Self::block`].
    pub async fn read(&self, name: &str, page: u64) -> io::Result<Option<Page>> {
        match self.commit.file_id(name) {
            Some(id) => self.block(page_id(id, page)).await,
            None => Ok(None),
        }
    }

    /// The records' values, by name (see [`crate::Journal::record`]).
    pub fn records(&self) -> &Records {
        &self.records
    }

    /// The base the file `file` was opened with; `Err` for a file made with
    /// a base it was not opened with.
    fn base_of(&self, file: u32) -> io::Result<Option<&Arc<dyn Base>>> {
        let Some(entry) = self.commit.directory.get(&file) else {
            return Ok(None);
        };
        match (&entry.kind, self.bases.get(&entry.name)) {
            (Kind::Paged { base: Some(_), .. }, None) => Err(io::Error::other(format!(
                "{:?} has a base: open it with it",
                entry.name
            ))),
            (_, base) => Ok(base),
        }
    }

    /// The bases of the files opened with one, by id.
    fn bases(&self, commit: &Commit) -> Bases {
        (self.bases.iter())
            .filter_map(|(name, base)| Some((commit.file_id(name)?, base.clone())))
            .collect()
    }

    /// What a hole of the page `index` reads as: its file's base page, if it
    /// has a base.
    fn hole(&self, index: u64) -> io::Result<Option<Page>> {
        let base = self.base_of(file_of(index))?;
        Ok(base
            .and_then(|base| base.page(page_of(index)))
            .map(Page::copy_from_slice))
    }

    /// Decode the page `index` (a page id, see [`log::page_id`]); `None` means
    /// zeros. The log's latest image wins, then the cache, then the
    /// checkpointed value at its page key. A hole, or a page absent from the
    /// store, of a file with a base reads as the base's page.
    pub async fn block(&self, index: u64) -> io::Result<Option<Page>> {
        if let Some((cached, page)) = &*self.page.lock().unwrap()
            && *cached == index
        {
            return Ok(Some(page.clone()));
        }
        let cache = self.options.cache.as_ref();
        let base = self.base_of(file_of(index))?;
        let base_page = base.and_then(|base| base.page(page_of(index)));
        let page = if let Some(image) = self.log.images.get(&index) {
            let Some(image) = image else {
                return self.hole(index);
            };
            image.clone()
        } else if let Some((cache, value)) = match cache {
            Some(cache) => cache.get(index).await.map(|value| (cache, value)),
            None => None,
        } {
            match cache.is_decoded() {
                true => value,
                false => page_codec::decode_against(&value, base_page)?,
            }
        } else {
            let (cache_key, Some(image)) = self.fetch(index).await? else {
                return self.hole(index);
            };
            let page = page_codec::decode_against(&image, base_page)?;
            if let (Some(cache), Some(key)) = (cache, cache_key) {
                let value = if cache.is_decoded() {
                    page.clone()
                } else {
                    image
                };
                cache.insert_read(key, value);
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
        let file = file_of(index);
        let last = page_id(file, self.commit.size(file).div_ceil(PAGE));
        let ahead: Vec<u64> = {
            let fetched = self.fetched.lock().unwrap();
            (index + 1..(index + 1 + ahead).min(last))
                .filter(|n| !self.log.images.contains_key(n) && !fetched.contains_key(n))
                .collect()
        };
        let pages: Vec<_> = std::iter::once(index).chain(ahead).collect();
        let cache = self.options.cache.as_ref();
        let mut values = get_pages(&*self.store, cache, &pages).await?.into_iter();
        let value = values.next().expect("the page itself");
        let mut fetched = self.fetched.lock().unwrap();
        fetched.extend(pages[1..].iter().copied().zip(values));
        Ok(value)
    }

    /// The working-set hint a checkpointing commit writes, unless it is unchanged
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

    /// Whether the log is long enough to checkpoint, and can be: no file's
    /// segments wait for their base.
    fn wants_checkpoint(&self) -> bool {
        let log = &self.log;
        let too_long = log.transactions > self.options.checkpoint_after as usize
            || log.bytes > self.options.checkpoint_log_bytes;
        log.transactions > 0 && too_long && !log.waits_for_bases()
    }

    /// Refuse what a commit cannot store, before it writes anything: a
    /// record too large, a name too long, a name both a record and a paged
    /// file, a size not a multiple of 512 or past the largest file.
    pub(crate) fn check(&self, writes: &Writes, records: &Records) -> io::Result<()> {
        let paged = |id| matches!(self.commit.directory[&id].kind, Kind::Paged { .. });
        for (name, value) in records {
            if value.len() > log::MAX_RECORD_BYTES {
                let (len, max) = (value.len(), log::MAX_RECORD_BYTES);
                return refuse(format!("record {name:?} of {len} bytes exceeds {max}"));
            }
        }
        for name in records.keys().chain(writes.keys()) {
            if name.len() > log::MAX_NAME_BYTES {
                return refuse(format!(
                    "name {name:?} exceeds {} bytes",
                    log::MAX_NAME_BYTES
                ));
            }
        }
        for (name, written) in writes {
            if records.contains_key(name) || self.commit.file_id(name).is_some_and(|id| !paged(id))
            {
                return refuse(format!("{name:?} is a record, not a paged file"));
            }
            let size = written.size;
            if !size.is_multiple_of(512) || size > log::MAX_DATABASE_BYTES {
                return refuse(format!("{name:?}: invalid size {size}"));
            }
            if written.pages.keys().any(|&page| page >= 1 << 32) {
                return refuse(format!("{name:?}: a page past the largest file"));
            }
        }
        for name in records.keys() {
            if self.commit.file_id(name).is_some_and(paged) {
                return refuse(format!("{name:?} is a paged file, not a record"));
            }
        }
        Ok(())
    }

    /// Commit `writes` and `records` as one transaction (see the module
    /// documentation), returning what it wrote and the committed view
    /// afterwards, which a session may continue on. `writes` change paged
    /// files (a new name makes a new file), each of `records` replaces that
    /// record's value, in the same put. Without page changes the log gains no
    /// transaction; without any change nothing is committed. A commit that
    /// leaves the log long also checkpoints it, before the one sync.
    ///
    /// Requires exclusive ownership until every request settles, even on
    /// cancellation. After an error the outcome is unknown: reopen. This
    /// snapshot is not used again either way.
    pub(crate) async fn commit(
        &self,
        mut writes: Writes,
        mut records: Records,
    ) -> io::Result<(CommitReport, Self)> {
        self.check(&writes, &records)?;
        // A record never set reads as empty.
        records.retain(|name, value| {
            self.records
                .get(name)
                .map_or(!value.is_empty(), |v| v != value)
        });
        writes.retain(|name, written| {
            !written.pages.is_empty() || written.size != self.file_size(name)
        });
        let changed = !writes.is_empty();
        if !changed && records.is_empty() {
            let report = CommitReport::default();
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
        let mut commit = self.commit.clone();
        // A file's id, allocated for a new name.
        let allocate = |commit: &mut Commit, name: &str, kind: Kind| match commit.file_id(name) {
            Some(id) => Ok(id),
            None if commit.next_file >= log::MAX_FILES => refuse("too many files".into()),
            None => {
                let id = commit.next_file;
                commit.next_file += 1;
                let name = name.to_owned();
                commit.directory.insert(id, Entry { name, kind });
                Ok(id)
            }
        };
        let mut images = Images::new();
        let mut sizes = BTreeMap::new();
        for (name, written) in writes {
            let base = self.bases.get(&name);
            let kind = Kind::Paged {
                size: 0,
                base: base.map(|base| base.id()),
            };
            let id = allocate(&mut commit, &name, kind)?;
            let based = matches!(
                commit.directory[&id].kind,
                Kind::Paged { base: Some(_), .. }
            );
            for (page, image) in written.pages {
                let image = match (based, base) {
                    (true, None) if image.is_some() => {
                        return refuse(format!("{name:?} has a base: open it with it"));
                    }
                    // A page of the base is a hole.
                    (true, Some(base)) if image.as_deref() == base.page(page) => None,
                    _ => image,
                };
                images.insert(page_id(id, page), image);
            }
            sizes.insert(id, written.size);
        }
        let bases = self.bases(&commit);
        let mut segments = log::encode_transaction(&images, &self.log.chain, &bases)?;
        let logged_bytes = segments.values().flatten().map(Bytes::len).sum::<usize>();
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
        // 2. A transaction past the budget, or a record, goes to
        // a stored value of its own, before the record that names it.
        let mut store_value = |commit: &mut Commit, parts: &[Bytes]| {
            let n = commit.next_segment;
            commit.next_segment += 1;
            let value = log::stored_value(parts);
            named.insert(n, value.clone());
            write(&log_key(n), Some(value)).map(|()| n)
        };
        let large = logged_bytes > budget;
        let mut changes = Vec::with_capacity(sizes.len());
        for (&file, &size) in &sizes {
            let segments = segments.remove(&file).unwrap_or_default();
            let parts: Vec<Part> = match large {
                true => (segments.iter())
                    .map(|segment| {
                        store_value(&mut commit, std::slice::from_ref(segment))
                            .map(|n| Part::Stored(n, 0))
                    })
                    .collect::<io::Result<_>>()?,
                false => segments.into_iter().map(Part::Inline).collect(),
            };
            if let Some(Entry {
                kind: Kind::Paged { size: held, .. },
                ..
            }) = commit.directory.get_mut(&file)
            {
                *held = size;
            }
            changes.push(Change { file, size, parts });
        }
        // Past its budget, the record's inline log is sealed into a stored
        // value, which the record names in its place.
        let added = if large { 0 } else { logged_bytes };
        let inline = commit.inline_parts();
        if !inline.is_empty() && commit.inline_bytes() + added > budget {
            let n = store_value(&mut commit, &inline)?;
            commit.name(n, inline.len());
        }
        let mut after_records = self.records.clone();
        for (name, value) in records {
            let empty = Kind::Record(Part::Inline(Bytes::new()));
            let id = allocate(&mut commit, &name, empty)?;
            let record = match value.len() > budget {
                true => Part::Stored(store_value(&mut commit, std::slice::from_ref(&value))?, 0),
                false => Part::Inline(value.clone()),
            };
            if let Some(entry) = commit.directory.get_mut(&id) {
                entry.kind = Kind::Record(record);
            }
            after_records.insert(name, value);
        }
        // 3. The record, listing as garbage the values it names no more.
        commit.version += 1;
        if changed {
            commit.log.push(Logged { changes });
        }
        commit.garbage = &named_before - &commit.stored_values();
        let bytes = commit.encode();
        let record_bytes = bytes.len();
        write(COMMIT, Some(bytes.clone()))?;
        let version = commit.version;
        let mut after = self.continued(commit, Some(bytes));
        after.records = after_records;
        if changed {
            after.extend(images, logged_bytes);
        }
        // 4. A long log is checkpointed in the same sync. This view is not
        // used again: it must not hold the checkpoint back.
        drop(self.reader.lock().unwrap().take());
        let others_open = cache.is_some_and(|cache| cache.other_readers(version));
        let checkpoint = match after.wants_checkpoint() && !others_open {
            true => Some(after.checkpoint(&write)?),
            false => None,
        };
        if let Err(error) = self.store.sync().await {
            return Err(self.failed(error));
        }
        let checkpointed_pages = checkpoint.as_ref().map_or(0, |c| c.images.len());
        if let Some(cache) = cache {
            // The checkpoint's pages first: a journal that opens on the new
            // record must not find a page as it was before the checkpoint.
            // (One opening on the old record replays the log over them.)
            if let Some(checkpoint) = &checkpoint {
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
        let report = CommitReport {
            checkpointed_pages,
            logged_bytes,
            record_bytes,
        };
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
        let bases = self.bases(&self.commit);
        let checkpoint = Checkpoint::new(&self.log.images, self.options.page_compression, &bases)?;
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
        // The log's stored values, but those a record names too.
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
    /// after a commit. The log, pages fetched ahead and usage carry over.
    fn continued(&self, commit: Commit, commit_bytes: Option<Bytes>) -> Self {
        Self {
            store: self.store.clone(),
            options: self.options.clone(),
            commit,
            commit_bytes,
            log: self.log.clone(),
            records: self.records.clone(),
            bases: self.bases.clone(),
            unchanged: false,
            page: Mutex::default(),
            fetched: Mutex::new(std::mem::take(&mut *self.fetched.lock().unwrap())),
            readahead: Mutex::default(),
            hint: self.hint.clone(),
            usage: Mutex::new(std::mem::take(&mut *self.usage.lock().unwrap())),
            reader: Mutex::default(),
        }
    }

    /// This view, with the file `name` opened with `base`: a file made with
    /// another base, or without one, refuses it. Its log decodes now.
    pub(crate) fn with_base(&self, name: &str, base: Arc<dyn Base>) -> io::Result<Self> {
        let id = self.commit.file_id(name);
        if let Some(id) = id {
            match &self.commit.directory[&id].kind {
                Kind::Paged {
                    base: Some(held), ..
                } if *held == base.id() => {}
                _ => return refuse(format!("{name:?} was made with another base, or none")),
            }
        }
        let mut with = self.continued(self.commit.clone(), self.commit_bytes.clone());
        if let Some(id) = id {
            with.log.decode_file(id, &*base)?;
            // Pages fetched ahead that its log now covers are stale once a
            // checkpoint drops them from the log (see `with_fetched`).
            let Self { fetched, log, .. } = &mut with;
            fetched
                .get_mut()
                .unwrap()
                .retain(|page, _| !log.images.contains_key(page));
        }
        with.bases.insert(name.to_owned(), base);
        Ok(with.registered())
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

    /// Add this owner's acknowledged transaction to the log. Values fetched
    /// ahead for its pages are stale once a checkpoint drops it from the log.
    fn extend(&mut self, images: Images, bytes: usize) {
        let fetched = self.fetched.get_mut().unwrap();
        for page in images.keys() {
            fetched.remove(page);
        }
        self.log.append(images, bytes);
    }
}

/// The stored values of `pages`, read together, each with its cache key taken
/// before the read.
async fn get_pages(
    store: &dyn KvStore,
    cache: Option<&NamespaceCache>,
    pages: &[u64],
) -> io::Result<Vec<Fetched>> {
    let cache_keys: Vec<_> = pages
        .iter()
        .map(|&n| cache.map(|cache| cache.key(n)))
        .collect();
    let keys: Vec<_> = pages.iter().copied().map(page_key).collect();
    let values = futures::future::try_join_all(keys.iter().map(|key| store.get(key))).await?;
    Ok(cache_keys.into_iter().zip(values).collect())
}

/// A commit refused before it writes anything: what it was asked is invalid.
fn refuse<T>(message: String) -> io::Result<T> {
    Err(io::Error::new(io::ErrorKind::InvalidInput, message))
}

/// A checkpoint's page values: each page's latest image, encoded.
struct Checkpoint {
    images: Vec<(u64, Option<Bytes>)>,
}

impl Checkpoint {
    /// A page of a file with a base is encoded against its base page, and
    /// is a hole if it equals it.
    fn new(images: &Images, compression: PageCompression, bases: &Bases) -> io::Result<Self> {
        let images = images
            .iter()
            .map(|(&index, image)| {
                let base = bases.get(&file_of(index));
                let base_page = base.and_then(|base| base.page(page_of(index)));
                let value = match (image, base_page) {
                    (Some(page), Some(base_page)) if page[..] == base_page[..] => None,
                    (Some(page), Some(base_page)) => Some(Bytes::from(page_codec::encode_against(
                        page,
                        base_page,
                        compression,
                    )?)),
                    (Some(page), None) => Some(Bytes::from(page_codec::encode(page, compression)?)),
                    (None, _) => None,
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
