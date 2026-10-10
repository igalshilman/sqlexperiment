//! Immutable pages and a persistent radix map, published through one durable head.
//!
//! Logical page numbers are u64 addresses. Each tree level consumes one address
//! byte, most significant first. Edges at level one point to pages; higher edges
//! point to map nodes. Height grows only as needed and collapses through a sole
//! zero edge when high pages disappear. An absent edge is a zero-filled hole.
//!
//! Radix addressing needs no separator keys, splits or balancing and has at most
//! eight levels; a height-one map covers 256 pages with no external node. Path
//! copying limits an update to touched branches. SQLite's own B-trees stay inside
//! the pages; this map only translates page numbers to immutable object IDs.
//!
//! The root is embedded in [`Head`]; only lower nodes are separate objects.
//! [`rewrite`](Snapshot::rewrite) copies touched paths and records replaced edges.
//! [`finalize_plan`](Snapshot::finalize_plan) walks only newly staged objects,
//! prunes temporary wrappers and keeps reused old references out of retirement.
//! The whole plan is built before the first mutation; see [`crate::pending`] for
//! publication order and recovery.
//!
//! Reads use a snapshot-scoped decoded-node cache, a one-page decoded cache and the
//! optional cross-session [`ObjectCache`]. Old snapshot readers must stop before a
//! save retires replaced objects: structural sharing is not historical-reader support.
use crate::{
    cache::ObjectCache,
    format::{HEAD, HEAD_MAGIC, Id, Kind, PENDING},
    kv::KvStore,
    log::{self, Log, MAX_SEGMENT_BYTES, Pages},
    map_codec::{self, Node},
    page_codec::{self, PageCompression},
    pending::{Mutation, Objects, PendingSave, SaveReport, concurrently, recover},
};
use futures::future::BoxFuture;
use std::{
    collections::{BTreeMap, BTreeSet},
    io,
    sync::{Arc, Mutex},
};

/// Logical page replacements; `None` removes a page (a sparse hole).
type Changes = Pages;
/// The same changes after page bytes have received object IDs.
type Refs = BTreeMap<u64, Option<Id>>;

/// Per-session storage options. They never change the stored format.
#[derive(Clone, Default)]
pub struct StorageOptions {
    /// Encoding of newly written pages; stored tags decide how pages are read.
    pub page_compression: PageCompression,
    /// Cross-session object cache. It must be dedicated to this namespace.
    pub cache: Option<ObjectCache>,
    /// Durable delta-log segments a save may append before folding them into a
    /// full publication; zero publishes on every save. See [`crate::log`].
    pub max_log_segments: u32,
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
fn height_for(index: u64) -> u8 {
    ((64 - index.leading_zeros()).max(1).div_ceil(8)) as u8
}
fn digit(index: u64, level: u8) -> u8 {
    (index >> (8 * (level - 1))) as u8
}

/// A write plan, built completely before any mutation.
struct Build {
    /// High-water mark, including temporary objects later pruned from the plan.
    last_id: Id,
    /// New objects; ascending IDs place children before parents.
    objects: BTreeMap<Id, (Kind, Vec<u8>)>,
    /// Replaced committed objects, filtered against references kept by the new root.
    retired: Objects,
}
impl Build {
    fn put(&mut self, kind: Kind, bytes: Vec<u8>) -> io::Result<Id> {
        self.last_id = self
            .last_id
            .checked_add(1)
            .ok_or_else(|| invalid("object IDs exhausted"))?;
        self.objects.insert(self.last_id, (kind, bytes));
        Ok(self.last_id)
    }
}

/// The snapshot selection stored at the head key.
///
/// Encoding: `SK11`, database size (u64 LE), allocation counter (u64 LE), height
/// (u8), first delta-log sequence number (u64 LE, zero when the log is disabled),
/// then the encoded root filling the rest. Height is zero iff the root is empty.
/// Root children are bounded by the allocation counter.
#[derive(Default)]
pub(crate) struct Head {
    /// First radix level.
    pub root: Node,
    /// Number of address bytes, or zero for an empty map.
    pub height: u8,
    /// Committed allocation high-water mark.
    pub last_id: Id,
    /// Logical database length in bytes, including sparse holes.
    pub size: u64,
    /// First delta-log sequence number to replay, or zero when the log is disabled.
    pub log_start: u64,
}
impl Head {
    const PREFIX: usize = 29;

    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut out = HEAD_MAGIC.to_vec();
        out.extend_from_slice(&self.size.to_le_bytes());
        out.extend_from_slice(&self.last_id.to_le_bytes());
        out.push(self.height);
        out.extend_from_slice(&self.log_start.to_le_bytes());
        out.extend(map_codec::encode(&self.root));
        out
    }
    pub(crate) fn decode(bytes: &[u8]) -> io::Result<Self> {
        if bytes.len() < Self::PREFIX || &bytes[..4] != HEAD_MAGIC {
            return Err(invalid("invalid snapshot head"));
        }
        let size = u64::from_le_bytes(bytes[4..12].try_into().unwrap());
        let last_id = u64::from_le_bytes(bytes[12..20].try_into().unwrap());
        let height = bytes[20];
        let log_start = u64::from_le_bytes(bytes[21..29].try_into().unwrap());
        let root = map_codec::decode(&bytes[Self::PREFIX..], last_id, true)?;
        if height > 8 || root.is_empty() != (height == 0) {
            return Err(invalid("invalid root height"));
        }
        Ok(Self {
            root,
            height,
            last_id,
            size,
            log_start,
        })
    }
}

