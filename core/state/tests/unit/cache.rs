use super::*;
use crate::simulation::{fresh_namespace_cache, shared_cache};

fn value(n: usize) -> Bytes {
    Bytes::from(vec![n as u8; n])
}

#[test]
fn namespaces_never_share_blobs() {
    futures::executor::block_on(async {
        let (a, b) = (fresh_namespace_cache(), fresh_namespace_cache());
        a.insert(1, value(4));
        assert_eq!(a.get(1).await, Some(value(4)));
        assert_eq!(b.get(1).await, None, "same number, other namespace");
        // A handle by the same key is the same namespace.
        shared_cache().namespace("by-key").insert(7, value(7));
        assert_eq!(
            shared_cache().namespace("by-key").get(7).await,
            Some(value(7))
        );
    })
}

/// The same root is known; another one forgets this owner's flush, and
/// keeps the blobs, which never change.
#[test]
fn another_root_forgets_the_flush_not_the_blobs() {
    futures::executor::block_on(async {
        let root = |n: u8| Bytes::from(vec![b'S', b'R', b'1', b'6', n]);
        let cache = fresh_namespace_cache();
        assert!(!cache.validate(None), "a fresh owner knows nothing");
        cache.record(None, Some(0));
        assert!(cache.validate(None), "an absent root, as recorded");
        assert!(!cache.validate(Some(&root(1))), "not after another");
        cache.record(Some(root(1)), Some(3));
        cache.insert(1, value(4));
        assert!(cache.validate(Some(&root(1))));
        assert_eq!(cache.committed(), Some(3));

        // Another writer flushed: the blobs stay.
        assert!(!cache.validate(Some(&root(2))));
        assert_eq!(cache.committed(), None);
        assert_eq!(cache.get(1).await, Some(value(4)));

        // A flush with an unknown outcome forgets the root, and makes the
        // next open sync.
        cache.record(Some(root(3)), Some(5));
        cache.forget();
        assert!(!cache.validate(Some(&root(3))));
        assert_eq!(cache.unsettled(), Some(1));
        cache.settled(1);
        assert_eq!(cache.unsettled(), None);
    })
}

/// Idle state that refers back to its namespace's handle (as a parked SQL
/// engine does); dropping the last `PageCache` still frees everything.
#[test]
fn dropping_the_cache_frees_idle_state() -> crate::Result<()> {
    use crate::{kv::KvStore, simulation::*};
    futures::executor::block_on(async {
        let cache = open_cache(cache_config(1 << 20, 16 << 20));
        let store: Arc<dyn KvStore> = Arc::new(MemoryKv::default());
        let namespace = cache.namespace("parked");
        let state = crate::State::open(store).cached(&cache, "parked").await?;
        let freed = Arc::new(());
        let idle = (namespace, freed.clone());
        assert!(state.join("engine")?.park(idle, 1).is_ok());
        assert_eq!(cache.idle.lock().unwrap().parked.len(), 1);
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
        cache.insert(1, slice.clone());
        let cached = cache.get(1).await.expect("cached");
        assert_eq!(cached, slice);
        assert!(!inside(&cached));
        cache.record(Some(slice.clone()), None);
        assert!(!inside(cache.owner().root.as_ref().unwrap()));
    })
}
