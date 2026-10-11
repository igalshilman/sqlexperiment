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
fn a_different_meta_resets_pages_and_claims() {
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
        cache.validate(None),
        Some((None, None)),
        "absent meta at start"
    );
    cache.insert(1, value(4));
    cache.record(Some(meta.encode().into()), Some(claim), 9);
    assert_eq!(cache.expected_start(), Some(7));
    assert_eq!(
        cache.validate(Some(&meta.encode())),
        Some((Some(claim), Some(9)))
    );
    assert_eq!(cache.bytes(), 4);

    assert_eq!(cache.validate(Some(&claim.encode())), None);
    assert_eq!(cache.bytes(), 0);
    assert_eq!(
        cache.validate(Some(&claim.encode())),
        None,
        "until recorded"
    );
    cache.record(Some(claim.encode().into()), None, 9);
    cache.forget();
    assert_eq!(cache.validate(Some(&claim.encode())), None);
}