/// A recovered, committed head plus read access to its immutable objects.
pub(crate) struct Snapshot {
    store: Arc<dyn KvStore>,
    options: StorageOptions,
    /// Exact committed bytes; the next save records them as its old head.
    head_bytes: Option<Vec<u8>>,
    pub head: Head,
    /// Delta-log segments replayed over the head.
    log: Log,
    nodes: Mutex<BTreeMap<Id, Node>>,
    page: Mutex<Option<(Id, Vec<u8>)>>,
}

impl Snapshot {
    /// Settle any pending save, then load the committed head and its delta log.
    pub async fn open_with_options(
        store: Arc<dyn KvStore>,
        options: StorageOptions,
    ) -> io::Result<Self> {
        let expected = options.max_log_segments as usize;
        let hint = options.cache.as_ref().map_or(0, ObjectCache::log_start);
        // Probe the log speculatively alongside recovery when the cache knows
        // where it starts. Recovery never mutates segments at or after that start.
        let (head_bytes, prefetched) = if hint != 0 {
            let (head, probe) = futures::join!(
                recover(&*store),
                log::probe(&*store, hint, log::batch(expected))
            );
            (head?, Some(probe))
        } else {
            (recover(&*store).await?, None)
        };
        let head = match &head_bytes {
            Some(bytes) => Head::decode(bytes)?,
            None => Head::default(),
        };
        let prefetched = match prefetched {
            Some(probe) if hint == head.log_start => Some(probe?),
            _ => None,
        };
        let log = log::load(&*store, head.log_start, expected, prefetched).await?;
        if let Some(cache) = &options.cache {
            cache.set_log_start(head.log_start);
        }
        Ok(Self {
            store,
            options,
            head_bytes,
            head,
            log,
            nodes: Mutex::default(),
            page: Mutex::default(),
        })
    }

    /// Logical database size after replaying the log.
    pub fn size(&self) -> u64 {
        self.log.size.unwrap_or(self.head.size)
    }

    /// Fetch a committed object, through the shared cache when configured.
    async fn fetch(&self, kind: Kind, id: Id) -> io::Result<Arc<[u8]>> {
        let cache = self.options.cache.as_ref();
        if let Some(bytes) = cache.and_then(|cache| cache.get((kind, id))) {
            return Ok(bytes);
        }
        let bytes: Arc<[u8]> = self
            .store
            .get(&kind.key(id))
            .await?
            .ok_or_else(|| invalid("missing referenced snapshot object"))?
            .into();
        if let Some(cache) = cache {
            cache.insert((kind, id), bytes.clone());
        }
        Ok(bytes)
    }

    async fn node(&self, id: Option<Id>) -> io::Result<Node> {
        let Some(id) = id else {
            return Ok(Node::new());
        };
        if let Some(node) = self.nodes.lock().unwrap().get(&id).cloned() {
            return Ok(node);
        }
        let node = map_codec::decode(&self.fetch(Kind::Node, id).await?, id - 1, false)?;
        self.nodes.lock().unwrap().insert(id, node.clone());
        Ok(node)
    }

    async fn staged_node(&self, build: &Build, id: Option<Id>) -> io::Result<Node> {
        if let Some(id) = id
            && let Some((kind, bytes)) = build.objects.get(&id)
        {
            return match kind {
                Kind::Node => map_codec::decode(bytes, id - 1, false),
                Kind::Block | Kind::Log => Err(invalid("expected index node")),
            };
        }
        self.node(id).await
    }

    async fn block_id(&self, index: u64) -> io::Result<Option<Id>> {
        let Head { root, height, .. } = &self.head;
        if height_for(index) > *height {
            return Ok(None);
        }
        let mut node = root.clone();
        for level in (2..=*height).rev() {
            let Some(&child) = node.get(&digit(index, level)) else {
                return Ok(None);
            };
            node = self.node(Some(child)).await?;
        }
        Ok(node.get(&digit(index, 1)).copied())
    }

