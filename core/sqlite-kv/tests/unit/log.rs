use super::*;
use crate::io::BLOCK_SIZE;
use bytes::Bytes;

fn image(n: u8) -> Page {
    Bytes::from(vec![n; BLOCK_SIZE])
}

/// A page of random bytes from `seed`, its first `n` of them changed.
fn varied(seed: u64, n: usize) -> Page {
    let mut state = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
    let mut page: Vec<u8> = (0..BLOCK_SIZE)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as u8
        })
        .collect();
    for byte in page.iter_mut().take(n) {
        *byte ^= 0x5a;
    }
    Bytes::from(page)
}

#[test]
fn transactions_round_trip_and_split_into_parts() -> io::Result<()> {
    let small = Images::from([(0, Some(image(1))), (3, None), (900, Some(image(2)))]);
    let parts = encode_transaction(5, 901 * 4096, &small, &Chain::new(5))?;
    assert_eq!(parts.len(), 1);
    assert_eq!(
        decode(&parts[0].clone().into(), 5, &mut Chain::new(5))?,
        Segment {
            first: 5,
            commit: true,
            size: 901 * 4096,
            base: 5,
            images: small,
        }
    );

    // Holes only: no frame.
    let holes = Images::from([(2, None)]);
    let parts = encode_transaction(5, 4096, &holes, &Chain::new(5))?;
    assert_eq!(
        decode(&parts[0].clone().into(), 5, &mut Chain::new(5))?.images,
        holes
    );

    // Pages beyond one segment's capacity span several parts.
    let large: Images = (0..300).map(|i| (i, Some(varied(i, 0)))).collect();
    let parts = encode_transaction(7, 300 * 4096, &large, &Chain::new(7))?;
    assert_eq!(parts.len(), 2);
    let mut merged = Images::new();
    for (n, bytes) in parts.iter().enumerate() {
        assert!(bytes.len() <= MAX_SEGMENT_BYTES);
        let segment = decode(&bytes.clone().into(), 7 + n as u64, &mut Chain::new(7))?;
        assert_eq!((segment.first, segment.commit), (7, n == 1));
        merged.extend(segment.images);
    }
    assert_eq!(merged, large);
    Ok(())
}

/// A page costs about what changed in it since its previous image in the
/// chain, and decodes only against that chain.
#[test]
fn segments_compress_against_the_previous_images() -> io::Result<()> {
    let alone = encode_transaction(
        3,
        8192,
        &Images::from([(1, Some(varied(1, 40)))]),
        &Chain::new(1),
    )?;
    let chain = Chain {
        base: 1,
        images: Images::from([(1, Some(varied(1, 0))), (2, Some(image(9)))]),
    };
    let changed = Images::from([(1, Some(varied(1, 40)))]);
    let parts = encode_transaction(3, 8192, &changed, &chain)?;
    assert!(alone[0].len() > 3000, "{} bytes alone", alone[0].len());
    assert!(
        parts[0].len() < 200,
        "{} bytes against the chain",
        parts[0].len()
    );
    let bytes = Bytes::from(parts[0].clone());
    assert_eq!(decode(&bytes, 3, &mut chain.clone())?.images, changed);

    // Another previous image, or none, fails the frame's checksum.
    let mut other = chain.clone();
    other.images.insert(1, Some(varied(2, 0)));
    assert!(decode(&bytes, 3, &mut other).is_err());
    assert!(decode(&bytes, 3, &mut Chain::new(1)).is_err());

    // A later base restarts the chain; an earlier one is refused.
    let restarted = encode_transaction(3, 8192, &changed, &Chain::new(3))?;
    let mut followed = chain.clone();
    decode(&restarted[0].clone().into(), 3, &mut followed)?;
    assert_eq!(followed, Chain::new(3));
    assert!(decode(&bytes, 3, &mut Chain::new(2)).is_err());
    Ok(())
}

#[test]
fn malformed_segments_and_records_are_rejected() -> io::Result<()> {
    let images = Images::from([(1, Some(image(1))), (2, None)]);
    let valid = encode_transaction(9, 3 * 4096, &images, &Chain::new(9))?.remove(0);
    let chain = || Chain::new(9);
    assert!(
        decode(&valid.clone().into(), 8, &mut chain()).is_err(),
        "sequence must match its key"
    );
    let mut invalid: Vec<_> = (0..valid.len()).map(|n| valid[..n].to_vec()).collect();
    let mut bytes = valid.clone();
    bytes.push(0);
    invalid.push(bytes);
    let mut bytes = valid.clone();
    bytes[0] ^= 1;
    invalid.push(bytes);
    // `SL13`, flags, sequence 9, first and base 0 before, 24 units, 2 entries.
    assert_eq!(valid[4..12], [1, 9, 0, 0, 24, 2, 3, 0]);
    let mut bytes = valid.clone();
    bytes[4] = 2;
    invalid.push(bytes); // unknown flag
    let mut bytes = valid.clone();
    bytes[6] = 10;
    invalid.push(bytes); // transaction starts before sequence 0
    let mut bytes = valid.clone();
    bytes[7] = 10;
    invalid.push(bytes); // chain starts before sequence 0
    let mut bytes = valid.clone();
    bytes[9] = 241;
    invalid.push(bytes); // more entries than a segment holds
    let mut bytes = valid.clone();
    bytes[11] = 1;
    invalid.push(bytes); // a hole that claims an image: the frame is short
    let mut bytes = valid.clone();
    bytes[11] = 0x80;
    invalid.push(bytes); // a varint that does not end
    let mut bytes = valid.clone();
    let last = bytes.len() - 1;
    bytes[last] ^= 1;
    invalid.push(bytes); // the frame's checksum
    for bytes in invalid {
        assert!(
            decode(&bytes.clone().into(), 9, &mut chain()).is_err(),
            "accepted {} bytes",
            bytes.len()
        );
    }

    let meta = Meta {
        log_start: 9,
        deleted_below: 4,
        size: 8192,
    };
    assert_eq!(Meta::decode(&meta.encode())?, meta);
    for bad in [
        Meta {
            deleted_below: 10,
            ..meta
        },
        Meta {
            deleted_below: 0,
            ..meta
        },
    ] {
        assert!(Meta::decode(&bad.encode()).is_err());
    }
    assert!(Meta::decode(&meta.encode()[..27]).is_err());
    Ok(())
}

