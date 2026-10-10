use super::*;
use crate::io::BLOCK_SIZE;

fn page(n: u8) -> Vec<u8> {
    vec![n; BLOCK_SIZE]
}

#[test]
fn segments_round_trip_pages_holes_and_size() -> io::Result<()> {
    let pages = Pages::from([(0, Some(page(1))), (3, None), (900, Some(page(2)))]);
    for compression in [PageCompression::Raw, PageCompression::Lz4] {
        let bytes = encode(5, 901 * 4096, &pages, compression)?;
        assert_eq!(decode(&bytes, 5)?, (901 * 4096, pages.clone()));
    }
    Ok(())
}

#[test]
fn malformed_segments_are_rejected() -> io::Result<()> {
    let pages = Pages::from([(1, Some(page(1))), (2, None)]);
    let valid = encode(9, 3 * 4096, &pages, PageCompression::Raw)?;
    assert!(decode(&valid, 8).is_err(), "sequence must match its key");
    let mut invalid: Vec<_> = (0..valid.len()).map(|n| valid[..n].to_vec()).collect();
    let mut bytes = valid.clone();
    bytes.push(0);
    invalid.push(bytes);
    let mut bytes = valid.clone();
    bytes[0] ^= 1;
    invalid.push(bytes);
    let mut bytes = valid.clone();
    bytes[PREFIX..PREFIX + 8].copy_from_slice(&5u64.to_le_bytes());
    invalid.push(bytes); // pages out of order
    let mut bytes = valid.clone();
    bytes[PREFIX + 10] = 9;
    invalid.push(bytes); // invalid page codec tag
    for bytes in invalid {
        assert!(decode(&bytes, 9).is_err(), "accepted {} bytes", bytes.len());
    }
    Ok(())
}
