use super::*;

fn root() -> Root {
    let mut root = Root::new(4, 64);
    root.version = 7;
    root.next_blob = 300;
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
                    chunk(Some(1), Some(299)),
                    chunk(None, None),
                    chunk(Some(200), None),
                ],
            ),
        ),
        (1, "vm", paged(Some(9), vec![chunk(Some(5), None); 3])),
        (
            2,
            "small",
            Kind::Record(Value::Inline(Bytes::from_static(b"hi"))),
        ),
        (3, "large", Kind::Record(Value::Blob(250))),
        (
            4,
            "turns",
            Kind::Log {
                first: 12,
                entries: vec![260, 261],
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
fn a_root_naming_a_blob_past_its_next_is_invalid() {
    let mut root = root();
    root.next_blob = 299;
    assert!(Root::decode(&root.encode()).is_err());
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

#[test]
fn a_sweep_and_a_claim_decode_as_encoded() {
    let sweep = Sweep {
        database: 7,
        marked: 40,
        dead: BTreeSet::from([0, 3, 900, 1 << 40]),
    };
    assert_eq!(Sweep::decode(&sweep.encode()).unwrap(), sweep);
    let empty = Sweep::default();
    assert_eq!(Sweep::decode(&empty.encode()).unwrap(), empty);
    let bytes = sweep.encode();
    for len in 0..bytes.len() {
        assert!(Sweep::decode(&bytes[..len]).is_err(), "truncated to {len}");
    }
    assert_eq!(decode_claim(&encode_claim(4096)).unwrap(), 4096);
    assert!(decode_claim(&encode_claim(4096)[..4]).is_err());
}
