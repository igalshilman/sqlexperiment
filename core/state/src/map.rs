//! A paged file's chunk map: a copy-on-write radix tree over its chunk
//! numbers, like ZFS's indirect blocks. Its top node is in the root; the
//! nodes below it are blobs, written once, as chunks are.
//!
//! A node holds up to `fanout` entries: a leaf, the [`Chunk`]s of
//! consecutive chunk numbers; an inner node, the blob numbers of the nodes
//! below it, `None` for a subtree of holes. A file of at most `fanout` chunks,
//! as most are, is a leaf in the root: nothing more to read or write. Each
//! level more multiplies what the map holds by `fanout`, and costs a cold
//! read a round trip, and a flush a node on each path it changes.
//!
//! A node's place is counted from the leaves: level `k`, index `i` covers
//! `fanout^(k+1)` chunks from `i * fanout^(k+1)` on, however high the tree,
//! so a flush that grows or shrinks it keeps every subtree it does not
//! change. Nodes are encoded as the root encodes chunks (each blob number a
//! difference from the one before), and zstd folds runs of them, so a file
//! written in order maps in a few bytes, as an extent map would.
use crate::store::{Chunk, Reading, get_ref, put_ref, put_varint};
use crate::zstd;
use bytes::Bytes;
use std::{cell::RefCell, collections::BTreeMap, io, sync::Arc};

/// The default of [`crate::StateOptions::fanout`].
pub const FANOUT: u64 = 64;
/// Most entries a node holds.
pub(crate) const MAX_FANOUT: u64 = 4096;

/// What a map holds: `count` chunks, at most `fanout` entries a node.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Shape {
    pub fanout: u64,
    pub count: u64,
}

impl Shape {
    /// Its levels: the fewest that hold its chunks (1: the root's node is
    /// a leaf).
    pub fn height(self) -> u32 {
        let (mut height, mut holds) = (1, self.fanout);
        while holds < self.count {
            holds = holds.saturating_mul(self.fanout);
            height += 1;
        }
        height
    }

    /// Nodes below the top, at most (fewer where subtrees are holes).
    pub fn nodes(self) -> u64 {
        (0..self.height() - 1)
            .map(|k| self.count.div_ceil(self.span(k + 1)))
            .sum()
    }

    /// Chunks under one entry of a node at level `k`.
    fn span(self, k: u32) -> u64 {
        self.fanout.saturating_pow(k)
    }

    /// The first chunk under the node at level `k`, index `i`.
    fn start(self, k: u32, i: u64) -> u64 {
        i.saturating_mul(self.span(k + 1))
    }

    /// Entries of the node at level `k`, index `i`: as many as its chunks
    /// below `count` need.
    pub fn slots(self, k: u32, i: u64) -> u64 {
        let start = self.start(k, i);
        match self.count > start {
            true => (self.count - start).div_ceil(self.span(k)).min(self.fanout),
            false => 0,
        }
    }
}

/// A node of a map.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Node {
    /// The chunks of consecutive chunk numbers.
    Leaf(Vec<Chunk>),
    /// The blobs of the nodes below; `None` for a subtree of holes.
    Inner(Vec<Option<u64>>),
}

impl Node {
    fn holes(leaf: bool, slots: u64) -> Self {
        match leaf {
            true => Node::Leaf(vec![Chunk::default(); slots as usize]),
            false => Node::Inner(vec![None; slots as usize]),
        }
    }

    fn is_holes(&self) -> bool {
        match self {
            Node::Leaf(chunks) => chunks.iter().all(|&chunk| chunk == Chunk::default()),
            Node::Inner(children) => children.iter().all(Option::is_none),
        }
    }

    fn encode(&self, out: &mut Vec<u8>) {
        match self {
            Node::Leaf(chunks) => {
                let (mut bases, mut deltas) = (0, 0);
                for chunk in chunks {
                    put_ref(out, &mut bases, chunk.base);
                    put_ref(out, &mut deltas, chunk.delta);
                }
            }
            Node::Inner(children) => {
                let mut previous = 0;
                for &child in children {
                    put_ref(out, &mut previous, child);
                }
            }
        }
    }

    /// A node of `slots` entries, naming only blobs below `bound`.
    fn decode(r: &mut Reading, leaf: bool, slots: u64, bound: u64) -> io::Result<Self> {
        let mut previous = [0; 2];
        let mut next = |r: &mut Reading, stream: usize| match get_ref(r, &mut previous[stream])? {
            Some(n) if n >= bound => Err(r.invalid()),
            n => Ok(n),
        };
        Ok(match leaf {
            true => Node::Leaf(
                (0..slots)
                    .map(|_| {
                        let base = next(r, 0)?;
                        let delta = next(r, 1)?;
                        Ok(Chunk { base, delta })
                    })
                    .collect::<io::Result<_>>()?,
            ),
            false => Node::Inner((0..slots).map(|_| next(r, 0)).collect::<io::Result<_>>()?),
        })
    }

