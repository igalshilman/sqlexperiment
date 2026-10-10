use super::*;

#[test]
fn node_shapes_and_full_id_range_round_trip() {
    for count in [0, 1, 2, 31, 32, 33, 128, 255, 256] {
        for base in [1, 127, 128, 65535, u64::MAX - 255] {
            let node: Node = (0..count).map(|i| (i as u8, base + i as u64)).collect();
            let bytes = encode(&node);
            assert_eq!(decode(&bytes, u64::MAX, true).unwrap(), node);
            assert!(bytes.len() <= 1 + count * 9);
        }
    }
    let dense: Node = (0..=255).map(|edge| (edge, u64::from(edge) + 1)).collect();
    assert_eq!(encode(&dense).len(), 420);
    assert_eq!(encode(&dense)[0], 1);
    let scattered: Node = (0..33)
        .map(|edge| (edge, (u64::MAX / 32 * u64::from(edge)).max(1)))
        .collect();
    assert_eq!(encode(&scattered)[0], 0);
    assert_eq!(
        decode(&encode(&scattered), u64::MAX, false).unwrap(),
        scattered
    );
}

#[test]
fn rejects_truncation_trailing_data_bad_edges_and_overflow() {
    let node: Node = (0..=255).map(|edge| (edge, u64::from(edge) + 1)).collect();
    let encoded = encode(&node);
    for len in 0..encoded.len() {
        assert!(decode(&encoded[..len], 256, false).is_err());
    }
    let mut trailing = encoded.clone();
    trailing.push(0);
    assert!(decode(&trailing, 256, false).is_err());
    let mut wrong_bitmap = encoded.clone();
    wrong_bitmap[3] = 0;
    assert!(decode(&wrong_bitmap, 256, false).is_err());
    assert!(decode(&encoded, 255, false).is_err());
    for bad in [
        vec![2],
        vec![0],
        vec![1, 0, 0],
        vec![1, 1, 1],
        vec![1, 2, 0, 5, 5, 1, 0, 1],
        vec![1, 1, 0, 0, 0, 0],
        vec![1, 1, 0, 0, 0x81, 0, 0],
    ] {
        assert!(decode(&bad, u64::MAX, false).is_err());
    }
    let mut overflow = vec![1, 2, 0, 0, 1];
    write_varint(u64::MAX, &mut overflow);
    overflow.extend([0, 1]);
    assert!(decode(&overflow, u64::MAX, false).is_err());
    for bytes in [vec![0x80; 10], vec![0xff; 10], vec![0x80, 0], vec![0x80]] {
        assert!(read_varint(&mut bytes.as_slice()).is_err());
    }
}

#[test]
fn deterministic_random_nodes_and_arbitrary_inputs() {
    let mut rng = 7u64;
    let mut next = || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
    };
    for _ in 0..2000 {
        let mut node = Node::new();
        for _ in 0..next() % 257 {
            node.insert(next() as u8, next().max(1));
        }
        assert_eq!(decode(&encode(&node), u64::MAX, true).unwrap(), node);
        let bytes: Vec<u8> = (0..next() % 2400).map(|_| next() as u8).collect();
        if let Ok(decoded) = decode(&bytes, u64::MAX, false) {
            assert!(!decoded.is_empty());
            assert_eq!(decode(&encode(&decoded), u64::MAX, false).unwrap(), decoded);
        }
    }
}
