use super::*;

fn random(seed: &mut u64) -> u8 {
    *seed ^= *seed << 13;
    *seed ^= *seed >> 7;
    *seed ^= *seed << 17;
    (*seed >> 32) as u8
}

#[test]
fn roundtrip_raw_compressed_and_incompressible_pages() -> io::Result<()> {
    let mut seed = 0x8a71_c25f_b90d_3517;
    let noise: Vec<_> = (0..PAGE_SIZE).map(|_| random(&mut seed)).collect();
    for page in [vec![0; PAGE_SIZE], vec![73; PAGE_SIZE], noise.clone()] {
        for policy in [PageCompression::Raw, PageCompression::Lz4] {
            let encoded = encode(&page, policy)?;
            assert!(encoded.len() <= MAX_ENCODED_PAGE);
            assert_eq!(decode(&encoded)?, page);
            if policy == PageCompression::Raw || page == noise {
                assert_eq!(encoded[0], 0);
                assert_eq!(encoded.len(), MAX_ENCODED_PAGE);
            } else {
                assert_eq!(encoded[0], 1);
                assert!(encoded.len() < PAGE_SIZE / 10);
            }
        }
    }
    for len in [0, PAGE_SIZE - 1, PAGE_SIZE + 1] {
        assert!(encode(&vec![0; len], PageCompression::Lz4).is_err());
    }
    Ok(())
}

#[test]
fn malformed_and_oversized_pages_are_rejected() -> io::Result<()> {
    let good = encode(&vec![0; PAGE_SIZE], PageCompression::Lz4)?;
    for len in 0..good.len() {
        assert!(decode(&good[..len]).is_err(), "accepted prefix {len}");
    }
    for len in [0, 1, PAGE_SIZE - 1, PAGE_SIZE + 1, u16::MAX as usize] {
        let mut bad = good.clone();
        bad[1..3].copy_from_slice(&(len as u16).to_le_bytes());
        assert!(decode(&bad).is_err());
    }
    let mut bad = good.clone();
    bad[0] = 2;
    assert!(decode(&bad).is_err());
    let raw = encode(&vec![0; PAGE_SIZE], PageCompression::Raw)?;
    assert!(decode(&raw[..raw.len() - 1]).is_err());
    let mut oversized = raw;
    oversized.push(0);
    assert!(decode(&oversized).is_err());
    // Well-formed LZ4 of the wrong size must fail too, including expansion
    // beyond the fixed output buffer; no allocation uses a claimed length.
    for size in [1, PAGE_SIZE - 1, PAGE_SIZE + 1, PAGE_SIZE * 16] {
        let mut wrong = vec![1, 0, 16];
        wrong.extend(lz4_flex::block::compress(&vec![0; size]));
        assert!(decode(&wrong).is_err());
    }
    assert!(decode(&[1, 0, 16, 0, 0, 0]).is_err()); // invalid back-reference
    Ok(())
}

#[test]
fn seeded_page_roundtrips_and_arbitrary_inputs_are_bounded() -> io::Result<()> {
    let mut seed = 0x4613_0a91_ade8_5397;
    for iteration in 0..2000 {
        let mut page = vec![0; PAGE_SIZE];
        // Mixed entropy, rather than only all-zero or uniformly random pages.
        for byte in &mut page[..iteration * 7919 % (PAGE_SIZE + 1)] {
            *byte = random(&mut seed);
        }
        assert_eq!(decode(&encode(&page, PageCompression::Lz4)?)?, page);
        let len = iteration * 3571 % (MAX_ENCODED_PAGE + 32);
        let mut bytes: Vec<_> = (0..len).map(|_| random(&mut seed)).collect();
        if bytes.len() >= HEADER {
            bytes[..HEADER].copy_from_slice(&[1, 0, 16]);
        }
        if let Ok(decoded) = decode(&bytes) {
            assert_eq!(decoded.len(), PAGE_SIZE);
        }
    }
    Ok(())
}

/// Decoding never panics on a mutated encoding of any tag, with or without
/// the base a tag 3 page needs, or with a base of the wrong size; what
/// decodes is one page.
#[test]
fn mutated_encodings_of_every_tag_never_panic() -> io::Result<()> {
    let mut seed = 0x0dd5_eed0_c0de_c0de;
    let base: Vec<u8> = (0..PAGE_SIZE).map(|i| (i / 7) as u8).collect();
    let mut page = base.clone();
    for byte in &mut page[100..900] {
        *byte = random(&mut seed);
    }
    let encodings = [
        encode(&page, PageCompression::Raw)?,
        encode(&page, PageCompression::Lz4)?,
        encode(&page, PageCompression::Zstd)?,
        encode_against(&page, &base, PageCompression::Zstd)?,
    ];
    assert_eq!(
        encodings.iter().map(|e| e[0]).collect::<Vec<_>>(),
        [0, 1, 2, 3]
    );
    for i in 0..8000 {
        let mut bytes = encodings[i % 4].clone();
        for _ in 0..1 + random(&mut seed) % 4 {
            let at = (random(&mut seed) as usize * 31 + i) % bytes.len().max(1);
            match random(&mut seed) % 4 {
                0 if at < bytes.len() => bytes[at] ^= 1 << (random(&mut seed) % 8),
                1 if at < bytes.len() => bytes[at] = random(&mut seed),
                2 => bytes.insert(at.min(bytes.len()), random(&mut seed)),
                _ => bytes.truncate(at),
            }
        }
        let bytes = bytes::Bytes::from(bytes);
        let decoded = [
            decode_bytes(&bytes),
            decode_against(&bytes, None),
            decode_against(&bytes, Some(&base)),
            decode_against(&bytes, Some(&base[..100])),
        ];
        for page in decoded.into_iter().flatten() {
            assert_eq!(page.len(), PAGE_SIZE);
        }
    }
    Ok(())
}
