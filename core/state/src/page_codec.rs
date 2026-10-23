//! The encoding of a checkpointed page at `page/<file>/<n>`; logical pages
//! remain 4096 bytes.
//!
//! The value starts with a codec tag (u8) and logical size (u16 LE, always 4096),
//! followed by raw bytes (tag 0), one LZ4 block (tag 1), one zstd frame (tag 2),
//! or one zstd frame whose dictionary is the page's base (tag 3: a page of a
//! file with a base, see [`crate::Base`], which decoding needs).
//! Compression is retained only when its payload is smaller than the raw page.
//! Incompressible pages therefore use 4099 bytes including the header, even
//! under the LZ4 policy.
//!
//! Raw and compressed pages can coexist. Write policy is per session, while
//! stored tags determine decoding; apart from a file's base, no dictionary,
//! neighboring page or policy setting is needed to read a page.
//!
//! Decode allocates exactly one logical page, never a stored-length-sized buffer.
//! Bounds, tag, logical size and LZ4 expansion are validated before a decoded
//! page is used or cached. A valid-looking bit change can remain undetected:
//! there is no checksum or corruption repair.
use crate::{PAGE_SIZE, zstd};
use std::io;

const HEADER: usize = 3;
/// A page held in memory, encoded or decoded, shared without copying.
pub type Page = bytes::Bytes;
/// Largest valid encoded value: a three-byte envelope plus one raw logical page.
const MAX_ENCODED_PAGE: usize = HEADER + PAGE_SIZE;

/// Per-session write policy. Both policies can read either encoded form.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PageCompression {
    /// Store every new page verbatim behind the envelope; useful for CPU/traffic comparisons.
    Raw,
    /// Try an independent LZ4 block, falling back to raw if compression would not shrink it.
    Lz4,
    /// Try an independent zstd frame (level 3): smaller than LZ4 on SQL pages,
    /// at more CPU per page; raw if it would not shrink it.
    #[default]
    Zstd,
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

/// Encode a page under `policy`: tag 0 (raw), 1 (LZ4) or 2 (zstd), whichever
/// of raw and the policy's is smaller. Only the result allocates.
pub fn encode(page: &[u8], policy: PageCompression) -> io::Result<Vec<u8>> {
    if page.len() != PAGE_SIZE {
        return Err(invalid("invalid logical page length"));
    }
    let smaller = |tag, payload: &[u8]| (payload.len() < PAGE_SIZE).then(|| envelope(tag, payload));
    let compressed = match policy {
        PageCompression::Raw => None,
        PageCompression::Lz4 => {
            let mut block = [0; lz4_flex::block::get_maximum_output_size(PAGE_SIZE)];
            let len = lz4_flex::block::compress_into(page, &mut block)
                .map_err(|e| io::Error::other(e.to_string()))?;
            smaller(1, &block[..len])
        }
        PageCompression::Zstd => zstd::compress(page, &[], false, |frame| smaller(2, frame))?,
    };
    Ok(compressed.unwrap_or_else(|| envelope(0, page)))
}

/// `payload` behind the header: `tag` and the logical size.
fn envelope(tag: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER + payload.len());
    out.push(tag);
    out.extend_from_slice(&(PAGE_SIZE as u16).to_le_bytes());
    out.extend_from_slice(payload);
    out
}

/// The tag and payload of an encoded page, once its length and logical size
/// are valid.
fn open_envelope(bytes: &[u8]) -> io::Result<(u8, &[u8])> {
    if bytes.len() < HEADER || bytes.len() > MAX_ENCODED_PAGE {
        return Err(invalid("invalid encoded page length"));
    }
    if u16::from_le_bytes([bytes[1], bytes[2]]) as usize != PAGE_SIZE {
        return Err(invalid("invalid decoded page length"));
    }
    Ok((bytes[0], &bytes[HEADER..]))
}

/// Encode a page of a file with a base against `base`, its base's page: as a
/// zstd frame with the base page as its dictionary (tag 3) when that is
/// smaller than the page's own encoding, else as [`encode`] does.
pub(crate) fn encode_against(
    page: &[u8],
    base: &[u8],
    policy: PageCompression,
) -> io::Result<Vec<u8>> {
    let own = encode(page, policy)?;
    if policy == PageCompression::Raw || base.len() != PAGE_SIZE {
        return Ok(own);
    }
    let against = zstd::compress(page, base, false, |frame| {
        (HEADER + frame.len() < own.len()).then(|| envelope(3, frame))
    })?;
    Ok(against.unwrap_or(own))
}

/// [`decode_bytes`], for a page of a file whose base page is `base`: a page
/// encoded against it (tag 3) needs it.
pub(crate) fn decode_against(bytes: &bytes::Bytes, base: Option<&[u8]>) -> io::Result<Page> {
    if bytes.first() != Some(&3) {
        return decode_bytes(bytes);
    }
    let base = base.ok_or_else(|| invalid("a page encoded against a base, without it"))?;
    let (_, payload) = open_envelope(bytes)?;
    let mut page = Vec::with_capacity(PAGE_SIZE);
    zstd::decompress(&mut page, payload, base)
        .map_err(|_| invalid("invalid zstd page against its base"))?;
    if page.len() != PAGE_SIZE {
        return Err(invalid("invalid zstd decoded length"));
    }
    Ok(page.into())
}

/// Decodes into exactly one bounded page. Never allocate from stored lengths.
/// This validates structure and size, not arbitrary corruption (no checksum).
pub fn decode(bytes: &[u8]) -> io::Result<Vec<u8>> {
    decode_bytes(&bytes::Bytes::copy_from_slice(bytes)).map(Vec::from)
}

/// Like [`decode`], but a raw page is returned as a slice of `bytes` without
/// copying; only compressed pages allocate.
pub(crate) fn decode_bytes(bytes: &bytes::Bytes) -> io::Result<Page> {
    let (tag, payload) = open_envelope(bytes)?;
    match tag {
        0 if payload.len() == PAGE_SIZE => Ok(bytes.slice(HEADER..)),
        1 if !payload.is_empty() && payload.len() < PAGE_SIZE => {
            let mut page = vec![0; PAGE_SIZE];
            let decoded = lz4_flex::block::decompress_into(payload, &mut page)
                .map_err(|_| invalid("invalid LZ4 page"))?;
            if decoded != PAGE_SIZE {
                return Err(invalid("invalid LZ4 decoded length"));
            }
            Ok(page.into())
        }
        2 if !payload.is_empty() && payload.len() < PAGE_SIZE => {
            let mut page = Vec::with_capacity(PAGE_SIZE);
            zstd::decompress(&mut page, payload, &[]).map_err(|_| invalid("invalid zstd page"))?;
            if page.len() != PAGE_SIZE {
                return Err(invalid("invalid zstd decoded length"));
            }
            Ok(page.into())
        }
        _ => Err(invalid("invalid page codec or payload length")),
    }
}

#[cfg(test)]
#[path = "../tests/unit/page_codec.rs"]
mod tests;
