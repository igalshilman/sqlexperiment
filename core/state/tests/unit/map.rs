use super::*;
use crate::store::{Against, Reading};
use std::collections::{BTreeSet, HashMap};

/// A deterministic random source.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

/// The database the maps are of.
const DATABASE: u64 = 7;

/// Node blobs as a store holds them, and the next chunk blob's number.
#[derive(Default)]
struct Blobs {
    frames: HashMap<BlobId, Bytes>,
    next: u64,
}

impl Blobs {
    /// A name for a new chunk blob, never used before.
    fn chunk(&mut self) -> BlobId {
        self.next += 1;
        BlobId::of(DATABASE, Against::Nothing, &self.next.to_le_bytes())
    }
}

impl Blobs {
    /// The nodes `walk` needs, fetched a level at a time, as a cold state
    /// does: the rounds it took, and what it returned.
    fn walk<T>(&self, mut walk: impl FnMut(&Nodes) -> T) -> (usize, T) {
        let mut loaded: HashMap<BlobId, Arc<Node>> = HashMap::new();
        for round in 0.. {
            let get = |blob: BlobId| loaded.get(&blob).cloned();
            let nodes = Nodes::new(&get);
            let out = walk(&nodes);
            let missing = nodes.missing();
            if missing.is_empty() {
                return (round, out);
            }
            for need in missing {
                let node = Node::from_frame(&self.frames[&need.blob], need).unwrap();
                loaded.insert(need.blob, Arc::new(node));
            }
        }
        unreachable!()
    }
}

/// Every blob `map` names, each once.
fn named(map: &Map, shape: Shape, blobs: &Blobs) -> BTreeSet<BlobId> {
    let mut named = BTreeSet::new();
    let (_, nodes) = blobs.walk(|nodes| {
        let mut seen = Vec::new();
        map.walk(shape, nodes, &mut |visit| seen.push(visit));
        seen
    });
    for visit in nodes {
        match visit {
            Visit::Node(n) => assert!(named.insert(n), "blob {n} named twice"),
            Visit::Chunk(_, chunk) => {
                for n in chunk.blobs() {
                    assert!(named.insert(n), "blob {n} named twice");
                }
            }
        }
    }
    named
}

/// Random flushes of a file that grows, shrinks, and changes chunks here
/// and there, checked against the chunks as a plain row.
fn run(seed: u64, fanout: u64) {
    let mut rng = Rng(seed);
    let mut blobs = Blobs::default();
    let mut map = Map::empty();
    let mut row: Vec<Chunk> = Vec::new();
    let largest = fanout.pow(4).min(400);
    for step in 0..60 {
        let old = Shape {
            fanout,
            count: row.len() as u64,
        };
        let count = match rng.below(4) {
            0 => rng.below(largest + 1),
            _ => old.count,
        };
        let new = Shape { fanout, count };
        // The flush's chunk blobs, then its nodes, as a flush puts them.
        let mut changes = BTreeMap::new();
        for _ in 0..rng.below(8) {
            if count == 0 {
                break;
            }
            let chunk = match rng.below(3) {
                0 => count - 1,
                _ => rng.below(count),
            };
            let held = row.get(chunk as usize).copied().unwrap_or_default();
            let fresh = match rng.below(4) {
                0 => Chunk::default(),
                1 if held.base.is_some() => Chunk {
                    base: held.base,
                    delta: Some(blobs.chunk()),
                },
                _ => Chunk {
                    base: Some(blobs.chunk()),
                    delta: None,
                },
            };
            if fresh != held && !changes.contains_key(&chunk) {
                changes.insert(chunk, fresh);
            }
        }
        let (_, update) = blobs.walk(|nodes| map.update(old, new, &changes, nodes, DATABASE));
        // From the leaves up: a node is put after those it names.
        let mut put = BTreeSet::new();
        for (blob, frame, node) in &update.fresh {
            assert!(!node.is_holes(), "a node of holes is put");
            assert_eq!(*blob, BlobId::of(DATABASE, Against::Nothing, frame));
            assert_eq!(frame, &node.frame());
            if let Node::Inner(children) = &**node {
                for child in children.iter().flatten() {
                    assert!(
                        put.contains(child) || blobs.frames.contains_key(child),
                        "a node put before a node it names"
                    );
                }
            }
            let need = Need {
                blob: *blob,
                leaf: matches!(**node, Node::Leaf(_)),
                slots: match &**node {
                    Node::Leaf(chunks) => chunks.len() as u64,
                    Node::Inner(children) => children.len() as u64,
                },
            };
            assert_eq!(&Node::from_frame(frame, need).unwrap(), &**node);
            put.insert(*blob);
            blobs.frames.insert(*blob, frame.clone());
        }
        map = update.map;
        row.resize(count as usize, Chunk::default());
        for (&chunk, &fresh) in &changes {
            row[chunk as usize] = fresh;
        }
        // Read back cold: every chunk, a round trip a level below the top.
        let chunks: Vec<u64> = (0..count).collect();
        let (rounds, read) = blobs.walk(|nodes| {
            (chunks.iter())
                .map(|&c| map.get(new, c, nodes))
                .collect::<Vec<_>>()
        });
        assert_eq!(read, row, "seed {seed}, fanout {fanout}, step {step}");
        assert!(rounds < map.height as usize, "{rounds} rounds");
        assert_eq!(map.height, new.height());
        // It names its chunks' blobs, and nodes the flushes put.
        let named = named(&map, new, &blobs);
        let chunks: BTreeSet<BlobId> = row.iter().flat_map(|c| c.blobs()).collect();
        let nodes: BTreeSet<BlobId> = named.difference(&chunks).copied().collect();
        assert!(
            chunks.is_subset(&named),
            "seed {seed}, fanout {fanout}, step {step}"
        );
        assert!(nodes.iter().all(|node| blobs.frames.contains_key(node)));
        // The root's part round-trips.
        let mut bytes = Vec::new();
        map.encode(&mut bytes);
        let mut r = Reading::new(&bytes, "map");
        assert_eq!(Map::decode(&mut r, new).unwrap(), map);
        r.end().unwrap();
    }
}

