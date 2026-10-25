use super::*;

fn b(n: u8) -> BlobId {
    BlobId([n; 16])
}

fn root() -> Root {
    let mut root = Root::new(4, 64);
    root.version = 7;
    root.next_file = 5;
    let paged = |base, chunks| Kind::Paged {
        size: 3 * 4 * PAGE,
        base,
        map: Map {
            height: 1,
            node: Arc::new(Node::Leaf(chunks)),
        },
    };
    let chunk = |base, delta| Chunk { base, delta };
    let entries = [
        (
            0,
            "db",
            paged(
                None,
                vec![
                    chunk(Some(b(1)), Some(b(199))),
                    chunk(None, None),
                    chunk(Some(b(200)), None),
                ],
            ),
        ),
        (1, "vm", paged(Some(9), vec![chunk(Some(b(5)), None); 3])),
        (
            2,
            "small",
            Kind::Record(Value::Inline(Bytes::from_static(b"hi"))),
        ),
        (3, "large", Kind::Record(Value::Blob(b(250)))),
        (
            4,
            "turns",
            Kind::Log {
                first: 12,
                entries: vec![b(60), b(61)],
            },
        ),
    ];
    root.directory = (entries.into_iter())
        .map(|(id, name, kind)| {
            (
                id,
                Entry {
                    name: name.into(),
                    kind,
                },
            )
        })
        .collect();
    root
}

#[test]
fn a_root_decodes_as_encoded() {
    let root = root();
    assert_eq!(Root::decode(&root.encode()).unwrap(), root);
    let empty = Root::new(1, 2);
    assert_eq!(Root::decode(&empty.encode()).unwrap(), empty);
}

#[test]
fn a_sweep_decodes_as_encoded() {
    let sweep = Sweep {
        marked: 9,
        candidates: BTreeSet::from([b(1), b(2), b(255)]),
    };
    assert_eq!(Sweep::decode(&sweep.encode()).unwrap(), sweep);
    let bytes = sweep.encode();
    for len in 0..bytes.len() {
        assert!(Sweep::decode(&bytes[..len]).is_err(), "truncated to {len}");
    }
}

#[test]
fn a_blob_key_parses_back() {
    let blob = BlobId::of(7, Against::Nothing, b"frame");
    assert_eq!(parse_blob_key(&blob_key(blob)), Some(blob));
    assert_eq!(
        blob_key(b(0xab)),
        format!("{BLOB_PREFIX}{}", "ab".repeat(16))
    );
    for key in ["v17/blob/1", "v17/root", &blob_key(blob).to_uppercase()] {
        assert_eq!(parse_blob_key(key), None, "{key}");
    }
}

/// A frame's name hashes what it is compressed against: the same frame
/// against two references holds different pages.
#[test]
fn a_name_hashes_the_database_the_reference_and_the_frame() {
    let names = [
        BlobId::of(7, Against::Nothing, b"frame"),
        BlobId::of(8, Against::Nothing, b"frame"),
        BlobId::of(7, Against::Nothing, b"other"),
        BlobId::of(7, Against::Blob(b(1)), b"frame"),
        BlobId::of(7, Against::Blob(b(2)), b"frame"),
        BlobId::of(7, Against::Base { base: 1, chunk: 0 }, b"frame"),
        BlobId::of(7, Against::Base { base: 1, chunk: 1 }, b"frame"),
        BlobId::of(7, Against::Base { base: 2, chunk: 0 }, b"frame"),
    ];
    assert_eq!(BTreeSet::from(names).len(), names.len());
    assert_eq!(names[0], BlobId::of(7, Against::Nothing, b"frame"));
}

/// Truncated, extended, or any bytes at all: an error, never a panic.
#[test]
fn malformed_roots_fail_to_decode() {
    let bytes = root().encode();
    for len in 0..bytes.len() {
        assert!(Root::decode(&bytes[..len]).is_err(), "truncated to {len}");
    }
    let mut longer = bytes.to_vec();
    longer.push(0);
    assert!(Root::decode(&longer).is_err(), "a byte past the frame");
    let mut seed = 1u64;
    for _ in 0..2000 {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let len = (seed >> 56) as usize;
        let noise: Vec<u8> = (0..len)
            .map(|i| (seed >> (i % 8 * 8)) as u8 ^ i as u8)
            .collect();
        let _ = Root::decode(&[&ROOT_MAGIC[..], &noise].concat());
    }
}

/// A body that is valid zstd, but not a root: refused, without a panic.
#[test]
fn a_frame_of_noise_is_not_a_root() {
    for body in [&b""[..], &[0; 7], &[0xff; 64], &[0; 64]] {
        let mut bytes = ROOT_MAGIC.to_vec();
        zstd::compress(body, &[], |frame| bytes.extend_from_slice(frame)).unwrap();
        assert!(Root::decode(&bytes).is_err(), "{body:?}");
    }
}
