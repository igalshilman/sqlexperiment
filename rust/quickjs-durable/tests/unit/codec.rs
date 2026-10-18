use crate::{codec::*, model::PAGE};

#[test]
fn bounded_decode_and_malformed_batches() {
    let data = vec![0; 128];
    let blob = compress(&data).unwrap();
    assert_eq!(decompress(&blob, 128).unwrap(), data);
    assert!(decompress(&blob, 127).is_err());
    assert!(decode_pages(&blob).is_err());
    assert!(encode_pages(&[(1, vec![0; PAGE]), (1, vec![0; PAGE])]).is_err());
    assert!(encode_pages(&[(1, vec![0; PAGE - 1])]).is_err());
    let pages = vec![(0, vec![17; PAGE]), (16383, vec![23; PAGE])];
    let blob = encode_pages(&pages).unwrap();
    assert_eq!(decode_pages(&blob).unwrap(), pages);
    assert!(decode_pages(&blob[..blob.len() / 2]).is_err());
    let mut raw = decompress(&blob, 2 * (PAGE + 4) + 8).unwrap();
    raw.extend_from_slice(&[0]);
    assert!(decode_pages(&compress(&raw).unwrap()).is_err());
}
