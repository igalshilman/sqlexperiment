//! Immutable pages and a persistent radix map, published through one durable head.
//!
//! Logical page numbers are u64 addresses. A tree level consumes one byte of the
//! address, most significant relevant byte first; leaves map to encoded page IDs,
//! while internal edges point to immutable node IDs. Height grows only as needed
//! and collapses through a sole zero edge when high pages disappear. An absent edge
//! represents a zero-filled hole, not an object to fetch.
//!
//! Radix addressing fits fixed-width integer page numbers: it needs no separator
//! keys, node splits or balancing, and has at most eight levels. A height-one map
//! covers 256 pages with no external nodes. A flat page-ID array would rewrite
//! metadata proportional to the database size; path copying limits updates to
//! touched branches. SQLite's own table/index B-trees remain inside the pages;
//! this outer map only translates page numbers to immutable object IDs.
//!
//! The root is always embedded in [`Head`]. Only lower nodes occupy separate KV
//! objects. [`rewrite`](Snapshot::rewrite) copies touched paths and records replaced
//! edges; [`finalize_plan`](Snapshot::finalize_plan) walks newly staged objects only,
//! prunes temporary wrappers and protects reused old references from retirement.
//! Old subtrees are shared without a whole-tree scan.
//!
//! [`Build`] allocates from one monotonic counter shared by pages and nodes. The
//! plan's metadata is validated against the pending-record limit before mutation;
//! staged payloads have no separate byte budget.
//! Save then writes intent, new objects, head, retired-object deletes and intent
//! removal, awaiting each mutation separately. The head put is the commit point;
//! cleanup failure does not undo it. See [`crate::pending`] for unknown outcomes.
//!
//! A snapshot has a decoded-node cache and one decoded-page cache, both scoped to
//! that snapshot. Locks are released before backend awaits. Immutable bytes make
//! concurrent reads safe; concurrent cache misses may perform duplicate gets. Old
//! snapshot readers must stop before save retires replaced objects: structural
//! sharing here is an update optimization, not historical-reader support.
use crate::{
    format::{HEAD, HEAD_MAGIC, PENDING, key},
    kv::KvStore,
    map_codec::{Node, decode as decode_node, encode as encode_node},
    page_codec::{self, PageCompression},
    pending::{Objects, PendingSave, SaveReport, recover},
};
use futures::future::BoxFuture;
use std::{
    collections::{BTreeMap, BTreeSet},
    io,
    sync::{Arc, Mutex},
};

/// Immutable object identity; shared allocation space for map nodes and pages.
type Id = u64;
/// Logical page replacements; `None` removes a page edge (a sparse hole).
type Changes = BTreeMap<u64, Option<Vec<u8>>>;
/// The same changes after page bytes have received immutable object IDs.
type Refs = BTreeMap<u64, Option<Id>>;

