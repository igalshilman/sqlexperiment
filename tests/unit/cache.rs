use super::*;
use std::sync::Arc;

fn value(n: usize) -> Arc<[u8]> {
    vec![n as u8; n].into()
}

#[test]
fn evicts_least_recently_used_within_capacity() {
    let cache = ObjectCache::new(10);
    cache.insert((Kind::Block, 1), value(4));
    cache.insert((Kind::Node, 1), value(4));
    assert_eq!(cache.bytes(), 8);
    // Touch the block so the node becomes the eviction candidate.
    assert!(cache.get((Kind::Block, 1)).is_some());
    cache.insert((Kind::Block, 2), value(4));
    assert_eq!(cache.bytes(), 8);
    assert!(cache.get((Kind::Node, 1)).is_none());
    assert_eq!(cache.get((Kind::Block, 1)).as_deref(), Some(&*value(4)));
    assert!(cache.get((Kind::Block, 2)).is_some());
}

#[test]
fn replaces_removes_and_skips_oversized_values() {
    let cache = ObjectCache::new(10);
    cache.insert((Kind::Block, 1), value(4));
    cache.insert((Kind::Block, 1), value(6));
    assert_eq!(cache.bytes(), 6);
    cache.insert((Kind::Block, 2), value(11));
    assert!(cache.get((Kind::Block, 2)).is_none());
    assert_eq!(cache.bytes(), 6);
    cache.remove((Kind::Block, 1));
    cache.remove((Kind::Block, 1));
    assert_eq!(cache.bytes(), 0);
    assert!(cache.get((Kind::Block, 1)).is_none());
}
