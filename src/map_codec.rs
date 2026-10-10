//! Compact encoding of one 256-way radix node, also used for the embedded root.
//!
//! [`Node`] maps a byte-sized edge to an immutable child ID. The tree's height tells
//! the reader whether children are pages or lower map nodes; that level is not
//! repeated in every node. This codec describes references, not database payloads.
//!
//! | Tag | Encoding after the tag |
//! |---|---|
//! | 0 | Strictly sorted `(edge u8, ID u64 LE)` entries |
//! | 1 | Count u16 LE; edge list (count <= 32) or 32-byte bitmap; minimum ID as unsigned LEB128; then ID-minus-minimum varints in edge order |
//!
//! The encoder chooses whichever representation is smaller. This is automatic
//! encoding selection within one map layout, not a configurable storage algorithm.
//! The raw bound is 1 + 256 * 9 = 2305 bytes. Only an empty embedded root may use
//! zero bytes; external nodes must contain at least one edge.
//!
//! Decoding checks exact consumption, edge ordering, bitmap population, minimal
//! varints, overflow and nonzero IDs. External children must have smaller IDs than
//! their parent; root children are bounded by the head's allocation counter. These
//! checks reject malformed structure but are not a payload checksum.
use std::{collections::BTreeMap, io};

/// Sparse edge-to-object references. BTreeMap gives deterministic edge order for
/// encoding and rewrite grouping without allocating a 256-entry array per node.
/// This in-memory container is separate from the persisted radix-tree structure.
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

/// Choose the smaller raw or delta encoding; empty input encodes only an empty root.
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

/// Decode a complete node with an inclusive child-ID bound and explicit empty-root permission.
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
#[path = "../tests/unit/map_codec.rs"]
mod tests;