/// Encoding policy for newly written pages; layout and mutation ordering are fixed.
/// Both raw and compressed pages remain readable under either policy.
#[derive(Clone, Copy, Debug)]
pub struct StorageOptions {
    /// Encoding for newly written pages. Existing pages carry their own codec tag.
    pub page_compression: PageCompression,
}
impl Default for StorageOptions {
    fn default() -> Self {
        Self {
            page_compression: PageCompression::Lz4,
        }
    }
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
/// Entire write plan is constructed before any mutations. IDs above the old
/// head's counter can be reused after recovery has removed an aborted attempt.
struct Build {
    /// High-water mark, including any temporary objects later pruned from the plan.
    last_id: Id,
    /// New immutable objects keyed by ID; ascending order writes children before parents.
    objects: BTreeMap<Id, (&'static str, Vec<u8>)>,
    /// Replaced committed objects, filtered against references retained by the new root.
    retired: Objects,
}
impl Build {
    fn put(&mut self, kind: &'static str, bytes: Vec<u8>) -> io::Result<Id> {
        self.last_id = self
            .last_id
            .checked_add(1)
            .ok_or_else(|| invalid("object IDs exhausted"))?;
        self.objects.insert(self.last_id, (kind, bytes));
        Ok(self.last_id)
    }
}

/// Complete snapshot selection stored at the head key.
///
/// Encoding: magic `SK10` (4 bytes), database size (u64 LE), allocation counter
/// (u64 LE), height (u8), then the encoded root. The prefix is 21 bytes; the root
/// is at most 2305 bytes. Height zero iff the root is empty. There is no separate
/// root ID, layout tag, root length or override list. The node codec validates
/// the entire remainder, including child IDs bounded by the allocation counter.
#[derive(Default)]
struct Head {
    /// First radix level; edge targets are pages at height one, nodes above it.
    root: Node,
    /// Number of byte-sized address digits, or zero for an empty page map.
    height: u8,
    /// Committed allocation high-water mark shared by node and page objects.
    last_id: Id,
    /// Logical database length in bytes; sparse holes still contribute to this size.
    size: u64,
}
impl Head {
    fn encode(&self) -> Vec<u8> {
        let mut out = HEAD_MAGIC.to_vec();
        out.extend_from_slice(&self.size.to_le_bytes());
        out.extend_from_slice(&self.last_id.to_le_bytes());
        out.push(self.height);
        out.extend(encode_node(&self.root));
        out
    }
    fn decode(bytes: &[u8]) -> io::Result<Self> {
        if bytes.len() < 21 || &bytes[..4] != HEAD_MAGIC {
            return Err(invalid("invalid snapshot head"));
        }
        let size = u64::from_le_bytes(bytes[4..12].try_into().unwrap());
        let last_id = u64::from_le_bytes(bytes[12..20].try_into().unwrap());
        let height = bytes[20];
        let root = decode_node(&bytes[21..], last_id, true)?;
        if height > 8 || root.is_empty() != (height == 0) {
            return Err(invalid("invalid root height"));
        }
        Ok(Self {
            root,
            height,
            last_id,
            size,
        })
    }
}

/// A recovered head plus immutable-object access and snapshot-scoped read caches.
/// `original_head` preserves the exact bytes needed for recovery's old/new comparison.
pub(crate) struct Snapshot {
    store: Arc<dyn KvStore>,
    original_head: Option<Vec<u8>>,
    root: Node,
    height: u8,
    last_id: Id,
    page_compression: PageCompression,
    pub size: u64,
    nodes: Mutex<BTreeMap<Id, Node>>,
    // One validated immutable page, scoped to this snapshot. No lock across await.
    block_cache: Mutex<Option<(Id, Vec<u8>)>>,
}

impl Snapshot {
    /// Resolve a pending save before exposing any page from the selected head.
    pub async fn open_with_options(
        store: Arc<dyn KvStore>,
        options: StorageOptions,
    ) -> io::Result<Self> {
        let mut result = Self {
            store,
            original_head: None,
            root: Node::new(),
            height: 0,
            last_id: 0,
            page_compression: options.page_compression,
            size: 0,
            nodes: Mutex::default(),
            block_cache: Mutex::default(),
        };
        let bytes = recover(result.store.clone()).await?;
        if let Some(bytes) = bytes {
            result.load_head(&bytes)?;
            result.original_head = Some(bytes);
        }

        Ok(result)
    }
    /// Check structure and references and return the allocation high-water mark.
    pub(crate) fn validate_head(bytes: &[u8]) -> io::Result<u64> {
        Ok(Head::decode(bytes)?.last_id)
    }
    fn load_head(&mut self, bytes: &[u8]) -> io::Result<()> {
        let head = Head::decode(bytes)?;
        self.root = head.root;
        self.height = head.height;
        self.last_id = head.last_id;
        self.size = head.size;
        Ok(())
    }
    async fn fetch(&self, kind: &str, id: Id) -> io::Result<Vec<u8>> {
        self.store
            .get(&key(kind, &id))
            .await?
            .ok_or_else(|| invalid("missing referenced snapshot object"))
    }
    async fn node(&self, id: Option<Id>) -> io::Result<Node> {
        let Some(id) = id else {
            return Ok(Node::new());
        };
        if let Some(node) = self.nodes.lock().unwrap().get(&id).cloned() {
            return Ok(node);
        }
        let node = decode_node(&self.fetch("node", id).await?, id - 1, false)?;
        self.nodes.lock().unwrap().insert(id, node.clone());
        Ok(node)
    }
    async fn staged_node(&self, build: &Build, id: Option<Id>) -> io::Result<Node> {
        if let Some(id) = id
            && let Some((kind, bytes)) = build.objects.get(&id)
        {
            if *kind != "node" {
                return Err(invalid("expected index node"));
            }
            return decode_node(bytes, id - 1, false);
        }
        self.node(id).await
    }
    async fn block_id(&self, index: u64) -> io::Result<Option<Id>> {
        if height_for(index) > self.height {
            return Ok(None);
        }
        let mut node = self.root.clone();
        for level in (1..=self.height).rev() {
            let id = node.get(&digit(index, level)).copied();
            if level == 1 {
                return Ok(id);
            }
            if id.is_none() {
                return Ok(None);
            }
            node = self.node(id).await?;
        }
        Ok(None)
    }
    /// Resolve a logical page and decode its immutable payload; `None` means a hole.
    pub async fn block(&self, index: u64) -> io::Result<Option<Vec<u8>>> {
        let Some(id) = self.block_id(index).await? else {
            return Ok(None);
        };
        if let Some((cached_id, bytes)) = self.block_cache.lock().unwrap().as_ref()
            && *cached_id == id
        {
            return Ok(Some(bytes.clone()));
        }
        let bytes = page_codec::decode(&self.fetch("block", id).await?)?;
        *self.block_cache.lock().unwrap() = Some((id, bytes.clone()));
        Ok(Some(bytes))
    }
    /// Group sorted changes by this level's digit, recursively copy only touched
    /// branches and collect replaced child IDs. Unchanged edges keep their IDs.
    fn rewrite<'a>(
        &'a self,
        build: &'a mut Build,
        mut node: Node,
        level: u8,
        changes: &'a [(u64, Option<Id>)],
    ) -> BoxFuture<'a, io::Result<Node>> {
        Box::pin(async move {
            let mut start = 0;
            while start < changes.len() {
                let edge = digit(changes[start].0, level);
                let mut end = start + 1;
                while end < changes.len() && digit(changes[end].0, level) == edge {
                    end += 1;
                }
                let child = if level == 1 {
                    changes[start].1
                } else {
                    let old_id = node.get(&edge).copied();
                    let old = self.staged_node(build, old_id).await?;
                    let updated = self
                        .rewrite(build, old.clone(), level - 1, &changes[start..end])
                        .await?;
                    if updated == old {
                        old_id
                    } else if updated.is_empty() {
                        None
                    } else {
                        Some(build.put("node", encode_node(&updated))?)
                    }
                };
                if let Some(old) = node.get(&edge).copied()
                    && Some(old) != child
                {
                    build
                        .retired
                        .insert((if level == 1 { "block" } else { "node" }, old));
                }
                match child {
                    Some(id) => {
                        node.insert(edge, id);
                    }
                    None => {
                        node.remove(&edge);
                    }
                }
                start = end;
            }
            Ok(node)
        })
    }
    /// Grow with zero-edge wrappers, apply changes, then collapse redundant levels.
    /// Temporary wrappers are pruned by `finalize_plan` before any object is written.
    async fn rewrite_map(&self, build: &mut Build, changes: Refs) -> io::Result<(Node, u8)> {
        let mut node = self.root.clone();
        let mut height = self.height;
        let needed = changes
            .keys()
            .map(|i| height_for(*i))
            .max()
            .unwrap_or(0)
            .max(height);
        while height < needed {
            if !node.is_empty() {
                let child = build.put("node", encode_node(&node))?;
                node = Node::from([(0, child)]);
            }
            height += 1;
        }
        if height > 0 {
            node = self
                .rewrite(
                    build,
                    node,
                    height,
                    &changes.into_iter().collect::<Vec<_>>(),
                )
                .await?;
        }
        while height > 1 && node.len() == 1 && node.contains_key(&0) {
            build.retired.insert(("node", node[&0]));
            node = self.staged_node(build, node.get(&0).copied()).await?;
            height -= 1;
        }
        if node.is_empty() {
            height = 0;
        }
        Ok((node, height))
    }

