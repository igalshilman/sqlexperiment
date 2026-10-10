use super::*;

#[test]
fn embedded_root_head_rejects_invalid_shape_and_references() {
    let empty = Head::default().encode();
    assert_eq!(empty.len(), 21);
    assert!(Head::decode(&empty).unwrap().root.is_empty());
    let head = Head {
        root: Node::from([(0, 1), (255, 2)]),
        height: 1,
        last_id: 2,
        size: 256 * 4096,
    };
    let valid = head.encode();
    let decoded = Head::decode(&valid).unwrap();
    assert_eq!(decoded.root, head.root);
    assert_eq!(decoded.size, head.size);
    assert_eq!(decoded.height, head.height);
    assert_eq!(decoded.last_id, head.last_id);
    let mut invalid: Vec<_> = (0..valid.len()).map(|n| valid[..n].to_vec()).collect();
    for height in [0, 9, 255] {
        let mut bytes = valid.clone();
        bytes[20] = height;
        invalid.push(bytes);
    }
    let mut bytes = empty;
    bytes[20] = 1;
    invalid.push(bytes);
    let mut bytes = valid.clone();
    bytes[12..20].copy_from_slice(&1u64.to_le_bytes());
    invalid.push(bytes);
    let mut bytes = valid.clone();
    bytes[0] ^= 1;
    invalid.push(bytes);
    let mut bytes = valid;
    bytes.push(0);
    invalid.push(bytes);
    invalid.push(vec![0; 4096]);
    for bytes in invalid {
        assert!(
            Head::decode(&bytes).is_err(),
            "accepted malformed head: {bytes:?}"
        );
    }
}
