//! Experimental v9 immutable-page envelope. Logical pages remain 4096 bytes.
//! Raw and LZ4 objects may coexist; the write policy is not a read requirement.
use crate::io::BLOCK_SIZE;
use std::io;

const HEADER: usize = 3;
pub const MAX_ENCODED_PAGE: usize = HEADER + BLOCK_SIZE;

/// Per-session write policy. Both policies can read either encoded form.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PageCompression {
    Raw,
    #[default]
    Lz4,
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

/// Tag (0 = raw, 1 = LZ4 block), u16 LE logical length, then payload.
/// Compression is retained only when it is smaller than the raw payload.
pub fn encode(page: &[u8], policy: PageCompression) -> io::Result<Vec<u8>> {
    if page.len() != BLOCK_SIZE {
        return Err(invalid("invalid logical page length"));
    }
    let compressed = (policy == PageCompression::Lz4).then(|| lz4_flex::block::compress(page));
    let (tag, payload) = match compressed.as_deref() {
        Some(bytes) if bytes.len() < BLOCK_SIZE => (1, bytes),
        _ => (0, page),
    };
    let mut out = Vec::with_capacity(HEADER + payload.len());
    out.push(tag);
    out.extend_from_slice(&(BLOCK_SIZE as u16).to_le_bytes());
    out.extend_from_slice(payload);
    Ok(out)
}

/// Decodes into exactly one bounded page. Never allocate from stored lengths.
/// This validates structure and size, not arbitrary corruption (no checksum).
pub fn decode(bytes: &[u8]) -> io::Result<Vec<u8>> {
    if bytes.len() < HEADER || bytes.len() > MAX_ENCODED_PAGE {
        return Err(invalid("invalid encoded page length"));
    }
    if u16::from_le_bytes([bytes[1], bytes[2]]) as usize != BLOCK_SIZE {
        return Err(invalid("invalid decoded page length"));
    }
    let payload = &bytes[HEADER..];
    match bytes[0] {
        0 if payload.len() == BLOCK_SIZE => Ok(payload.to_vec()),
        1 if !payload.is_empty() && payload.len() < BLOCK_SIZE => {
            let mut page = vec![0; BLOCK_SIZE];
            let decoded = lz4_flex::block::decompress_into(payload, &mut page)
                .map_err(|_| invalid("invalid LZ4 page"))?;
            if decoded != BLOCK_SIZE {
                return Err(invalid("invalid LZ4 decoded length"));
            }
            Ok(page)
        }
        _ => Err(invalid("invalid page codec or payload length")),
    }
}

#[cfg(test)]
#[path = "../tests/unit/page_codec.rs"]
mod tests;