#[test]
fn a_map_reads_back_what_flushes_wrote_and_names_its_chunks() {
    for seed in 0..200 {
        for fanout in [2, 3, 4, 16] {
            run(seed, fanout);
        }
    }
}

#[test]
fn heights() {
    let height = |fanout, count| Shape { fanout, count }.height();
    assert_eq!(height(64, 0), 1);
    assert_eq!(height(64, 64), 1);
    assert_eq!(height(64, 65), 2);
    assert_eq!(height(64, 4096), 2);
    assert_eq!(height(64, 4097), 3);
    // 10 GiB of 16 KiB chunks.
    assert_eq!(height(64, 655_360), 4);
    assert_eq!(height(256, 655_360), 3);
}

/// A node decodes only as the shape it has.
#[test]
fn a_node_decodes_only_as_its_shape() {
    let child = |n: u8| Some(BlobId([n; 16]));
    let node = Node::Inner(vec![child(3), None, child(9)]);
    let frame = node.frame();
    let need = |slots| Need {
        blob: BlobId([0; 16]),
        leaf: false,
        slots,
    };
    assert_eq!(Node::from_frame(&frame, need(3)).unwrap(), node);
    assert!(Node::from_frame(&frame, need(2)).is_err());
    assert!(Node::from_frame(&frame, need(4)).is_err());
    let leaf = Need {
        leaf: true,
        ..need(3)
    };
    assert!(Node::from_frame(&frame, leaf).is_err());
}

/// Equal nodes share a name; a node's name is its hash.
#[test]
fn equal_nodes_share_a_name() {
    let chunk = |n: u8| Chunk {
        base: Some(BlobId([n; 16])),
        delta: None,
    };
    let shape = Shape {
        fanout: 2,
        count: 4,
    };
    let none = |_: BlobId| None;
    let nodes = Nodes::new(&none);
    let changes = BTreeMap::from([(0, chunk(1)), (1, chunk(2)), (2, chunk(1)), (3, chunk(2))]);
    let update = Map::empty().update(Shape { count: 0, ..shape }, shape, &changes, &nodes, 7);
    let Node::Inner(children) = &*update.map.node else {
        panic!("two levels");
    };
    assert_eq!(children[0], children[1], "two equal leaves, one name");
    assert_eq!(update.fresh.len(), 2, "put twice, stored once");
}
