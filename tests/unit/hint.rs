use super::*;

#[test]
fn hints_round_trip_and_reject_malformed_input() {
    let pages = vec![0, 1, 2, 130, 70_000, u64::from(u32::MAX) * 3];
    let bytes = encode(&pages);
    assert_eq!(decode(&bytes, MAX_HINT_PAGES), Some(pages.clone()));
    assert_eq!(decode(&bytes, 3), None, "longer than the limit");
    assert_eq!(decode(&encode(&[]), 32), Some(vec![]));
    for bad in [
        &b"SH12"[..],
        &b"XX12\x01\x00\x00"[..],
        &bytes[..bytes.len() - 1],
        &[&bytes[..], &[0]].concat()[..],
        &b"SH12\x01\x00\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff\x01"[..],
    ] {
        assert_eq!(decode(bad, MAX_HINT_PAGES), None, "{bad:?}");
    }
    // 32 clustered pages fit in about 40 bytes.
    let clustered: Vec<u64> = (500..532).collect();
    assert!(encode(&clustered).len() <= 40);
}

#[test]
fn usage_ranks_the_header_then_interior_then_recent_leaves() {
    let mut usage = Usage::default();
    for page in 10..20 {
        usage.record(page, &[0x0d]); // table leaves
    }
    usage.record(7, &[0x05]); // interior table
    usage.record(3, &[0x02]); // interior index
    usage.record(0, b"S");
    usage.record(12, &[0x0d]); // read again: most recent leaf
    assert_eq!(usage.top(4), vec![0, 3, 7, 12]);
    assert_eq!(usage.top(5), vec![0, 3, 7, 12, 19]);
    assert_eq!(usage.top(1000).len(), 13);

    // Tracking is bounded; interior pages survive, old leaves go first.
    for page in 1000..1000 + 10 * MAX_HINT_PAGES as u64 {
        usage.record(page, &[0x0d]);
    }
    assert!(usage.pages.len() <= 2 * MAX_TRACKED);
    assert_eq!(usage.top(3), vec![0, 3, 7]);
}

/// Interior pages are bounded too, and reads stay cheap however many pages
/// look interior (overflow and freelist pages can).
#[test]
fn usage_stays_bounded_with_many_interior_pages() {
    let mut usage = Usage::default();
    let started = std::time::Instant::now();
    for page in 1..20_000 {
        usage.record(page, &[0x05]);
    }
    for page in 0..100_000 {
        usage.record(100_000 + page, &[0x0d]);
    }
    assert!(usage.pages.len() <= 2 * MAX_TRACKED);
    assert!(started.elapsed().as_secs() < 5, "{:?}", started.elapsed());
    assert!(
        usage.top(10).iter().all(|&page| page < 20_000),
        "interior first"
    );
}
