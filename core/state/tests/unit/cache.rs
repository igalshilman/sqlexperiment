use super::*;
use crate::{
    format::{Head, PackId},
    simulation::{fresh_namespace_cache, shared_cache},
};

fn value(n: usize) -> Bytes {
    Bytes::from(vec![n as u8; n])
}

/// The range at `offset` of the pack of version 1 of the database `id`.
fn at(id: u64, offset: u64) -> Location {
    let pack = PackId {
        version: 1,
        nonce: id,
    };
    Location {
        pack,
        offset,
        len: 4,
    }
}

fn durable() -> Arc<Durable> {
    Arc::new(Durable::empty(Head::new(1, 16)))
}

#[test]
fn namespaces_never_share_ranges() {
    futures::executor::block_on(async {
        let (a, b) = (fresh_namespace_cache(), fresh_namespace_cache());
        a.insert(at(0, 1), value(4));
        assert_eq!(a.get(at(0, 1)).await, Some(value(4)));
        assert_eq!(b.get(at(0, 1)).await, None, "same number, other namespace");
        assert_eq!(a.get(at(9, 1)).await, None, "same offset, other pack");
        // A handle by the same key is the same namespace.
        shared_cache()
            .namespace("by-key")
            .insert(at(0, 7), value(7));
        assert_eq!(
            shared_cache().namespace("by-key").get(at(0, 7)).await,
            Some(value(7))
        );
    })
}

/// The same root is known; another one forgets this owner's flush, and
/// keeps the ranges, which never change.
#[test]
fn another_head_forgets_the_flush_not_the_ranges() {
    futures::executor::block_on(async {
        let root = |n: u8| Bytes::from(vec![b'S', b'R', b'1', b'6', n]);
        let cache = fresh_namespace_cache();
        assert!(!cache.validate(None), "a fresh owner knows nothing");
        cache.record(None, Some(0), durable());
        assert!(cache.validate(None), "an absent root, as recorded");
        assert!(!cache.validate(Some(&root(1))), "not after another");
        cache.record(Some(root(1)), Some(3), durable());
        cache.insert(at(0, 1), value(4));
        assert!(cache.validate(Some(&root(1))));
        assert_eq!(cache.committed(), Some(3));

        // Another writer flushed: the ranges stay.
        assert!(!cache.validate(Some(&root(2))));
        assert_eq!(cache.committed(), None);
        assert_eq!(cache.get(at(0, 1)).await, Some(value(4)));

        // A flush with an unknown outcome forgets the root, and makes the
        // next open sync.
        cache.record(Some(root(3)), Some(5), durable());
        cache.forget();
        assert!(!cache.validate(Some(&root(3))));
        assert_eq!(cache.unsettled(), Some(1));
        cache.settled(1);
        assert_eq!(cache.unsettled(), None);

        // A flush runs: an open syncs, and does not let another flush build
        // on the root it reads, until the flush ends.
        cache.record(Some(root(4)), Some(6), durable());
        assert!(cache.claim(6));
        assert_eq!(cache.unsettled(), Some(1));
        cache.record(Some(root(5)), Some(7), durable());
        assert!(!cache.claim(7));
        cache.flushed(root(5), 7, durable());
        assert_eq!(cache.unsettled(), None);
        assert!(cache.claim(7));
    })
}

/// Idle state that refers back to its namespace's handle (as a parked SQL
/// engine does); dropping the last `PageCache` still frees everything.
#[test]
fn dropping_the_cache_frees_parked_values() -> crate::Result<()> {
    use crate::{kv::KvStore, simulation::*};
    futures::executor::block_on(async {
        let cache = open_cache(cache_config(1 << 20, 16 << 20));
        let store: Arc<dyn KvStore> = Arc::new(MemoryKv::default());
        let namespace = cache.namespace("parked");
        let state = crate::State::open(store).cached(&cache, "parked").await?;
        let freed = Arc::new(());
        let value = (namespace, freed.clone());
        assert!(state.park("engine", value, 1).is_ok());
        assert_eq!(cache.parked.lock().unwrap().parked.len(), 1);
        drop(state);
        let shared = Arc::downgrade(&cache.shared);
        drop(cache);
        assert!(shared.upgrade().is_none());
        assert_eq!(Arc::strong_count(&freed), 1);
        Ok(())
    })
}

#[test]
fn values_read_as_slices_of_a_larger_buffer_are_kept_alone() {
    futures::executor::block_on(async {
        let cache = fresh_namespace_cache();
        // A receive buffer holding much more than the one value.
        let buffer = Bytes::from(vec![9; 1 << 20]);
        let inside = |value: &Bytes| buffer.as_ptr_range().contains(&value.as_ptr());
        let slice = buffer.slice(4096..8192);
        cache.insert(at(0, 1), slice.clone());
        let cached = cache.get(at(0, 1)).await.expect("cached");
        assert_eq!(cached, slice);
        assert!(!inside(&cached));
        cache.record(Some(slice.clone()), None, durable());
        assert!(!inside(cache.owner().head.as_ref().unwrap()));
    })
}

/// A namespace evicted with a flush whose outcome is unknown would forget
/// that its next open must sync: it stays until a sync settles it.
#[test]
fn an_unsettled_namespace_is_never_evicted() {
    let cache = crate::simulation::open_cache(PageCacheConfig {
        namespaces: 4,
        ..crate::simulation::cache_config(1 << 20, 4 << 20)
    });
    cache.namespace("failed").forget();
    let others = |cache: &PageCache| (0..64).for_each(|n| drop(cache.namespace(&n.to_string())));
    others(&cache);
    let failed = cache.namespace("failed");
    assert_eq!(failed.unsettled(), Some(1));
    failed.settled(1);
    drop(failed);
    others(&cache);
    assert_eq!(cache.namespace("failed").unsettled(), None);
}
