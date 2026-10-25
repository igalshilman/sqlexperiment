use super::*;
use crate::store::Reading;
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

/// Node blobs as a store holds them, and the next blob number.
#[derive(Default)]
struct Blobs {
    frames: HashMap<u64, Bytes>,
    next: u64,
}

impl Blobs {
    /// The nodes `walk` needs, fetched a level at a time, as a cold state
    /// does: the rounds it took, and what it returned.
    fn walk<T>(&self, mut walk: impl FnMut(&Nodes) -> T) -> (usize, T) {
        let mut loaded: HashMap<u64, Arc<Node>> = HashMap::new();
        for round in 0.. {
            let get = |n: u64| loaded.get(&n).cloned();
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

/// Every blob `map` names, and checks it holds no node of holes.
fn named(map: &Map, shape: Shape, blobs: &Blobs) -> BTreeSet<u64> {
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
                    delta: Some(blobs.next),
                },
                _ => Chunk {
                    base: Some(blobs.next),
                    delta: None,
                },
            };
            blobs.next += u64::from(fresh.blobs().any(|n| n == blobs.next));
            if fresh != held && !changes.contains_key(&chunk) {
                changes.insert(chunk, fresh);
            }
        }
        let next = blobs.next;
        let (_, update) = blobs.walk(|nodes| map.update(old, new, &changes, nodes, next));
        for (n, node) in &update.fresh {
            assert_eq!(*n, blobs.next, "numbered in order");
            blobs.next += 1;
            assert!(!node.is_holes(), "a node of holes is put");
            let need = Need {
                blob: *n,
                leaf: matches!(**node, Node::Leaf(_)),
                slots: match &**node {
                    Node::Leaf(chunks) => chunks.len() as u64,
                    Node::Inner(children) => children.len() as u64,
                },
            };
            let frame = node.frame().unwrap();
            assert_eq!(&Node::from_frame(&frame, need).unwrap(), &**node);
            blobs.frames.insert(*n, frame);
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
        // It names its chunks' blobs, and nodes the flushes put: nothing
        // else.
        let named = named(&map, new, &blobs);
        let chunks: BTreeSet<u64> = row.iter().flat_map(|c| c.blobs()).collect();
        let nodes: BTreeSet<u64> = named.difference(&chunks).copied().collect();
        assert!(
            chunks.is_subset(&named),
            "seed {seed}, fanout {fanout}, step {step}"
        );
        assert!(nodes.iter().all(|node| blobs.frames.contains_key(node)));
        // The root's part round-trips.
        let mut bytes = Vec::new();
        map.encode(&mut bytes);
        let mut r = Reading::new(&bytes, "map");
        assert_eq!(Map::decode(&mut r, new, blobs.next).unwrap(), map);
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

/// A node that names a blob at or past its own is refused: no map loops.
#[test]
fn a_node_names_only_older_blobs() {
    let node = Node::Inner(vec![Some(3), None, Some(9)]);
    let frame = node.frame().unwrap();
    let need = |blob| Need {
        blob,
        leaf: false,
        slots: 3,
    };
    assert_eq!(Node::from_frame(&frame, need(10)).unwrap(), node);
    assert!(Node::from_frame(&frame, need(9)).is_err());
    assert!(
        Node::from_frame(
            &frame,
            Need {
                slots: 2,
                ..need(10)
            }
        )
        .is_err()
    );
}
