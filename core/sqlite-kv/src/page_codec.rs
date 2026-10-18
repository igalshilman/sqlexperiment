//! Independent immutable-page envelopes; logical pages remain 4096 bytes.
//!
//! The value starts with a codec tag (u8) and logical size (u16 LE, always 4096),
//! followed by raw bytes (tag 0), one LZ4 block (tag 1) or one zstd frame (tag 2).
//! Compression is retained only when its payload is smaller than the raw page. Incompressible pages therefore use
//! 4099 bytes including the header, even under the LZ4 policy.
//!
//! Raw and compressed pages can coexist. Write policy is per session, while stored
//! tags determine decoding; no dictionary, neighboring page, frame stream or policy
//! setting is needed to read an object. Dirty pages and the engine WAL are not encoded.
//!
//! Decode allocates exactly one logical page, never a stored-length-sized buffer.
//! Bounds, tag, logical size and LZ4 expansion are validated before a decoded page
//! enters the snapshot cache. A valid-looking bit change can remain undetected:
//! there is no checksum or corruption repair.
use crate::io::BLOCK_SIZE;
use std::cell::RefCell;
use std::io;
use zstd_safe::{CCtx, DCtx};

thread_local! {
    // A zstd context costs more to set up than a page to code: one per thread.
    static ZSTD_C: RefCell<CCtx<'static>> = RefCell::new(CCtx::create());
    static ZSTD_D: RefCell<DCtx<'static>> = RefCell::new(DCtx::create());
}

const HEADER: usize = 3;
/// A page held in memory, encoded or decoded, shared without copying.
pub(crate) type Page = bytes::Bytes;
/// Largest valid encoded value: a three-byte envelope plus one raw logical page.
pub const MAX_ENCODED_PAGE: usize = HEADER + BLOCK_SIZE;

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

const ZSTD_LEVEL: i32 = 3;

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

/// Tag (0 = raw, 1 = LZ4 block, 2 = zstd frame), u16 LE logical length, then
/// payload. Compression is retained only when it is smaller than the raw payload.
pub fn encode(page: &[u8], policy: PageCompression) -> io::Result<Vec<u8>> {
    if page.len() != BLOCK_SIZE {
        return Err(invalid("invalid logical page length"));
    }
    let compressed = match policy {
        PageCompression::Raw => None,
        PageCompression::Lz4 => Some((1, lz4_flex::block::compress(page))),
        PageCompression::Zstd => {
            let mut frame = Vec::with_capacity(zstd_safe::compress_bound(BLOCK_SIZE));
            ZSTD_C
                .with_borrow_mut(|cctx| cctx.compress(&mut frame, page, ZSTD_LEVEL))
                .map_err(|e| io::Error::other(zstd_safe::get_error_name(e)))?;
            Some((2, frame))
        }
    };
    let (tag, payload) = match &compressed {
        Some((tag, bytes)) if bytes.len() < BLOCK_SIZE => (*tag, &bytes[..]),
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
    decode_bytes(&bytes::Bytes::copy_from_slice(bytes)).map(Vec::from)
}

/// Like [`decode`], but a raw page is returned as a slice of `bytes` without
/// copying; only compressed pages allocate.
pub(crate) fn decode_bytes(bytes: &bytes::Bytes) -> io::Result<Page> {
    if bytes.len() < HEADER || bytes.len() > MAX_ENCODED_PAGE {
        return Err(invalid("invalid encoded page length"));
    }
    if u16::from_le_bytes([bytes[1], bytes[2]]) as usize != BLOCK_SIZE {
        return Err(invalid("invalid decoded page length"));
    }
    let payload = &bytes[HEADER..];
    match bytes[0] {
        0 if payload.len() == BLOCK_SIZE => Ok(bytes.slice(HEADER..)),
        1 if !payload.is_empty() && payload.len() < BLOCK_SIZE => {
            let mut page = vec![0; BLOCK_SIZE];
            let decoded = lz4_flex::block::decompress_into(payload, &mut page)
                .map_err(|_| invalid("invalid LZ4 page"))?;
            if decoded != BLOCK_SIZE {
                return Err(invalid("invalid LZ4 decoded length"));
            }
            Ok(page.into())
        }
        2 if !payload.is_empty() && payload.len() < BLOCK_SIZE => {
            let mut page = Vec::with_capacity(BLOCK_SIZE);
            let decoded = ZSTD_D
                .with_borrow_mut(|dctx| dctx.decompress(&mut page, payload))
                .map_err(|_| invalid("invalid zstd page"))?;
            if decoded != BLOCK_SIZE {
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