/// Replay applies whole transactions and stops at the first absent key or at a
/// segment of another transaction; what follows is debris of failed saves.
#[cfg(feature = "simulation")]
#[test]
fn replay_stops_at_incomplete_or_stale_transactions() -> io::Result<()> {
    use crate::{format::log_key, simulation::MemoryKv};
    futures::executor::block_on(async {
        let store = MemoryKv::default();
        // Each segment compressed against `previous`, page 0's image before it.
        let put = |seq: u64, first: u64, commit: bool, page: u8, previous: Option<u8>| {
            let chain = Chain {
                base: 1,
                images: previous.map_or_else(Images::new, |p| Images::from([(0, Some(image(p)))])),
            };
            let image = Some(image(page));
            let bytes = encode_segment(seq, first, commit, 4096, &[(0, &image)], &chain).unwrap();
            let store = store.clone();
            async move { store.put(&log_key(seq), bytes.into()).await }
        };
        put(1, 1, true, 1, None).await?; // committed
        put(2, 2, false, 2, Some(1)).await?; // a two-part transaction ...
        put(3, 2, true, 3, Some(1)).await?; // ... committed by its second part
        put(4, 4, false, 4, Some(3)).await?; // incomplete: its commit part never landed
        put(6, 4, true, 6, Some(3)).await?; // debris after the gap
        for batch in [2, 3, 16] {
            let log = replay(&store, 1, batch, None).await?;
            assert_eq!(log.transactions.len(), 2);
            assert_eq!(log.next, 4);
            assert_eq!(log.images[&0], Some(image(3)));
            assert_eq!(log.chain.images[&0], Some(image(3)));
            assert_eq!(
                log.tail.as_ref().map(|t| header(t, 3).unwrap().0.first),
                Some(2)
            );
        }

        // A later save overwrites the debris from `next`. A leftover part of
        // the failed transaction must not be taken as the continuation.
        put(4, 4, true, 7, Some(3)).await?;
        put(5, 2, true, 8, None).await?;
        let log = replay(&store, 1, 4, None).await?;
        assert_eq!(log.next, 5);
        assert_eq!(log.images[&0], Some(image(7)));
        Ok(())
    })
}

/// Stored numbers far outside what this crate writes are rejected, so a
/// corrupt record cannot overflow sequence arithmetic or size a huge view.
#[test]
fn implausible_numbers_are_rejected() -> io::Result<()> {
    let meta = |log_start, size| {
        Meta {
            log_start,
            deleted_below: 1,
            size,
        }
        .encode()
    };
    assert!(Meta::decode(&meta(u64::MAX - 1, 0)).is_err());
    assert!(Meta::decode(&meta(MAX_SEQUENCE + 1, 0)).is_err());
    assert!(Meta::decode(&meta(2, 4095)).is_err(), "not whole pages");
    assert!(Meta::decode(&meta(2, MAX_DATABASE_BYTES + 512)).is_err());
    assert!(Meta::decode(&meta(2, 512 * 3)).is_ok());

    let images = Images::from([(MAX_DATABASE_BYTES / 4096, None)]);
    let segment = Bytes::from(encode_transaction(1, 4096, &images, &Chain::new(1))?.remove(0));
    assert!(header(&segment, 1).is_err(), "page index out of range");
    let segment =
        Bytes::from(encode_transaction(1, 1 << 50, &Images::new(), &Chain::new(1))?.remove(0));
    assert!(header(&segment, 1).is_err(), "size out of range");
    Ok(())
}

/// A probe that would pass the largest sequence fails instead of wrapping.
#[cfg(feature = "simulation")]
#[test]
fn probes_do_not_overflow() {
    futures::executor::block_on(async {
        let store = crate::simulation::MemoryKv::default();
        assert!(probe(&store, u64::MAX - 1, 4).await.is_err());
        assert!(probe(&store, MAX_SEQUENCE - 1, 4).await.is_err());
    })
}
