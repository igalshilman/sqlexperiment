use super::*;
use crate::{
    io::BLOCK_SIZE,
    page_codec::{self, PageCompression},
};

fn image(n: u8) -> Vec<u8> {
    page_codec::encode(&vec![n; BLOCK_SIZE], PageCompression::Raw).unwrap()
}

#[test]
fn transactions_round_trip_and_split_into_parts() -> io::Result<()> {
    let small = Images::from([(0, Some(image(1))), (3, None), (900, Some(image(2)))]);
    let parts = encode_transaction(5, 901 * 4096, &small);
    assert_eq!(parts.len(), 1);
    assert_eq!(
        decode(&parts[0], 5)?,
        Segment {
            first: 5,
            commit: true,
            size: 901 * 4096,
            images: small,
        }
    );

    // Raw pages beyond one segment's capacity span several parts.
    let large: Images = (0..300).map(|i| (i, Some(image(i as u8)))).collect();
    let parts = encode_transaction(7, 300 * 4096, &large);
    assert_eq!(parts.len(), 2);
    let mut merged = Images::new();
    for (n, bytes) in parts.iter().enumerate() {
        assert!(bytes.len() <= MAX_SEGMENT_BYTES);
        let segment = decode(bytes, 7 + n as u64)?;
        assert_eq!((segment.first, segment.commit), (7, n == 1));
        merged.extend(segment.images);
    }
    assert_eq!(merged, large);
    Ok(())
}

#[test]
fn malformed_segments_and_records_are_rejected() -> io::Result<()> {
    let images = Images::from([(1, Some(image(1))), (2, None)]);
    let valid = encode_transaction(9, 3 * 4096, &images).remove(0);
    assert!(decode(&valid, 8).is_err(), "sequence must match its key");
    let mut invalid: Vec<_> = (0..valid.len()).map(|n| valid[..n].to_vec()).collect();
    let mut bytes = valid.clone();
    bytes.push(0);
    invalid.push(bytes);
    let mut bytes = valid.clone();
    bytes[0] ^= 1;
    invalid.push(bytes);
    let mut bytes = valid.clone();
    bytes[12..20].copy_from_slice(&10u64.to_le_bytes());
    invalid.push(bytes); // transaction starts after this segment
    let mut bytes = valid.clone();
    bytes[20] = 2;
    invalid.push(bytes); // unknown flag
    let mut bytes = valid.clone();
    bytes[PREFIX..PREFIX + 8].copy_from_slice(&5u64.to_le_bytes());
    invalid.push(bytes); // pages out of order
    let mut bytes = valid.clone();
    bytes[PREFIX + 10] = 9;
    invalid.push(bytes); // invalid page codec tag
    for bytes in invalid {
        assert!(decode(&bytes, 9).is_err(), "accepted {} bytes", bytes.len());
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
        let put = |seq: u64, first: u64, commit: bool, page: u8| {
            let mut bytes =
                encode_transaction(first, 4096, &Images::from([(0, Some(image(page)))])).remove(0);
            bytes[4..12].copy_from_slice(&seq.to_le_bytes());
            bytes[20] = u8::from(commit);
            let store = store.clone();
            async move { store.put(&log_key(seq), bytes).await }
        };
        put(1, 1, true, 1).await?; // committed
        put(2, 2, false, 2).await?; // a two-part transaction ...
        put(3, 2, true, 3).await?; // ... committed by its second part
        put(4, 4, false, 4).await?; // incomplete: its commit part never landed
        put(6, 4, true, 6).await?; // debris after the gap
        for batch in [2, 3, 16] {
            let log = replay(&store, 1, batch, None).await?;
            assert_eq!(log.transactions.len(), 2);
            assert_eq!(log.next, 4);
            assert_eq!(log.images[&0], Some(image(3)));
            assert_eq!(
                log.tail.as_deref().map(|t| decode(t, 3).unwrap().first),
                Some(2)
            );
        }

        // A later save overwrites the debris from `next`. A leftover part of
        // the failed transaction must not be taken as the continuation.
        put(4, 4, true, 7).await?;
        put(5, 2, true, 8).await?;
        let log = replay(&store, 1, 4, None).await?;
        assert_eq!(log.next, 5);
        assert_eq!(log.images[&0], Some(image(7)));
        Ok(())
    })
}
