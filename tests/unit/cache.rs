use super::*;

fn value(n: usize) -> Page {
    Page::from(vec![n as u8; n])
}

#[test]
fn evicts_least_recently_used_within_capacity() {
    let cache = PageCache::new(10);
    cache.insert(1, value(4));
    cache.insert(2, value(4));
    assert_eq!(cache.bytes(), 8);
    // Touch page 1 so page 2 becomes the eviction candidate.
    assert!(cache.get(1).is_some());
    cache.insert(3, value(4));
    assert_eq!(cache.bytes(), 8);
    assert!(cache.get(2).is_none());
    assert_eq!(cache.get(1).as_deref(), Some(&*value(4)));
    assert!(cache.get(3).is_some());
}

#[test]
fn replaces_removes_and_skips_oversized_values() {
    let cache = PageCache::new(10);
    cache.insert(1, value(4));
    cache.insert(1, value(6));
    assert_eq!(cache.bytes(), 6);
    cache.insert(1, value(11));
    assert!(
        cache.get(1).is_none(),
        "an oversized value replaces nothing stale"
    );
    assert_eq!(cache.bytes(), 0);
    cache.insert(2, value(3));
    cache.remove(2);
    cache.remove(2);
    assert_eq!(cache.bytes(), 0);
}

#[test]
fn a_different_meta_forgets_claims_and_keeps_pages_to_refresh() {
    let meta = Meta {
        log_start: 5,
        deleted_below: 2,
        size: 4096,
    };
    let claim = Meta {
        log_start: 7,
        ..meta
    };
    let cache = PageCache::new(100);
    assert_eq!(
        cache.validate(None, 1),
        Reuse::Known(None, None),
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
    assert_eq!(
        cache.validate(Some(&claim.encode()), 7),
        Reuse::Changed(Some(5))
    );
    assert_eq!(cache.expected_start(), Some(5), "the claim is forgotten");
    cache.refresh(Some(&Images::from([(1, None), (3, Some(value(2)))])));
    assert!(cache.get(1).is_none(), "a hole");
    assert!(cache.get(3).is_none(), "uncached pages stay uncached");
    assert_eq!(cache.bytes(), 4);
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
    assert_eq!((cache.bytes(), cache.expected_start()), (0, None));
    cache.insert(1, value(4));
    cache.record(Some(claim.encode().into()), None, 9);
    assert_eq!(
        cache.validate(Some(&meta.encode()), 5),
        Reuse::Changed(None)
    );
    assert_eq!(cache.bytes(), 0);
}
