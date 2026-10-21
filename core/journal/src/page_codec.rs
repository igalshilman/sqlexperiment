//! Independent immutable-page envelopes; logical pages remain 4096 bytes.
//!
//! The value starts with a codec tag (u8) and logical size (u16 LE, always 4096),
//! followed by raw bytes (tag 0), one LZ4 block (tag 1), one zstd frame (tag 2),
//! or one zstd frame whose dictionary is the page's base (tag 3: a page of a
//! file with a base, see [`crate::Base`], which decoding needs).
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
use crate::PAGE_SIZE;
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
pub type Page = bytes::Bytes;
/// Largest valid encoded value: a three-byte envelope plus one raw logical page.
pub const MAX_ENCODED_PAGE: usize = HEADER + PAGE_SIZE;

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
    if page.len() != PAGE_SIZE {
        return Err(invalid("invalid logical page length"));
    }
    let compressed = match policy {
        PageCompression::Raw => None,
        PageCompression::Lz4 => Some((1, lz4_flex::block::compress(page))),
        PageCompression::Zstd => {
            let mut frame = Vec::with_capacity(zstd_safe::compress_bound(PAGE_SIZE));
            ZSTD_C
                .with_borrow_mut(|cctx| cctx.compress(&mut frame, page, ZSTD_LEVEL))
                .map_err(|e| io::Error::other(zstd_safe::get_error_name(e)))?;
            Some((2, frame))
        }
    };
    let (tag, payload) = match &compressed {
        Some((tag, bytes)) if bytes.len() < PAGE_SIZE => (*tag, &bytes[..]),
        _ => (0, page),
    };
    let mut out = Vec::with_capacity(HEADER + payload.len());
    out.push(tag);
    out.extend_from_slice(&(PAGE_SIZE as u16).to_le_bytes());
    out.extend_from_slice(payload);
    Ok(out)
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
    let mut frame = Vec::with_capacity(zstd_safe::compress_bound(PAGE_SIZE));
    let mut cctx = CCtx::create();
    cctx.set_parameter(zstd_safe::CParameter::CompressionLevel(ZSTD_LEVEL))
        .and_then(|_| cctx.ref_prefix(base))
        .and_then(|_| cctx.compress2(&mut frame, page))
        .map_err(|e| io::Error::other(zstd_safe::get_error_name(e)))?;
    if HEADER + frame.len() >= own.len() {
        return Ok(own);
    }
    let mut out = Vec::with_capacity(HEADER + frame.len());
    out.push(3);
    out.extend_from_slice(&(PAGE_SIZE as u16).to_le_bytes());
    out.extend_from_slice(&frame);
    Ok(out)
}

/// [`decode_bytes`], for a page of a file whose base page is `base`: a page
/// encoded against it (tag 3) needs it.
pub(crate) fn decode_against(bytes: &bytes::Bytes, base: Option<&[u8]>) -> io::Result<Page> {
    if bytes.first() != Some(&3) {
        return decode_bytes(bytes);
    }
    let base = base.ok_or_else(|| invalid("a page encoded against a base, without it"))?;
    if bytes.len() < HEADER || bytes.len() > MAX_ENCODED_PAGE {
        return Err(invalid("invalid encoded page length"));
    }
    if u16::from_le_bytes([bytes[1], bytes[2]]) as usize != PAGE_SIZE {
        return Err(invalid("invalid decoded page length"));
    }
    let mut page = Vec::with_capacity(PAGE_SIZE);
    let mut dctx = DCtx::create();
    dctx.ref_prefix(base)
        .and_then(|_| dctx.decompress(&mut page, &bytes[HEADER..]))
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
    if bytes.len() < HEADER || bytes.len() > MAX_ENCODED_PAGE {
        return Err(invalid("invalid encoded page length"));
    }
    if u16::from_le_bytes([bytes[1], bytes[2]]) as usize != PAGE_SIZE {
        return Err(invalid("invalid decoded page length"));
    }
    let payload = &bytes[HEADER..];
    match bytes[0] {
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
            let decoded = ZSTD_D
                .with_borrow_mut(|dctx| dctx.decompress(&mut page, payload))
                .map_err(|_| invalid("invalid zstd page"))?;
            if decoded != PAGE_SIZE {
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
