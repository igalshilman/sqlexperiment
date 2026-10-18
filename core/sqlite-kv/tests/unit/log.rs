use super::*;
use crate::io::BLOCK_SIZE;
use bytes::Bytes;
use std::collections::BTreeSet;

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

fn chain(images: Images) -> Chain {
    Chain { images }
}

#[test]
fn transactions_round_trip_and_split_into_segments() -> io::Result<()> {
    let small = Images::from([(0, Some(image(1))), (3, None), (900, Some(image(2)))]);
    let segments = encode_transaction(&small, &Chain::default())?;
    assert_eq!(segments.len(), 1);
    assert_eq!(decode_segment(&segments[0], &Chain::default())?, small);

    // Holes only: no frame.
    let holes = Images::from([(2, None)]);
    let segments = encode_transaction(&holes, &Chain::default())?;
    assert_eq!(decode_segment(&segments[0], &Chain::default())?, holes);

    // An empty transaction has no segment.
    assert!(encode_transaction(&Images::new(), &Chain::default())?.is_empty());

    // Pages beyond one segment's capacity span several.
    let large: Images = (0..300).map(|i| (i, Some(varied(i, 0)))).collect();
    let segments = encode_transaction(&large, &Chain::default())?;
    assert_eq!(segments.len(), 2);
    let mut merged = Images::new();
    for segment in &segments {
        assert!(segment.len() < MAX_SEGMENT_BYTES);
        merged.extend(decode_segment(segment, &Chain::default())?);
    }
    assert_eq!(merged, large);
    Ok(())
}

/// A page costs about what changed in it since its previous image in the
/// chain, and decodes only against that chain.
#[test]
fn segments_compress_against_the_previous_images() -> io::Result<()> {
    let changed = Images::from([(1, Some(varied(1, 40)))]);
    let alone = encode_transaction(&changed, &Chain::default())?;
    let previous = chain(Images::from([(1, Some(varied(1, 0))), (2, Some(image(9)))]));
    let segments = encode_transaction(&changed, &previous)?;
    assert!(alone[0].len() > 3000, "{} bytes alone", alone[0].len());
    assert!(
        segments[0].len() < 200,
        "{} bytes against the chain",
        segments[0].len()
    );
    assert_eq!(decode_segment(&segments[0], &previous)?, changed);

    // Another previous image, or none, fails the frame's checksum.
    let mut other = previous.clone();
    other.images.insert(1, Some(varied(2, 0)));
    assert!(decode_segment(&segments[0], &other).is_err());
    assert!(decode_segment(&segments[0], &Chain::default()).is_err());
    Ok(())
}

fn record() -> io::Result<Commit> {
    let first = encode_transaction(&Images::from([(0, Some(image(1)))]), &Chain::default())?;
    Ok(Commit {
        version: 9,
        epoch: 2,
        size: 3 * 4096,
        next_segment: 12,
        garbage: BTreeSet::from([4, 5, 6]),
        effects: Bytes::from_static(b"out"),
        log: vec![
            Logged {
                size: 4096,
                parts: first.into_iter().map(Part::Inline).collect(),
            },
            Logged {
                size: 2 * 4096,
                parts: vec![Part::Stored(7, 0), Part::Stored(7, 1)],
            },
            Logged {
                size: 3 * 4096,
                parts: Vec::new(),
            },
        ],
        chain: vec![Part::Inline(Bytes::from_static(b"vm0")), Part::Stored(8, 0)],
    })
}

#[test]
fn commit_records_round_trip() -> io::Result<()> {
    let commit = record()?;
    assert_eq!(Commit::decode(&commit.encode())?, commit);
    assert_eq!(
        commit.stored_values().into_iter().collect::<Vec<_>>(),
        [7, 8]
    );
    let empty = Commit::default();
    assert_eq!(Commit::decode(&empty.encode())?, empty);
    // `SC14`, version, epoch, units, next segment, garbage {4, 5, 6} (as
    // deltas), the effects, 3 transactions.
    let bytes = commit.encode();
    assert_eq!(
        bytes[..17],
        *b"SC14\x09\x02\x18\x0c\x03\x04\x01\x01\x03out\x03"
    );
    // Then the chain: 2 parts, inline `vm0`, and value 8's first.
    assert!(bytes.ends_with(b"\x02\x00\x03vm0\x01\x08\x00"));
    // A chain part may not name garbage.
    let mut named = commit.clone();
    named.chain[1] = Part::Stored(4, 0);
    assert!(Commit::decode(&named.encode()).is_err());
    Ok(())
}

