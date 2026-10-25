use super::*;

fn checkpoint(version: u64) -> Checkpoint {
    Checkpoint {
        pack: PackId {
            version,
            nonce: u64::MAX - version,
        },
        next_blob: 900 + version,
        manifest: (1 << 20, 77),
    }
}

#[test]
fn a_head_decodes_as_encoded_and_not_truncated() {
    let head = Head {
        version: 9,
        root: Bytes::from_static(b"a root"),
        checkpoint: Some(checkpoint(6)),
        previous: Some(checkpoint(2)),
        tail_first: 3,
        parts: vec![1, 0, 2, 1, 1, 0, 1],
        tail_bytes: 4096,
    };
    let bytes = head.encode();
    assert_eq!(Head::decode(&bytes).unwrap(), head);
    for len in 0..bytes.len() {
        assert!(
            Head::decode(&bytes.slice(..len)).is_err(),
            "truncated to {len}"
        );
    }
    // A checkpoint past the head's version, or a tail of the wrong extent.
    let ahead = Head {
        checkpoint: Some(checkpoint(10)),
        ..head.clone()
    };
    assert!(Head::decode(&ahead.encode()).is_err());
    let short = Head {
        parts: vec![1],
        ..head
    };
    assert!(Head::decode(&short.encode()).is_err());
}

#[test]
fn a_manifest_decodes_as_encoded_and_not_truncated() {
    let packs = [checkpoint(3).pack, checkpoint(8).pack];
    let mut manifest = Manifest::default();
    for (i, n) in [0u64, 1, 5, 6, 400, 1 << 40].into_iter().enumerate() {
        let pack = packs[i % 2];
        let (offset, len) = (i as u64 * 100, 90 + i as u64);
        manifest.blobs.insert(n, Location { pack, offset, len });
    }
    manifest.sizes = BTreeMap::from([(packs[0], 1 << 30), (packs[1], 600)]);
    let bytes = manifest.encode();
    assert_eq!(Manifest::decode(&bytes).unwrap(), manifest);
    assert_eq!(manifest.packs(), BTreeSet::from(packs));
    for len in 0..bytes.len() {
        assert!(
            Manifest::decode(&bytes[..len]).is_err(),
            "truncated to {len}"
        );
    }
    let empty = Manifest::default();
    assert_eq!(Manifest::decode(&empty.encode()).unwrap(), empty);
}

#[test]
fn a_tail_entry_splits_into_parts_and_reads_back() {
    let blobs: Vec<(u64, Bytes)> = (0..40)
        .map(|n| (100 + n, Bytes::from(vec![n as u8; 100_000])))
        .chain([(1000, Bytes::from(vec![7; 3 << 20]))])
        .collect();
    let parts = encode_parts(&blobs);
    assert!(parts.len() > 2, "{} parts", parts.len());
    for part in &parts[..parts.len() - 1] {
        assert!(part.len() <= PART_BYTES + 16);
    }
    let mut read = HashMap::new();
    for part in &parts {
        read_part(part, &mut read).unwrap();
    }
    assert_eq!(read, blobs.into_iter().collect::<HashMap<_, _>>());
    assert!(encode_parts(&[]).is_empty());
}

#[test]
fn keys_parse_as_made() {
    let pack = checkpoint(12).pack;
    assert_eq!(PackId::parse(&pack.key()), Some(pack));
    assert_eq!(PackId::parse("checkpoint/zz-1"), None);
    assert_eq!(parse_tail_key(&tail_key(7, 2)), Some((7, 2)));
    assert_eq!(parse_tail_key("v18/head"), None);
}