    /// The blob that holds it.
    pub fn frame(&self) -> io::Result<Bytes> {
        let mut body = Vec::new();
        self.encode(&mut body);
        zstd::compress(&body, &[], Bytes::copy_from_slice)
    }

    /// The node `frame` holds, as `need` describes it. It names only blobs
    /// below its own: a node is put after what it names, so no map loops.
    pub fn from_frame(frame: &[u8], need: Need) -> io::Result<Self> {
        let max = need.slots as usize * 2 * 10;
        let body = zstd::decompress_sized(frame, &[], max)?;
        let mut r = Reading::new(&body, "map node");
        let node = Node::decode(&mut r, need.leaf, need.slots, need.blob)?;
        r.end()?;
        Ok(node)
    }
}

/// A node blob a walk needs, and its place's shape.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Need {
    pub blob: u64,
    pub leaf: bool,
    pub slots: u64,
}

/// The nodes a walk reads: those `get` has. Any other reads as holes, and
/// is listed as missing: the caller fetches them and walks again, a level
/// further each time.
pub(crate) struct Nodes<'a> {
    get: &'a dyn Fn(u64) -> Option<Arc<Node>>,
    missing: RefCell<Vec<Need>>,
}

impl<'a> Nodes<'a> {
    pub fn new(get: &'a dyn Fn(u64) -> Option<Arc<Node>>) -> Self {
        Self {
            get,
            missing: RefCell::default(),
        }
    }

    fn node(&self, need: Need) -> Arc<Node> {
        (self.get)(need.blob).unwrap_or_else(|| {
            self.missing.borrow_mut().push(need);
            Arc::new(Node::holes(need.leaf, need.slots))
        })
    }

    /// The nodes the walk lacked: if any, what it found is incomplete.
    pub fn missing(self) -> Vec<Need> {
        let mut missing = self.missing.into_inner();
        missing.sort();
        missing.dedup();
        missing
    }
}

/// What a walk of a map meets: a node blob, or a stored chunk and its
/// number.
pub(crate) enum Visit {
    Node(u64),
    Chunk(u64, Chunk),
}

/// A file's map: its height, and its top node, which the root holds.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Map {
    pub height: u32,
    pub node: Arc<Node>,
}

impl Map {
    /// The map of a file without chunks.
    pub fn empty() -> Self {
        Self {
            height: 1,
            node: Arc::new(Node::Leaf(Vec::new())),
        }
    }

    pub fn encode(&self, out: &mut Vec<u8>) {
        put_varint(out, u64::from(self.height));
        self.node.encode(out);
    }

    /// A map of `shape`, naming only blobs below `bound`.
    pub fn decode(r: &mut Reading, shape: Shape, bound: u64) -> io::Result<Self> {
        let height = shape.height();
        if r.varint()? != u64::from(height) {
            return Err(r.invalid());
        }
        let slots = shape.slots(height - 1, 0);
        let node = Node::decode(r, height == 1, slots, bound)?;
        Ok(Self {
            height,
            node: Arc::new(node),
        })
    }

    fn top(&self) -> u32 {
        self.height - 1
    }

    /// Chunk `chunk`, below `shape.count`.
    pub fn get(&self, shape: Shape, chunk: u64, nodes: &Nodes) -> Chunk {
        let mut node = self.node.clone();
        for k in (0..self.height).rev() {
            let (child, blob) = match &*node {
                Node::Leaf(chunks) => return chunks[(chunk % shape.fanout) as usize],
                Node::Inner(children) => {
                    let child = chunk / shape.span(k);
                    match children[(child % shape.fanout) as usize] {
                        Some(blob) => (child, blob),
                        None => return Chunk::default(),
                    }
                }
            };
            node = nodes.node(Need {
                blob,
                leaf: k == 1,
                slots: shape.slots(k - 1, child),
            });
        }
        unreachable!("a map ends in leaves")
    }

    /// Every node blob below the top, and every stored chunk.
    pub fn walk(&self, shape: Shape, nodes: &Nodes, visit: &mut dyn FnMut(Visit)) {
        walk(&self.node, self.top(), 0, shape, nodes, visit);
    }

    /// This map of `old` shape, as one of `new` shape with the chunks
    /// `changes` sets (below `new.count`): the nodes on their paths, and
    /// across the old and new ends, rewritten as blobs numbered from `next`
    /// on; those past the new end dropped; the others kept.
    pub fn update(
        &self,
        old: Shape,
        new: Shape,
        changes: &BTreeMap<u64, Chunk>,
        nodes: &Nodes,
        next: u64,
    ) -> Update {
        let mut rebuild = Rebuild {
            held: self,
            old,
            new,
            changes,
            nodes,
            next,
            fresh: Vec::new(),
        };
        let top = new.height() - 1;
        let node = rebuild.node(top, 0);
        Update {
            map: Map {
                height: top + 1,
                node,
            },
            fresh: rebuild.fresh,
        }
    }
}