    /// Walk only newly staged nodes. Old subtrees are reused without traversal.
    /// Rewrites retire replaced edges; collapse can promote old children, so
    /// filter those candidates against references in the final plan.
    /// Also discard temporary wrappers that became unreachable during planning.
    fn finalize_plan(&self, build: &mut Build, root: &Node, height: u8) -> io::Result<()> {
        let mut live_new = BTreeSet::new();
        let mut kept_old = BTreeSet::new();
        let mut pending = Vec::new();
        for id in root.values() {
            pending.push((
                if height == 1 { "block" } else { "node" },
                *id,
                height.saturating_sub(1),
            ));
        }
        while let Some((kind, id, level)) = pending.pop() {
            if id <= self.last_id {
                kept_old.insert((kind, id));
                continue;
            }
            if !live_new.insert(id) {
                continue;
            }
            let (actual_kind, bytes) = build
                .objects
                .get(&id)
                .ok_or_else(|| invalid("missing planned object"))?;
            if *actual_kind != kind {
                return Err(invalid("invalid planned object kind"));
            }
            if kind == "node" {
                for child in decode_node(bytes, id - 1, false)?.into_values() {
                    pending.push((
                        if level == 1 { "block" } else { "node" },
                        child,
                        level.saturating_sub(1),
                    ));
                }
            }
        }
        build.objects.retain(|id, _| live_new.contains(id));
        build
            .retired
            .retain(|entry| entry.1 <= self.last_id && !kept_old.contains(entry));
        Ok(())
    }
    /// Exclusive ownership must persist until all requests settle, even on
    /// cancellation. Reopen after failure; never reuse this Snapshot for writes.
    pub async fn publish(&self, dirty: Changes, size: u64) -> io::Result<SaveReport> {
        let mut build = Build {
            last_id: self.last_id,
            objects: BTreeMap::new(),
            retired: Objects::new(),
        };
        let mut changes = Refs::new();
        for (index, value) in dirty {
            let id = match value {
                Some(bytes) => {
                    Some(build.put("block", page_codec::encode(&bytes, self.page_compression)?)?)
                }
                None => None,
            };
            changes.insert(index, id);
        }
        let (root, height) = if changes.is_empty() {
            (self.root.clone(), self.height)
        } else {
            self.rewrite_map(&mut build, changes).await?
        };
        self.finalize_plan(&mut build, &root, height)?;
        let retired_objects = build.retired.len();
        let head = Head {
            root,
            height,
            last_id: build.last_id,
            size,
        }
        .encode();
        if self.original_head.as_ref() == Some(&head)
            && build.objects.is_empty()
            && build.retired.is_empty()
        {
            return Ok(SaveReport::default());
        }
        let pending = PendingSave {
            old_head: self.original_head.clone(),
            new_head: head.clone(),
            created: build
                .objects
                .iter()
                .map(|(id, (kind, _))| (*kind, *id))
                .collect(),
            retired: build.retired,
        };
        let intent = pending.encode()?; // size/shape validation before any mutation
        self.store.put(PENDING, intent).await?;
        // The backend permits one mutation at a time. On failure leave the
        // complete intent in place; recovery also deletes never-written IDs.
        for (id, (kind, bytes)) in build.objects {
            self.store.put(&key(kind, &id), bytes).await?;
        }
        // Every referenced new object is now durably acknowledged. This atomic,
        // durable head replacement is the only publication point.
        self.store.put(HEAD, head).await?;
        let cleanup_error = pending.finish(self.store.clone(), true).await.err();
        Ok(SaveReport {
            retired_objects,
            cleanup_error,
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
#[path = "../tests/unit/snapshot_support.rs"]
mod test_support;

#[cfg(test)]
#[path = "../tests/unit/head.rs"]
mod head_tests;
