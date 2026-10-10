//! Immutable pages with durable head publication and selectable map layouts.
use crate::{
    io::BLOCK_SIZE,
    kv::KvStore,
    pending::{Objects, PENDING, PendingSave, SaveReport, recover},
};
use futures::{StreamExt, future::BoxFuture, stream};
use std::{
    collections::{BTreeMap, BTreeSet},
    io,
    sync::{Arc, Mutex},
};

pub const HEAD: &str = "v7/head";
const OVERRIDE_LIMIT: usize = 64;
type Id = u64;
type Node = BTreeMap<u8, Id>;
type Changes = BTreeMap<u64, Option<Vec<u8>>>;
type Refs = BTreeMap<u64, Option<Id>>;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum MapLayout {
    #[default]
    Radix = 0,
    InlineRoot = 1,
    HeadOverrides = 2,
}

#[derive(Clone, Copy, Debug)]
pub struct StorageOptions {
    /// Used for a new store. An existing head selects its own persisted layout.
    pub layout: MapLayout,
    /// Bounds outstanding object puts/deletes, including recovery. Head publication remains ordered.
    pub write_concurrency: usize,
}
impl Default for StorageOptions {
    fn default() -> Self {
        Self {
            layout: MapLayout::Radix,
            write_concurrency: 16,
        }
    }
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
pub(crate) fn key(kind: &str, id: &Id) -> String {
    format!("v7/{kind}/{id:016x}")
}
fn height_for(index: u64) -> u8 {
    ((64 - index.leading_zeros()).max(1).div_ceil(8)) as u8
}
fn digit(index: u64, level: u8) -> u8 {
    (index >> (8 * (level - 1))) as u8
}
fn encode_node(node: &Node) -> Vec<u8> {
    let mut out = Vec::with_capacity(node.len() * 9);
    for (edge, id) in node {
        out.push(*edge);
        out.extend_from_slice(&id.to_le_bytes());
    }
    out
}
fn decode_node(bytes: &[u8], max_id: Id, allow_empty: bool) -> io::Result<Node> {
    if (!allow_empty && bytes.is_empty()) || !bytes.len().is_multiple_of(9) || bytes.len() > 256 * 9
    {
        return Err(invalid("invalid radix node"));
    }
    let mut node = Node::new();
    let mut previous = None;
    for entry in bytes.as_chunks::<9>().0 {
        let child = u64::from_le_bytes(entry[1..].try_into().unwrap());
        if child == 0 || child > max_id || previous.is_some_and(|edge| edge >= entry[0]) {
            return Err(invalid("invalid radix edge/child ID"));
        }
        node.insert(entry[0], child);
        previous = Some(entry[0]);
    }
    Ok(node)
}

/// Entire write plan is constructed before any mutations. IDs above the old
/// head's counter can be reused after recovery has removed an aborted attempt.
struct Build {
    last_id: Id,
    objects: BTreeMap<Id, (&'static str, Vec<u8>)>,
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

#[derive(Default)]
struct Head {
    root: Option<Id>,
    inline: Node,
    overrides: Refs,
    height: u8,
    last_id: Id,
    layout: MapLayout,
    size: u64,
}
impl Head {
    fn decode(bytes: &[u8]) -> io::Result<Self> {
        let mut result = Self::default();
        if bytes.len() < 26 || &bytes[..4] != b"TKV7" {
            return Err(invalid("invalid snapshot head"));
        }
        result.size = u64::from_le_bytes(bytes[4..12].try_into().unwrap());
        result.last_id = u64::from_le_bytes(bytes[12..20].try_into().unwrap());
        result.height = bytes[20];
        result.layout = match bytes[21] {
            0 => MapLayout::Radix,
            1 => MapLayout::InlineRoot,
            2 => MapLayout::HeadOverrides,
            _ => return Err(invalid("invalid map layout")),
        };
        let root_len = u16::from_le_bytes(bytes[22..24].try_into().unwrap()) as usize;
        let count = u16::from_le_bytes(bytes[24..26].try_into().unwrap()) as usize;
        if result.height > 8 || count > OVERRIDE_LIMIT || bytes.len() != 26 + root_len + count * 16
        {
            return Err(invalid("invalid snapshot head lengths"));
        }
        let root_bytes = &bytes[26..26 + root_len];
        if result.layout == MapLayout::InlineRoot {
            result.inline = decode_node(root_bytes, result.last_id, true)?;
            if result.inline.is_empty() != (result.height == 0) {
                return Err(invalid("invalid inline root height"));
            }
        } else {
            if root_len != 8 {
                return Err(invalid("invalid root ID length"));
            }
            let id = u64::from_le_bytes(root_bytes.try_into().unwrap());
            if id > result.last_id || (id == 0) != (result.height == 0) {
                return Err(invalid("invalid root ID/height"));
            }
            result.root = (id != 0).then_some(id);
        }
        if count != 0 && result.layout != MapLayout::HeadOverrides {
            return Err(invalid("unexpected overrides"));
        }
        let mut previous = None;
        let map_end = 26 + root_len + count * 16;
        for entry in bytes[26 + root_len..map_end].as_chunks::<16>().0 {
            let index = u64::from_le_bytes(entry[..8].try_into().unwrap());
            let id = u64::from_le_bytes(entry[8..].try_into().unwrap());
            if id > result.last_id || previous.is_some_and(|i| i >= index) {
                return Err(invalid("invalid override"));
            }
            result.overrides.insert(index, (id != 0).then_some(id));
            previous = Some(index);
        }
        Ok(result)
    }
}

pub(crate) struct Snapshot {
    store: Arc<dyn KvStore>,
    original_head: Option<Vec<u8>>,
    root: Option<Id>,
    inline: Node,
    overrides: Refs,
    height: u8,
    last_id: Id,
    layout: MapLayout,
    concurrency: usize,
    pub size: u64,
    nodes: Mutex<BTreeMap<Id, Node>>,
}

impl Snapshot {
    #[cfg(test)]
    pub async fn open(store: Arc<dyn KvStore>) -> io::Result<Self> {
        Self::open_with_options(store, StorageOptions::default()).await
    }
    pub async fn open_with_options(
        store: Arc<dyn KvStore>,
        options: StorageOptions,
    ) -> io::Result<Self> {
        if options.write_concurrency == 0 {
            return Err(invalid("zero write concurrency"));
        }
        let mut result = Self {
            store,
            original_head: None,
            root: None,
            inline: Node::new(),
            overrides: Refs::new(),
            height: 0,
            last_id: 0,
            layout: options.layout,
            concurrency: options.write_concurrency,
            size: 0,
            nodes: Mutex::default(),
        };
        let bytes = recover(result.store.clone(), result.concurrency).await?;
        if let Some(bytes) = bytes {
            result.load_head(&bytes)?;
            result.original_head = Some(bytes);
        } else {
            for legacy in [
                "v6/head",
                "v5/head",
                "v5/allocated",
                "v4/head",
                "v3/head",
                "v2/head",
                "object.db/size",
                "object.db-wal/size",
            ] {
                if result.store.get(legacy).await?.is_some() {
                    return Err(invalid(
                        "older storage format requires migration; use a fresh namespace",
                    ));
                }
            }
        }
        Ok(result)
    }
    pub(crate) fn validate_head(bytes: &[u8]) -> io::Result<u64> {
        Ok(Head::decode(bytes)?.last_id)
    }
    fn load_head(&mut self, bytes: &[u8]) -> io::Result<()> {
        let head = Head::decode(bytes)?;
        self.root = head.root;
        self.inline = head.inline;
        self.overrides = head.overrides;
        self.height = head.height;
        self.last_id = head.last_id;
        self.layout = head.layout;
        self.size = head.size;
        Ok(())
    }
    fn head(
        &self,
        root: Option<Id>,
        inline: &Node,
        overrides: &Refs,
        height: u8,
        last_id: Id,
        size: u64,
    ) -> Vec<u8> {
        let root_bytes = if self.layout == MapLayout::InlineRoot {
            encode_node(inline)
        } else {
            root.unwrap_or(0).to_le_bytes().to_vec()
        };
        let mut out = b"TKV7".to_vec();
        out.extend_from_slice(&size.to_le_bytes());
        out.extend_from_slice(&last_id.to_le_bytes());
        out.push(height);
        out.push(self.layout as u8);
        out.extend_from_slice(&(root_bytes.len() as u16).to_le_bytes());
        out.extend_from_slice(&(overrides.len() as u16).to_le_bytes());
        out.extend(root_bytes);
        for (index, id) in overrides {
            out.extend_from_slice(&index.to_le_bytes());
            out.extend_from_slice(&id.unwrap_or(0).to_le_bytes());
        }
        out
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
    async fn root_node(&self) -> io::Result<Node> {
        if self.layout == MapLayout::InlineRoot {
            Ok(self.inline.clone())
        } else {
            self.node(self.root).await
        }
    }
    async fn block_id(&self, index: u64) -> io::Result<Option<Id>> {
        if let Some(id) = self.overrides.get(&index) {
            return Ok(*id);
        }
        if height_for(index) > self.height {
            return Ok(None);
        }
        let mut node = self.root_node().await?;
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
    pub async fn block(&self, index: u64) -> io::Result<Option<Vec<u8>>> {
        let Some(id) = self.block_id(index).await? else {
            return Ok(None);
        };
        let bytes = self.fetch("block", id).await?;
        if bytes.len() != BLOCK_SIZE {
            return Err(invalid("invalid block length"));
        }
        Ok(Some(bytes))
    }
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
    async fn rewrite_map(
        &self,
        build: &mut Build,
        changes: Refs,
    ) -> io::Result<(Option<Id>, Node, u8)> {
        let original = self.root_node().await?;
        let mut node = original.clone();
        let mut height = self.height;
        let needed = changes
            .keys()
            .map(|i| height_for(*i))
            .max()
            .unwrap_or(0)
            .max(height);
        let mut root_id = self.root;
        while height < needed {
            if !node.is_empty() {
                let child = match root_id {
                    Some(id) => id,
                    None => build.put("node", encode_node(&node))?,
                };
                node = Node::from([(0, child)]);
                root_id = None;
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
        if self.layout == MapLayout::InlineRoot {
            return Ok((None, node, height));
        }
        let root = if node.is_empty() {
            None
        } else if height == self.height && node == original {
            self.root
        } else {
            Some(build.put("node", encode_node(&node))?)
        };
        Ok((root, Node::new(), height))
    }
    /// Walk only newly staged nodes. Old subtrees are reused without traversal.
    /// Rewrites retire replaced edges; growth/collapse can still reuse an old
    /// root, so filter those candidates against references in the final plan.
    /// Also discard temporary wrappers that became unreachable during planning.
    fn finalize_plan(
        &self,
        build: &mut Build,
        root: Option<Id>,
        inline: &Node,
        overrides: &Refs,
        height: u8,
    ) -> io::Result<()> {
        let mut live_new = BTreeSet::new();
        let mut kept_old = BTreeSet::new();
        let mut pending = Vec::new();
        if let Some(id) = root {
            pending.push(("node", id, height));
        }
        for id in inline.values() {
            pending.push((
                if height == 1 { "block" } else { "node" },
                *id,
                height.saturating_sub(1),
            ));
        }
        for id in overrides.values().flatten() {
            pending.push(("block", *id, 0));
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
    /// Conservative marking: overridden base blocks stay live until a fold.
    /// No database payload reads. Inline roots are already inside the head.
    #[cfg(test)]
    pub(crate) async fn reachable(&self) -> io::Result<BTreeSet<String>> {
        let mut live = BTreeSet::new();
        let mut visited = BTreeSet::new();
        let mut pending = Vec::new();
        if let Some(root) = self.root {
            pending.push((Some(root), self.height));
        } else if self.layout == MapLayout::InlineRoot && self.height > 0 {
            pending.push((None, self.height));
        }
        while let Some((id, level)) = pending.pop() {
            if !visited.insert((id, level)) {
                continue;
            }
            let node = match id {
                Some(id) => {
                    live.insert(key("node", &id));
                    self.node(Some(id)).await?
                }
                None => self.inline.clone(),
            };
            for child in node.into_values() {
                if level == 1 {
                    live.insert(key("block", &child));
                } else {
                    pending.push((Some(child), level - 1));
                }
            }
        }
        for id in self.overrides.values().flatten() {
            live.insert(key("block", id));
        }
        Ok(live)
    }
    /// Exclusive ownership must persist until all requests settle, even on
    /// cancellation. Reopen after failure; never reuse this Snapshot for writes.
    pub async fn publish(&self, dirty: Changes, size: u64) -> io::Result<SaveReport> {
        let mut build = Build {
            last_id: self.last_id,
            objects: BTreeMap::new(),
            retired: Objects::new(),
        };
        let mut changes = self.overrides.clone();
        for (index, value) in dirty {
            if let Some(Some(old)) = self.overrides.get(&index) {
                build.retired.insert(("block", *old));
            }
            let id = match value {
                Some(bytes) if bytes.len() == BLOCK_SIZE => Some(build.put("block", bytes)?),
                Some(_) => return Err(invalid("invalid block length")),
                None => None,
            };
            changes.insert(index, id);
        }
        let (root, inline, height, overrides) = if self.layout == MapLayout::HeadOverrides
            && changes.len() <= OVERRIDE_LIMIT
            && size >= self.size
        {
            (self.root, Node::new(), self.height, changes)
        } else if changes.is_empty() {
            (self.root, self.inline.clone(), self.height, Refs::new())
        } else {
            let (root, inline, height) = self.rewrite_map(&mut build, changes).await?;
            (root, inline, height, Refs::new())
        };
        if let Some(old) = self.root {
            build.retired.insert(("node", old));
        }
        self.finalize_plan(&mut build, root, &inline, &overrides, height)?;
        let retired_objects = build.retired.len();
        let head = self.head(root, &inline, &overrides, height, build.last_id, size);
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
        // Parent references are already known; all object puts may overlap.
        // Drain all futures on ordinary errors. Cancellation still requires host fencing/drain.
        let jobs: Vec<BoxFuture<'static, io::Result<()>>> = build
            .objects
            .into_iter()
            .map(|(id, (kind, bytes))| {
                let store = self.store.clone();
                let object_key = key(kind, &id);
                Box::pin(async move { store.put(&object_key, bytes).await })
                    as BoxFuture<'static, io::Result<()>>
            })
            .collect();
        let mut writes = stream::iter(jobs).buffer_unordered(self.concurrency);
        let mut error = None;
        while let Some(result) = writes.next().await {
            if let Err(e) = result {
                error.get_or_insert(e);
            }
        }
        if let Some(error) = error {
            return Err(error);
        }
        // Every referenced new object is now durably acknowledged. This atomic,
        // durable head replacement is the only publication point.
        self.store.put(HEAD, head).await?;
        let cleanup_error = pending
            .finish(self.store.clone(), true, self.concurrency)
            .await
            .err();
        Ok(SaveReport {
            retired_objects,
            cleanup_error,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kv::MemoryKv;
    #[test]
    fn height_grows_shrinks_and_small_reads_use_one_index_node() -> io::Result<()> {
        futures::executor::block_on(async {
            let store = Arc::new(MemoryKv::default());
            let snapshot = Snapshot::open(store.clone()).await?;
            snapshot
                .publish(
                    BTreeMap::from([
                        (0, Some(vec![1; BLOCK_SIZE])),
                        (255, Some(vec![2; BLOCK_SIZE])),
                    ]),
                    256 * BLOCK_SIZE as u64,
                )
                .await?;
            let mut snapshot = Snapshot::open(store.clone()).await?;
            assert_eq!(snapshot.height, 1);
            for (index, expected) in [(256, 2), (65536, 3), (1 << 40, 6)] {
                assert!(snapshot.block(index).await?.is_none());
                snapshot
                    .publish(
                        BTreeMap::from([(index, Some(vec![7; BLOCK_SIZE]))]),
                        (index + 1) * BLOCK_SIZE as u64,
                    )
                    .await?;
                snapshot = Snapshot::open(store.clone()).await?;
                assert_eq!(snapshot.height, expected);
                assert_eq!(snapshot.block(index).await?, Some(vec![7; BLOCK_SIZE]));
                assert_eq!(snapshot.block(0).await?, Some(vec![1; BLOCK_SIZE]));
                assert!(snapshot.block(index + 1).await?.is_none());
            }
            snapshot
                .publish(
                    BTreeMap::from([(256, None), (65536, None), (1 << 40, None)]),
                    256 * BLOCK_SIZE as u64,
                )
                .await?;
            store.reset_stats();
            let snapshot = Snapshot::open(store.clone()).await?;
            assert_eq!(snapshot.height, 1);
            assert_eq!(snapshot.block(255).await?, Some(vec![2; BLOCK_SIZE]));
            assert_eq!(
                store.stats().gets,
                4,
                "pending check + one head + one index node + one block"
            );
            assert!(snapshot.block(256).await?.is_none());
            assert_eq!(store.stats().gets, 4, "outside tree range requires no read");
            snapshot
                .publish(BTreeMap::from([(0, None), (255, None)]), 0)
                .await?;
            let empty = Snapshot::open(store).await?;
            assert_eq!(empty.height, 0);
            assert!(empty.root.is_none());
            Ok(())
        })
    }

    #[test]
    fn persistent_tree_sparse_paths_updates_and_deletions() -> io::Result<()> {
        futures::executor::block_on(async {
            let store = Arc::new(MemoryKv::default());
            let mut snapshot = Snapshot::open(store.clone()).await?;
            let indexes = [0, 1, 255, 256, 257, 65535, 65536, 1 << 40];
            let mut dirty = Changes::new();
            for (i, &index) in indexes.iter().enumerate() {
                dirty.insert(index, Some(vec![i as u8 + 1; BLOCK_SIZE]));
            }
            snapshot
                .publish(dirty, ((1 << 40) + 1) * BLOCK_SIZE as u64)
                .await?;
            snapshot = Snapshot::open(store.clone()).await?;
            assert!(snapshot.block(99).await?.is_none());
            for (i, &index) in indexes.iter().enumerate() {
                assert_eq!(
                    snapshot.block(index).await?.unwrap(),
                    vec![i as u8 + 1; BLOCK_SIZE]
                );
            }
            let mut dirty = Changes::new();
            dirty.insert(256, None);
            dirty.insert(255, Some(vec![99; BLOCK_SIZE]));
            dirty.insert(258, Some(vec![42; BLOCK_SIZE]));
            snapshot.publish(dirty, snapshot.size).await?;
            let updated = Snapshot::open(store.clone()).await?;
            assert!(updated.block(256).await?.is_none());
            assert_eq!(updated.block(255).await?.unwrap(), vec![99; BLOCK_SIZE]);
            assert_eq!(updated.block(258).await?.unwrap(), vec![42; BLOCK_SIZE]);
            // Saving invalidates old snapshots; their replaced blocks are reclaimed.
            assert!(snapshot.block(255).await.is_err());
            let mut dirty: Changes = indexes.iter().map(|&i| (i, None)).collect();
            dirty.insert(258, None);
            updated.publish(dirty, 0).await?;
            let empty = Snapshot::open(store).await?;
            assert_eq!(empty.root, None);
            assert_eq!(empty.size, 0);
            Ok(())
        })
    }

    #[test]
    fn referenced_objects_must_exist_and_have_valid_structure() -> io::Result<()> {
        futures::executor::block_on(async {
            let store = Arc::new(MemoryKv::default());
            let snapshot = Snapshot::open(store.clone()).await?;
            let bytes = vec![7; BLOCK_SIZE];
            snapshot
                .publish(
                    BTreeMap::from([(0, Some(bytes.clone()))]),
                    BLOCK_SIZE as u64,
                )
                .await?;
            let block_key = key("block", &1);
            store.delete(&block_key).await?;
            let snapshot = Snapshot::open(store.clone()).await?;
            assert!(
                snapshot
                    .block(0)
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("missing referenced")
            );
            store.put(&block_key, vec![8; BLOCK_SIZE - 1]).await?;
            assert!(
                snapshot
                    .block(0)
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("invalid")
            );
            let root_key = key("node", &snapshot.root.unwrap());
            store.put(&root_key, vec![0; 9]).await?;
            let snapshot = Snapshot::open(store.clone()).await?;
            assert!(
                snapshot
                    .block(0)
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("invalid")
            );
            store.put(HEAD, b"bad head".to_vec()).await?;
            assert!(Snapshot::open(store.clone()).await.is_err());
            store.delete(HEAD).await?;
            store
                .put("object.db/size", 4096u64.to_le_bytes().to_vec())
                .await?;
            assert!(Snapshot::open(store).await.is_err());
            Ok(())
        })
    }
}

#[cfg(test)]
#[path = "snapshot_tests.rs"]
mod advanced_tests;