    /// Resolve a logical page and decode it; `None` means a hole.
    pub async fn block(&self, index: u64) -> io::Result<Option<Vec<u8>>> {
        if let Some(page) = self.log.pages.get(&index) {
            return Ok(page.clone());
        }
        let Some(id) = self.block_id(index).await? else {
            return Ok(None);
        };
        if let Some((cached, bytes)) = &*self.page.lock().unwrap()
            && *cached == id
        {
            return Ok(Some(bytes.clone()));
        }
        let bytes = page_codec::decode(&self.fetch(Kind::Block, id).await?)?;
        *self.page.lock().unwrap() = Some((id, bytes.clone()));
        Ok(Some(bytes))
    }

    /// Group sorted changes by this level's digit, recursively copy only touched
    /// branches and record replaced child IDs. Unchanged edges keep their IDs.
    fn rewrite<'a>(
        &'a self,
        build: &'a mut Build,
        mut node: Node,
        level: u8,
        changes: &'a [(u64, Option<Id>)],
    ) -> BoxFuture<'a, io::Result<Node>> {
        Box::pin(async move {
            for group in changes.chunk_by(|a, b| digit(a.0, level) == digit(b.0, level)) {
                let edge = digit(group[0].0, level);
                let old_id = node.get(&edge).copied();
                let child = if level == 1 {
                    group[0].1
                } else {
                    let old = self.staged_node(build, old_id).await?;
                    let updated = self.rewrite(build, old.clone(), level - 1, group).await?;
                    if updated == old {
                        old_id
                    } else if updated.is_empty() {
                        None
                    } else {
                        Some(build.put(Kind::Node, map_codec::encode(&updated))?)
                    }
                };
                if let Some(old) = old_id
                    && old_id != child
                {
                    build.retired.insert((Kind::of_child(level), old));
                }
                match child {
                    Some(id) => node.insert(edge, id),
                    None => node.remove(&edge),
                };
            }
            Ok(node)
        })
    }

    /// Grow with zero-edge wrappers, apply changes, then collapse redundant levels.
    /// `finalize_plan` prunes wrappers that end up unreachable.
    async fn rewrite_map(&self, build: &mut Build, changes: Refs) -> io::Result<(Node, u8)> {
        let mut node = self.head.root.clone();
        let mut height = self.head.height;
        let needed = changes.keys().map(|&i| height_for(i)).max().unwrap_or(0);
        while height < needed {
            if !node.is_empty() {
                let child = build.put(Kind::Node, map_codec::encode(&node))?;
                node = Node::from([(0, child)]);
            }
            height += 1;
        }
        if height > 0 {
            let changes: Vec<_> = changes.into_iter().collect();
            node = self.rewrite(build, node, height, &changes).await?;
        }
        while height > 1 && node.len() == 1 && node.contains_key(&0) {
            build.retired.insert((Kind::Node, node[&0]));
            node = self.staged_node(build, Some(node[&0])).await?;
            height -= 1;
        }
        if node.is_empty() {
            height = 0;
        }
        Ok((node, height))
    }

    /// Walk only newly staged nodes; old subtrees are reused without traversal.
    /// Collapse can promote old children, so retirement candidates are filtered
    /// against old references kept by the final plan. Temporary wrappers that
    /// became unreachable are dropped.
    fn finalize_plan(&self, build: &mut Build, root: &Node, height: u8) -> io::Result<()> {
        let committed = self.head.last_id;
        let mut live_new = BTreeSet::new();
        let mut kept_old = BTreeSet::new();
        let mut pending: Vec<_> = root.values().map(|&id| (id, height)).collect();
        while let Some((id, level)) = pending.pop() {
            let kind = Kind::of_child(level);
            if id <= committed {
                kept_old.insert((kind, id));
                continue;
            }
            if !live_new.insert(id) {
                continue;
            }
            match build.objects.get(&id) {
                Some((Kind::Node, bytes)) if kind == Kind::Node => pending.extend(
                    map_codec::decode(bytes, id - 1, false)?
                        .into_values()
                        .map(|child| (child, level - 1)),
                ),
                Some((actual, _)) if *actual == kind => {}
                Some(_) => return Err(invalid("invalid planned object kind")),
                None => return Err(invalid("missing planned object")),
            }
        }
        build.objects.retain(|id, _| live_new.contains(id));
        build
            .retired
            .retain(|entry| entry.1 <= committed && !kept_old.contains(entry));
        Ok(())
    }

    /// Commit `dirty` pages, by appending a log segment when the policy allows,
    /// otherwise by a full publication that also folds the log.
    ///
    /// Requires exclusive ownership until every request settles, even on
    /// cancellation. After an error, reopen; never reuse this snapshot for writes.
    pub async fn publish(&self, dirty: Changes, size: u64) -> io::Result<SaveReport> {
        if dirty.is_empty() && size == self.size() {
            return Ok(SaveReport::default());
        }
        let max = u64::from(self.options.max_log_segments);
        if self.head.log_start != 0 && self.log.count < max {
            let seq = self.head.log_start + self.log.count;
            let segment = log::encode(seq, size, &dirty, self.options.page_compression)?;
            if segment.len() <= MAX_SEGMENT_BYTES {
                // One durable put commits the save.
                self.store.put(&Kind::Log.key(seq), segment).await?;
                return Ok(SaveReport::default());
            }
        }
        let mut changes = self.log.pages.clone();
        changes.extend(dirty);
        self.fold(changes, size).await
    }

    /// Publish all log and session pages through the intent protocol, retiring
    /// the log segments. The new head enables the log only when the policy does.
    async fn fold(&self, dirty: Changes, size: u64) -> io::Result<SaveReport> {
        let log_end = self.head.log_start + self.log.count;
        let log_start = match (self.options.max_log_segments, self.head.log_start) {
            (0, _) => 0,
            (_, 0) => 1,
            _ => log_end,
        };
        let mut build = Build {
            last_id: self.head.last_id,
            objects: BTreeMap::new(),
            retired: Objects::new(),
        };
        let mut changes = Refs::new();
        for (index, page) in dirty {
            let id = match page {
                Some(page) => Some(build.put(
                    Kind::Block,
                    page_codec::encode(&page, self.options.page_compression)?,
                )?),
                None => None,
            };
            changes.insert(index, id);
        }
        let (root, height) = if changes.is_empty() {
            (self.head.root.clone(), self.head.height)
        } else {
            self.rewrite_map(&mut build, changes).await?
        };
        self.finalize_plan(&mut build, &root, height)?;
        if self.head.log_start != 0 {
            let segments = self.head.log_start..log_end;
            build.retired.extend(segments.map(|seq| (Kind::Log, seq)));
        }
        let head = Head {
            root,
            height,
            last_id: build.last_id,
            size,
            log_start,
        }
        .encode();
        if self.head_bytes.as_ref() == Some(&head)
            && build.objects.is_empty()
            && build.retired.is_empty()
        {
            return Ok(SaveReport::default());
        }
        let pending = PendingSave {
            old_head: self.head_bytes.clone(),
            new_head: head.clone(),
            created: build
                .objects
                .iter()
                .map(|(&id, &(kind, _))| (kind, id))
                .collect(),
            retired: build.retired,
        };
        let intent = pending.encode()?; // validates size and shape before mutating
        // Keep copies for the cache; they become cacheable once the head commits.
        let cached: Vec<_> = match self.options.cache {
            Some(_) => build
                .objects
                .iter()
                .map(|(&id, (kind, bytes))| ((*kind, id), Arc::<[u8]>::from(bytes.as_slice())))
                .collect(),
            None => Vec::new(),
        };

        let store = &*self.store;
        store.put(PENDING, intent).await?;
        let puts = build
            .objects
            .into_iter()
            .map(|(id, (kind, bytes))| Mutation::Put(kind.key(id), bytes))
            .collect();
        concurrently(store, puts).await?;
        // Every new object is durable; this put is the only publication point.
        store.put(HEAD, head).await?;

        if let Some(cache) = &self.options.cache {
            cache.set_log_start(log_start);
            for (object, bytes) in cached {
                cache.insert(object, bytes);
            }
            for &object in &pending.retired {
                cache.remove(object);
            }
        }
        Ok(SaveReport {
            retired_objects: pending.retired.len(),
            cleanup_error: pending.finish(store, true).await.err(),
        })
    }
}

#[cfg(all(test, feature = "simulation"))]
#[path = "../tests/unit/snapshot.rs"]
mod tests;

#[cfg(all(test, feature = "simulation"))]
#[path = "../tests/unit/snapshot_properties.rs"]
mod properties;

#[cfg(all(test, feature = "simulation"))]
#[path = "../tests/unit/snapshot_log.rs"]
mod log_tests;

#[cfg(all(test, feature = "simulation"))]
#[path = "../tests/unit/snapshot_support.rs"]
mod test_support;

#[cfg(test)]
#[path = "../tests/unit/head.rs"]
mod head_tests;
