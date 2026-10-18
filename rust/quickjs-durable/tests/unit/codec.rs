use crate::{codec::*, model::PAGE};

fn pages(bytes: &[u8]) -> Vec<u8> {
    bytes.iter().flat_map(|&byte| [byte; PAGE]).collect()
}

#[test]
fn a_batch_decodes_against_its_reference() {
    let (new, old) = (pages(&[17, 23]), pages(&[17, 0]));
    let blob = encode_pages(&[0, 16383], &new, &old).unwrap();
    let (indexes, frame) = page_indexes(&blob).unwrap();
    assert_eq!(indexes, [0, 16383]);
    assert_eq!(decode_pages(frame, &old).unwrap(), new);
    // An unchanged page costs a match against the reference.
    assert!(blob.len() < 64, "{} bytes", blob.len());
}

#[test]
fn malformed_batches_are_rejected() {
    let page = pages(&[1]);
    assert!(encode_pages(&[1, 1], &pages(&[1, 1]), &pages(&[1, 1])).is_err());
    assert!(encode_pages(&[1], &page[1..], &page[1..]).is_err());
    assert!(encode_pages(&[1], &page, &pages(&[1, 1])).is_err());
    let blob = encode_pages(&[1, 2], &pages(&[1, 2]), &pages(&[3, 4])).unwrap();
    assert!(page_indexes(&blob[..5]).is_err());
    assert!(page_indexes(b"QPD1\x01\x00").is_err());
    let (_, frame) = page_indexes(&blob).unwrap();
    assert!(decode_pages(&frame[..frame.len() / 2], &pages(&[3, 4])).is_err());
    assert!(decode_pages(frame, &pages(&[3])).is_err());
}