fn walk(node: &Node, k: u32, i: u64, shape: Shape, nodes: &Nodes, visit: &mut dyn FnMut(Visit)) {
    let first = i * shape.fanout;
    match node {
        Node::Leaf(chunks) => {
            for (slot, &chunk) in chunks.iter().enumerate() {
                if chunk != Chunk::default() {
                    visit(Visit::Chunk(first + slot as u64, chunk));
                }
            }
        }
        Node::Inner(children) => {
            for (slot, child) in children.iter().enumerate() {
                let Some(blob) = *child else { continue };
                visit(Visit::Node(blob));
                let child = first + slot as u64;
                let need = Need {
                    blob,
                    leaf: k == 1,
                    slots: shape.slots(k - 1, child),
                };
                walk(&nodes.node(need), k - 1, child, shape, nodes, visit);
            }
        }
    }
}

/// A map a flush made.
pub(crate) struct Update {
    pub map: Map,
    /// The nodes to put, with their blob numbers, in order.
    pub fresh: Vec<(u64, Arc<Node>)>,
}

/// [`Map::update`] at work.
struct Rebuild<'a> {
    held: &'a Map,
    old: Shape,
    new: Shape,
    changes: &'a BTreeMap<u64, Chunk>,
    nodes: &'a Nodes<'a>,
    next: u64,
    fresh: Vec<(u64, Arc<Node>)>,
}

impl Rebuild<'_> {
    /// The old map's node at level `k`, index `i`, if it has one.
    fn held_node(&self, k: u32, i: u64) -> Option<Arc<Node>> {
        if k >= self.held.top() {
            return (k == self.held.top() && i == 0).then(|| self.held.node.clone());
        }
        let blob = self.held_blob(k, i)?;
        Some(self.nodes.node(Need {
            blob,
            leaf: k == 0,
            slots: self.old.slots(k, i),
        }))
    }

    /// The blob of the old map's node at level `k`, index `i`, if one
    /// holds it (the top is in the root).
    fn held_blob(&self, k: u32, i: u64) -> Option<u64> {
        if k >= self.held.top() {
            return None;
        }
        let parent = self.held_node(k + 1, i / self.old.fanout)?;
        match &*parent {
            Node::Inner(children) => children.get((i % self.old.fanout) as usize).copied()?,
            Node::Leaf(_) => unreachable!("a leaf is at level 0"),
        }
    }

    /// Whether the node at level `k`, index `i` (below the top) changes:
    /// it holds a changed chunk; or it lies across either end, so that its
    /// entries change; or it is where the old top was, in the root, and a
    /// blob must hold it now.
    fn changes(&self, k: u32, i: u64) -> bool {
        let start = self.new.start(k, i);
        let end = start.saturating_add(self.new.span(k + 1));
        let across = |count: u64| start < count && count < end;
        self.changes.range(start..end).next().is_some()
            || (self.old.count != self.new.count
                && (across(self.old.count) || across(self.new.count)))
            || (k >= self.held.top() && start < self.old.count)
    }

    /// The node at level `k`, index `i`, as the new map has it.
    fn node(&mut self, k: u32, i: u64) -> Arc<Node> {
        let slots = self.new.slots(k, i);
        let held = self.held_node(k, i);
        let first = i * self.new.fanout;
        let node = if k == 0 {
            let held = match held.as_deref() {
                Some(Node::Leaf(chunks)) => &chunks[..],
                _ => &[],
            };
            Node::Leaf(
                (0..slots)
                    .map(|slot| match self.changes.get(&(first + slot)) {
                        Some(&chunk) => chunk,
                        None => held.get(slot as usize).copied().unwrap_or_default(),
                    })
                    .collect(),
            )
        } else {
            let held = match held.as_deref() {
                Some(Node::Inner(children)) => &children[..],
                _ => &[],
            };
            Node::Inner(
                (0..slots)
                    .map(|slot| match self.changes(k - 1, first + slot) {
                        true => self.child(k - 1, first + slot),
                        false => held.get(slot as usize).copied().flatten(),
                    })
                    .collect(),
            )
        };
        Arc::new(node)
    }

    /// The node at level `k`, index `i`, below the top, which changes: a
    /// new blob, or none if it holds only holes.
    fn child(&mut self, k: u32, i: u64) -> Option<u64> {
        let node = self.node(k, i);
        if node.is_holes() {
            return None;
        }
        let blob = self.next;
        self.next += 1;
        self.fresh.push((blob, node));
        Some(blob)
    }
}

#[cfg(test)]
#[path = "../tests/unit/map.rs"]
mod tests;
