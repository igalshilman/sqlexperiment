//! Adaptive radix-node encoding. Dense nodes use a bitmap and IDs relative to
//! their minimum; scattered IDs fall back to fixed-width entries if smaller.
use std::{collections::BTreeMap, io};

pub(crate) type Node = BTreeMap<u8, u64>;

fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid radix node")
}

fn write_varint(mut value: u64, out: &mut Vec<u8>) {
    while value >= 128 {
        out.push(value as u8 | 128);
        value >>= 7;
    }
    out.push(value as u8);
}

fn read_varint(input: &mut &[u8]) -> io::Result<u64> {
    let mut value = 0u64;
    for byte_index in 0..10 {
        let (&byte, rest) = input.split_first().ok_or_else(invalid)?;
        *input = rest;
        if byte_index == 9 && byte > 1 {
            return Err(invalid());
        }
        value |= u64::from(byte & 127) << (byte_index * 7);
        if byte & 128 == 0 {
            if byte_index > 0 && byte == 0 {
                return Err(invalid()); // non-minimal encoding
            }
            return Ok(value);
        }
    }
    Err(invalid())
}

pub(crate) fn encode(node: &Node) -> Vec<u8> {
    if node.is_empty() {
        return Vec::new(); // only valid for an empty inline root
    }
    let mut raw = Vec::with_capacity(1 + node.len() * 9);
    raw.push(0);
    for (&edge, &id) in node {
        raw.push(edge);
        raw.extend_from_slice(&id.to_le_bytes());
    }

    let mut compact = vec![1];
    compact.extend_from_slice(&(node.len() as u16).to_le_bytes());
    if node.len() <= 32 {
        compact.extend(node.keys());
    } else {
        let mut bitmap = [0u8; 32];
        for &edge in node.keys() {
            bitmap[edge as usize / 8] |= 1 << (edge % 8);
        }
        compact.extend_from_slice(&bitmap);
    }
    let base = *node.values().min().unwrap();
    write_varint(base, &mut compact);
    for &id in node.values() {
        write_varint(id - base, &mut compact);
    }
    if compact.len() < raw.len() {
        compact
    } else {
        raw
    }
}

pub(crate) fn decode(bytes: &[u8], max_id: u64, allow_empty: bool) -> io::Result<Node> {
    if bytes.is_empty() {
        return if allow_empty {
            Ok(Node::new())
        } else {
            Err(invalid())
        };
    }
    if bytes.len() > 1 + 256 * 9 {
        return Err(invalid());
    }
    let mut node = Node::new();
    match bytes[0] {
        0 => {
            let entries = &bytes[1..];
            if entries.is_empty() || !entries.len().is_multiple_of(9) {
                return Err(invalid());
            }
            for entry in entries.as_chunks::<9>().0 {
                if node
                    .last_key_value()
                    .is_some_and(|(&edge, _)| edge >= entry[0])
                {
                    return Err(invalid());
                }
                node.insert(entry[0], u64::from_le_bytes(entry[1..].try_into().unwrap()));
            }
        }
        1 => {
            if bytes.len() < 3 {
                return Err(invalid());
            }
            let count = u16::from_le_bytes(bytes[1..3].try_into().unwrap()) as usize;
            if !(1..=256).contains(&count) {
                return Err(invalid());
            }
            let edge_len = count.min(32);
            let edges = bytes.get(3..3 + edge_len).ok_or_else(invalid)?;
            let indexes: Vec<u8> = if count <= 32 {
                if edges.windows(2).any(|w| w[0] >= w[1]) {
                    return Err(invalid());
                }
                edges.to_vec()
            } else {
                (0..=u8::MAX)
                    .filter(|&edge| edges[edge as usize / 8] & (1 << (edge % 8)) != 0)
                    .collect()
            };
            if indexes.len() != count {
                return Err(invalid());
            }
            let mut input = &bytes[3 + edge_len..];
            let base = read_varint(&mut input)?;
            if base == 0 || base > max_id {
                return Err(invalid());
            }
            let mut found_base = false;
            for edge in indexes {
                let delta = read_varint(&mut input)?;
                found_base |= delta == 0;
                let id = base.checked_add(delta).ok_or_else(invalid)?;
                node.insert(edge, id);
            }
            if !input.is_empty() || !found_base {
                return Err(invalid());
            }
        }
        _ => return Err(invalid()),
    }
    if node.values().any(|&id| id == 0 || id > max_id) {
        return Err(invalid());
    }
    Ok(node)
}

#[cfg(test)]
mod tests {
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
}
