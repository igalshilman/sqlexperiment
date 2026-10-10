//! Immutable pages with durable head publication and selectable map layouts.
use crate::{
    format::{HEAD, HEAD_MAGIC, PENDING, key},
    kv::KvStore,
    map_codec::{Node, decode as decode_node, encode as encode_node},
    page_codec::{self, PageCompression},
    pending::{Objects, PendingSave, SaveReport, recover},
};
use futures::{FutureExt, StreamExt, future::BoxFuture, stream};
use std::{
    collections::{BTreeMap, BTreeSet},
    io,
    sync::{Arc, Mutex},
};

const OVERRIDE_LIMIT: usize = 64;
type Id = u64;
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
    /// Encoding for newly written pages. Existing pages carry their own codec tag.
    pub page_compression: PageCompression,
}
impl Default for StorageOptions {
    fn default() -> Self {
        Self {
            layout: MapLayout::Radix,
            write_concurrency: 16,
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
        if bytes.len() < 26 || &bytes[..4] != HEAD_MAGIC {
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
    page_compression: PageCompression,
    pub size: u64,
    nodes: Mutex<BTreeMap<Id, Node>>,
    // One validated immutable page, scoped to this snapshot. No lock across await.
    block_cache: Mutex<Option<(Id, Vec<u8>)>>,
}

impl Snapshot {
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
            page_compression: options.page_compression,
            size: 0,
            nodes: Mutex::default(),
            block_cache: Mutex::default(),
        };
        let bytes = recover(result.store.clone(), result.concurrency).await?;
        if let Some(bytes) = bytes {
            result.load_head(&bytes)?;
            result.original_head = Some(bytes);
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
        let mut out = HEAD_MAGIC.to_vec();
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
        if let Some((cached_id, bytes)) = self.block_cache.lock().unwrap().as_ref()
            && *cached_id == id
        {
            return Ok(Some(bytes.clone()));
        }
        let bytes = page_codec::decode(&self.fetch("block", id).await?)?;
        *self.block_cache.lock().unwrap() = Some((id, bytes.clone()));
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
                Some(bytes) => {
                    Some(build.put("block", page_codec::encode(&bytes, self.page_compression)?)?)
                }
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
        let jobs = build.objects.into_iter().map(|(id, (kind, bytes))| {
            let store = self.store.clone();
            let object_key = key(kind, &id);
            async move { store.put(&object_key, bytes).await }.boxed()
        });
        let mut writes = stream::iter(jobs)
            .boxed()
            .buffer_unordered(self.concurrency);
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

#[cfg(all(test, feature = "simulation"))]
#[path = "../tests/unit/snapshot.rs"]
mod tests;

#[cfg(all(test, feature = "simulation"))]
#[path = "../tests/unit/snapshot_properties.rs"]
mod properties;

#[cfg(all(test, feature = "simulation"))]
#[path = "../tests/unit/snapshot_support.rs"]
mod test_support;