#[test]
fn malformed_segments_and_records_are_rejected() -> io::Result<()> {
    let images = Images::from([(1, Some(image(1))), (2, None)]);
    let valid = encode_transaction(&images, &Chain::default())?
        .remove(0)
        .to_vec();
    let mut invalid: Vec<Vec<u8>> = (0..valid.len()).map(|n| valid[..n].to_vec()).collect();
    let mut bytes = valid.clone();
    bytes.push(0);
    invalid.push(bytes);
    // 2 entries: page 1 with an image, then page 2, a hole.
    assert_eq!(valid[..3], [2, 3, 0]);
    let mut bytes = valid.clone();
    bytes[0] = 241;
    invalid.push(bytes); // more entries than a segment holds
    let mut bytes = valid.clone();
    bytes[2] = 1;
    invalid.push(bytes); // a hole that claims an image: the frame is short
    let mut bytes = valid.clone();
    bytes[2] = 0x80;
    invalid.push(bytes); // a varint that does not end
    let mut bytes = valid.clone();
    let last = bytes.len() - 1;
    bytes[last] ^= 1;
    invalid.push(bytes); // the frame's checksum
    for bytes in invalid {
        assert!(
            decode_segment(&bytes, &Chain::default()).is_err(),
            "accepted {} bytes",
            bytes.len()
        );
    }

    let valid = record()?.encode().to_vec();
    let mut invalid: Vec<Vec<u8>> = (0..valid.len()).map(|n| valid[..n].to_vec()).collect();
    let mut bytes = valid.clone();
    bytes.push(0);
    invalid.push(bytes); // trailing bytes
    let mut bytes = valid.clone();
    bytes[3] = b'3';
    invalid.push(bytes); // another format
    for bad in [
        Commit {
            garbage: BTreeSet::from([7]),
            ..record()?
        }, // garbage the log names
        Commit {
            garbage: BTreeSet::from([4, 12]),
            ..record()?
        }, // garbage not allocated
        Commit {
            version: 2,
            ..record()?
        }, // more transactions than ever committed
        Commit {
            size: 4 * 4096,
            ..record()?
        }, // the last transaction's size differs
    ] {
        invalid.push(bad.encode().to_vec());
    }
    let mut large = record()?;
    large.effects = Bytes::from(vec![0; MAX_EFFECTS_BYTES + 1]);
    invalid.push(large.encode().to_vec());
    let mut named = record()?;
    named.log[1].parts[0] = Part::Stored(12, 0); // not allocated
    invalid.push(named.encode().to_vec());
    let mut named = record()?;
    named.log[1].parts[0] = Part::Stored(5, 0); // garbage
    invalid.push(named.encode().to_vec());
    for bytes in invalid {
        assert!(Commit::decode(&Bytes::from(bytes)).is_err());
    }
    Ok(())
}

/// Replay reads the stored values a record names that it does not hold
/// already, together, and decodes every segment in order against the ones
/// before it; a missing value or segment fails.
#[cfg(feature = "simulation")]
#[test]
fn replay_reads_stored_values() -> io::Result<()> {
    use crate::{format::log_key, simulation::MemoryKv};
    futures::executor::block_on(async {
        let store = MemoryKv::default();
        let first = Images::from([(0, Some(varied(3, 0)))]);
        let second = Images::from([(0, Some(varied(3, 30))), (1, Some(image(4)))]);
        let third = Images::from([(1, Some(varied(4, 9)))]);
        let mut log = Log::new();
        let inline = encode_transaction(&first, &log.chain)?;
        log.append(4096, first, 0);
        let sealed = encode_transaction(&second, &log.chain)?;
        log.append(2 * 4096, second.clone(), 0);
        let last = encode_transaction(&third, &log.chain)?;
        // The second transaction sealed with the third, into one value.
        let value = stored_value(&[sealed[0].clone(), last[0].clone()]);
        store.put(&log_key(5), value.clone()).await?;
        let commit = Commit {
            version: 3,
            size: 2 * 4096,
            next_segment: 6,
            log: vec![
                Logged {
                    size: 4096,
                    parts: inline.into_iter().map(Part::Inline).collect(),
                },
                Logged {
                    size: 2 * 4096,
                    parts: vec![Part::Stored(5, 0)],
                },
                Logged {
                    size: 2 * 4096,
                    parts: vec![Part::Stored(5, 1)],
                },
            ],
            ..Commit::default()
        };
        let Replayed { log, values, .. } = replay(&store, &commit, Values::new()).await?;
        assert_eq!(log.transactions.len(), 3);
        let mut latest = second;
        latest.extend(third);
        assert_eq!(log.images, latest);
        assert_eq!(values, Values::from([(5, value.clone())]));

        // A value held already is not read again.
        store.delete(&log_key(5)).await?;
        replay(&store, &commit, values).await?;
        assert!(replay(&store, &commit, Values::new()).await.is_err());
        let mut beyond = commit.clone();
        beyond.log[2].parts[0] = Part::Stored(5, 2);
        let held = Values::from([(5, value)]);
        assert!(replay(&store, &beyond, held).await.is_err());
        Ok(())
    })
}

/// Stored numbers far outside what this crate writes are rejected, so a
/// corrupt record cannot overflow arithmetic or size a huge view.
#[test]
fn implausible_numbers_are_rejected() -> io::Result<()> {
    let commit = |version, size| {
        Commit {
            version,
            size,
            ..Commit::default()
        }
        .encode()
    };
    assert!(Commit::decode(&commit(u64::MAX - 1, 0)).is_err());
    assert!(Commit::decode(&commit(MAX_SEQUENCE + 1, 0)).is_err());
    assert!(Commit::decode(&commit(2, MAX_DATABASE_BYTES + 512)).is_err());
    assert!(Commit::decode(&commit(2, 512 * 3)).is_ok());

    let images = Images::from([(MAX_DATABASE_BYTES / 4096, None)]);
    let segment = encode_transaction(&images, &Chain::default())?.remove(0);
    assert!(entries(&segment).is_err(), "page index out of range");
    Ok(())
}
