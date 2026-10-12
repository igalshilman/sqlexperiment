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

#[test]
fn a_different_meta_forgets_claims_and_keeps_pages_to_refresh() {
    futures::executor::block_on(async {
        let meta = Meta {
            log_start: 5,
            deleted_below: 2,
            size: 4096,
        };
        let claim = Meta {
            log_start: 7,
            ..meta
        };
        let cache = fresh_namespace();
        assert_eq!(cache.expected_start(), None, "a fresh owner knows nothing");
        assert_eq!(
            cache.validate(Some(&meta.encode()), 5),
            Reuse::Changed(None)
        );
        cache.record(None, None, 1);
        assert_eq!(
            cache.validate(None, 1),
            Reuse::Known(None, Some(1)),
            "absent meta"
        );
        cache.insert(1, value(4));
        cache.insert(2, value(4));
        cache.record(Some(meta.encode().into()), Some(claim), 9);
        assert_eq!(cache.expected_start(), Some(7));
        assert_eq!(
            cache.validate(Some(&meta.encode()), 5),
            Reuse::Known(Some(claim), Some(9))
        );

        // Another writer moved meta: refresh what it changed from the base.
        let generation = cache.generation();
        assert_eq!(
            cache.validate(Some(&claim.encode()), 7),
            Reuse::Changed(Some(5))
        );
        assert_eq!(cache.expected_start(), Some(5), "the claim is forgotten");
        cache.refresh(Some(&Images::from([(1, None), (3, Some(value(2)))])));
        assert_eq!(cache.get(1).await, None, "a hole");
        assert_eq!(cache.get(3).await, None, "uncached pages stay uncached");
        assert_eq!(cache.get(2).await, Some(value(4)), "unchanged pages stay");
        assert_eq!(cache.generation(), generation);
        assert_eq!(
            cache.validate(Some(&claim.encode()), 7),
            Reuse::Changed(Some(5)),
            "until recorded"
        );
        cache.record(Some(claim.encode().into()), None, 9);
        cache.forget();
        assert_eq!(
            cache.validate(Some(&claim.encode()), 7),
            Reuse::Changed(Some(7))
        );

        // Unreadable changes, or a log start before the base, drop every page.
        cache.refresh(None);
        assert_eq!(cache.expected_start(), None);
        assert_eq!(cache.get(2).await, None);
        cache.insert(1, value(4));
        cache.record(Some(claim.encode().into()), None, 9);
        assert_eq!(
            cache.validate(Some(&meta.encode()), 5),
            Reuse::Changed(None)
        );
        assert_eq!(cache.get(1).await, None);
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
