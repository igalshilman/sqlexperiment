//! Page batches. A batch holds whole pages, compressed against what they
//! replace: their contents in the image the batch applies to (golden, for the
//! first batch of a chain). Zstd takes those as a prefix it matches against,
//! so a page that barely changed, or whose data moved, costs a few bytes.
//!
//! Layout: `QPD2`, the page count and the gaps between the sorted page indexes
//! as LEB128, then one Zstd frame of the pages, back to back.
use crate::model::{MAX_MEMORY, PAGE};
use anyhow::{Context, Result, ensure};
use zstd::zstd_safe::{CCtx, CParameter, DCtx, DParameter};

const MAGIC: &[u8] = b"QPD2";
const MAX_PAGES: usize = MAX_MEMORY / PAGE;
/// A window over the reference and the pages: twice the largest memory.
const MAX_WINDOW_LOG: u32 = (2 * MAX_MEMORY).ilog2();

/// Encode `pages` (back to back) at sorted, unique `indexes`, against
/// `reference`: what they replace, in the same order.
pub(crate) fn encode_pages(indexes: &[u32], pages: &[u8], reference: &[u8]) -> Result<Vec<u8>> {
    ensure!(
        indexes.len() <= MAX_PAGES
            && pages.len() == indexes.len() * PAGE
            && reference.len() == pages.len(),
        "Invalid page batch"
    );
    let mut blob = MAGIC.to_vec();
    leb128(&mut blob, indexes.len() as u64);
    let mut next = 0;
    for &index in indexes {
        ensure!(
            index >= next && (index as usize) < MAX_PAGES,
            "Pages must be sorted, unique and in memory"
        );
        leb128(&mut blob, u64::from(index - next));
        next = index + 1;
    }
    let mut zstd = CCtx::create();
    let window = (2 * pages.len()).max(1 << 17).next_power_of_two().ilog2();
    for parameter in [
        CParameter::CompressionLevel(2),
        CParameter::WindowLog(window),
        CParameter::EnableLongDistanceMatching(true),
    ] {
        zstd.set_parameter(parameter).map_err(zstd_error)?;
    }
    zstd.ref_prefix(reference).map_err(zstd_error)?;
    let mut frame = Vec::with_capacity(zstd::zstd_safe::compress_bound(pages.len()));
    zstd.compress2(&mut frame, pages).map_err(zstd_error)?;
    blob.extend_from_slice(&frame);
    Ok(blob)
}

/// A batch's page indexes, and its compressed pages for [`decode_pages`].
pub(crate) fn page_indexes(blob: &[u8]) -> Result<(Vec<u32>, &[u8])> {
    let mut rest = blob.strip_prefix(MAGIC).context("Invalid page batch")?;
    let count = read_leb128(&mut rest)? as usize;
    ensure!(count <= MAX_PAGES, "Too many pages");
    let mut indexes = Vec::with_capacity(count);
    let mut next = 0_u64;
    for _ in 0..count {
        let index = next + read_leb128(&mut rest)?;
        ensure!(index < MAX_PAGES as u64, "Invalid page index");
        indexes.push(index as u32);
        next = index + 1;
    }
    Ok((indexes, rest))
}

/// The pages of a batch, against the same `reference` it was encoded with.
pub(crate) fn decode_pages(frame: &[u8], reference: &[u8]) -> Result<Vec<u8>> {
    let mut zstd = DCtx::create();
    zstd.set_parameter(DParameter::WindowLogMax(MAX_WINDOW_LOG))
        .map_err(zstd_error)?;
    zstd.ref_prefix(reference).map_err(zstd_error)?;
    let mut pages = Vec::with_capacity(reference.len());
    let read = zstd.decompress(&mut pages, frame).map_err(zstd_error)?;
    ensure!(read == reference.len(), "Invalid page batch length");
    Ok(pages)
}

fn leb128(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            return out.push(byte);
        }
        out.push(byte | 0x80);
    }
}

fn read_leb128(input: &mut &[u8]) -> Result<u64> {
    let mut value = 0;
    for shift in (0..64).step_by(7) {
        let (&byte, rest) = input.split_first().context("Truncated page batch")?;
        *input = rest;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    anyhow::bail!("Invalid page batch")
}

fn zstd_error(code: usize) -> anyhow::Error {
    anyhow::anyhow!("Zstd: {}", zstd::zstd_safe::get_error_name(code))
}
