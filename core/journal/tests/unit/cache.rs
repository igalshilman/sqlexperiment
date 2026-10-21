use super::*;
use crate::simulation::{fresh_namespace, shared_cache};

fn value(n: usize) -> Page {
    Page::from(vec![n as u8; n])
}

#[test]
fn namespaces_never_share_pages() {
    futures::executor::block_on(async {
        let (a, b) = (fresh_namespace(), fresh_namespace());
        a.insert(1, value(4));
        assert_eq!(a.get(1).await, Some(value(4)));
        assert_eq!(b.get(1).await, None, "same page and meta, other namespace");
        // A handle by the same key is the same namespace.
        shared_cache().namespace("by-key").insert(7, value(7));
        assert_eq!(
            shared_cache().namespace("by-key").get(7).await,
            Some(value(7))
        );
    })
}

#[test]
fn replaced_and_removed_pages_get_new_keys() {
    futures::executor::block_on(async {
        let cache = fresh_namespace();
        cache.insert(1, value(4));
        let old = cache.key(1);
        cache.replace(1, value(5));
        assert_ne!(cache.key(1), old);
        assert_eq!(cache.get(1).await, Some(value(5)));
        cache.remove(1);
        assert_eq!(cache.get(1).await, None);
        // Even if the old value were still served under its key, it is unreachable.
        cache.shared.pages.insert(old, value(4));
        assert_eq!(cache.get(1).await, None);
    })
}

/// The same commit record is known; another one in the same epoch keeps the
/// pages, and another epoch drops them.
#[test]
fn another_record_forgets_the_commit_and_another_epoch_the_pages() {
    futures::executor::block_on(async {
        let record = |n: u8| Page::from(vec![b'S', b'C', b'1', b'4', n]);
        let cache = fresh_namespace();
        assert!(!cache.validate(None, 0), "a fresh owner knows nothing");
        assert!(!cache.knows_pages());
        cache.record(None, 0, Some(0));
        assert!(cache.validate(None, 0), "an absent record, as recorded");
        assert!(!cache.validate(Some(&record(1)), 0), "not after another");
        cache.record(Some(record(1)), 0, Some(3));
        cache.insert(1, value(4));
        assert!(cache.validate(Some(&record(1)), 0));
        assert_eq!(cache.committed(), Some(3));

        // Another writer committed in the same epoch: the pages stay.
        let generation = cache.generation();
        assert!(!cache.validate(Some(&record(2)), 0));
        assert_eq!(cache.committed(), None);
        assert!(cache.knows_epoch(0));
        assert_eq!(cache.get(1).await, Some(value(4)));
        assert_eq!(cache.generation(), generation);

        // Or checkpointed: every page goes.
        cache.record(Some(record(2)), 0, Some(4));
        assert!(!cache.validate(Some(&record(3)), 1));
        assert!(!cache.knows_pages());
        assert_eq!(cache.get(1).await, None);
        assert_ne!(cache.generation(), generation);

        // A commit with an unknown outcome forgets the record, not the pages.
        cache.record(Some(record(3)), 1, Some(5));
        cache.insert(2, value(4));
        cache.forget();
        assert!(!cache.validate(Some(&record(3)), 1));
        assert_eq!(cache.get(2).await, Some(value(4)));
    })
}

/// A parked engine refers back to its namespace's handle; dropping the last
/// `PageCache` still frees everything.
#[test]
fn dropping_the_cache_frees_parked_engines() -> crate::Result<()> {
    use crate::{Session, StorageOptions, kv::KvStore, simulation::*};
    futures::executor::block_on(async {
        let cache = open_cache(cache_config(1 << 20, 16 << 20));
        let store: Arc<dyn KvStore> = Arc::new(MemoryKv::default());
        let options = StorageOptions {
            cache: Some(cache.namespace("parked")),
            ..StorageOptions::default()
        };
        drop(Session::open_with_options(store, options).await?);
        assert_eq!(cache.engines.lock().unwrap().parked.len(), 1);
        let shared = Arc::downgrade(&cache.shared);
        drop(cache);
        assert!(shared.upgrade().is_none());
        Ok(())
    })
}
