//! Page batches. A batch holds pages, encoded against what they replace:
//! their contents in the image the batch applies to (golden, for the first
//! batch of a chain). Only the 64-byte blocks that differ are kept, found by
//! comparing words, and compressed as one Zstd frame whose prefix is the
//! blocks they replace: a block that barely changed, or whose data moved
//! from another block, costs a few bytes.
//!
//! Layout: `QPD3`; the page count and the gaps between the sorted page
//! indexes, as LEB128; each page's changed blocks (a count, then the block
//! numbers if fewer than 8, else a 64-bit mask); then the Zstd frame of the
//! changed blocks, back to back.
use crate::model::{MAX_MEMORY, PAGE};
use anyhow::{Context, Result, ensure};
use std::cell::RefCell;
use zstd::zstd_safe::{CCtx, CParameter, DCtx, DParameter, ResetDirective};

const MAGIC: &[u8] = b"QPD3";
const MAX_PAGES: usize = MAX_MEMORY / PAGE;
const BLOCK: usize = 64;
/// Changed blocks past which a page is kept whole.
const DENSE: u32 = 16;
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
    let (mut changed, mut replaced) = (Vec::new(), Vec::new());
    for (new, old) in pages
        .as_chunks::<PAGE>()
        .0
        .iter()
        .zip(reference.as_chunks::<PAGE>().0)
    {
        let (new, old) = (new.as_chunks::<BLOCK>().0, old.as_chunks::<BLOCK>().0);
        let mut mask = new
            .iter()
            .zip(old)
            .enumerate()
            .fold(0_u64, |mask, (block, (new, old))| {
                mask | u64::from(differs(new, old)) << block
            });
        // A page changed all over goes whole: its mask would cost more than
        // the unchanged blocks, which match the prefix for a few bytes each.
        if mask.count_ones() >= DENSE {
            mask = u64::MAX;
        }
        write_mask(&mut blob, mask);
        for block in blocks(mask) {
            changed.extend_from_slice(&new[block]);
            replaced.extend_from_slice(&old[block]);
        }
    }
    compress(&mut blob, &changed, &replaced)?;
    Ok(blob)
}

/// Whether two blocks differ: eight word compares, which the compiler turns
/// into a few vector instructions.
fn differs(a: &[u8; BLOCK], b: &[u8; BLOCK]) -> bool {
    let (a, b) = (a.as_chunks::<8>().0, b.as_chunks::<8>().0);
    a.iter().zip(b).fold(0, |any, (a, b)| {
        any | (u64::from_ne_bytes(*a) ^ u64::from_ne_bytes(*b))
    }) != 0
}

/// The block numbers set in `mask`, in order.
fn blocks(mut mask: u64) -> impl Iterator<Item = usize> {
    std::iter::from_fn(move || {
        (mask != 0).then(|| {
            let block = mask.trailing_zeros() as usize;
            mask &= mask - 1;
            block
        })
    })
}

fn write_mask(out: &mut Vec<u8>, mask: u64) {
    let count = mask.count_ones() as u8;
    out.push(count);
    if mask == u64::MAX {
        // The whole page: nothing to say which.
    } else if count < 8 {
        out.extend(blocks(mask).map(|block| block as u8));
    } else {
        out.extend_from_slice(&mask.to_le_bytes());
    }
}

fn read_mask(input: &mut &[u8]) -> Result<u64> {
    let (&count, rest) = input.split_first().context("Truncated page batch")?;
    ensure!(count as usize <= PAGE / BLOCK, "Invalid block count");
    let size = match count {
        64 => 0,
        0..8 => count as usize,
        _ => 8,
    };
    ensure!(rest.len() >= size, "Truncated page batch");
    let (bytes, rest) = rest.split_at(size);
    *input = rest;
    let mask = if count == 64 {
        u64::MAX
    } else if count < 8 {
        bytes.iter().try_fold(0_u64, |mask, &block| {
            ensure!((block as usize) < PAGE / BLOCK, "Invalid block");
            Ok(mask | 1 << block)
        })?
    } else {
        u64::from_le_bytes(bytes.try_into()?)
    };
    ensure!(mask.count_ones() == u32::from(count), "Invalid block mask");
    Ok(mask)
}

thread_local! {
    /// One compression context per thread, reused: setting one up costs more
    /// than compressing a typical turn's blocks.
    static COMPRESSOR: RefCell<CCtx<'static>> = RefCell::new(CCtx::create());
}

/// Append `data`, compressed with `prefix` as the history it matches against.
fn compress(out: &mut Vec<u8>, data: &[u8], prefix: &[u8]) -> Result<()> {
    COMPRESSOR.with_borrow_mut(|zstd| {
        zstd.reset(ResetDirective::SessionOnly)
            .map_err(zstd_error)?;
        let window = (2 * data.len()).max(1 << 17).next_power_of_two().ilog2();
        for parameter in [
            CParameter::CompressionLevel(2),
            CParameter::WindowLog(window),
            CParameter::EnableLongDistanceMatching(true),
        ] {
            zstd.set_parameter(parameter).map_err(zstd_error)?;
        }
        // SAFETY: Zstd references a prefix for the next frame only, which
        // `compress2` completes before `prefix` goes out of scope.
        let prefix = unsafe { std::mem::transmute::<&[u8], &'static [u8]>(prefix) };
        zstd.ref_prefix(prefix).map_err(zstd_error)?;
        let mut frame = Vec::with_capacity(zstd::zstd_safe::compress_bound(data.len()));
        zstd.compress2(&mut frame, data).map_err(zstd_error)?;
        out.extend_from_slice(&frame);
        Ok(())
    })
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
pub(crate) fn decode_pages(mut encoded: &[u8], reference: &[u8]) -> Result<Vec<u8>> {
    ensure!(reference.len().is_multiple_of(PAGE), "Invalid reference");
    let masks = (0..reference.len() / PAGE)
        .map(|_| read_mask(&mut encoded))
        .collect::<Result<Vec<_>>>()?;
    let pages = reference.as_chunks::<PAGE>().0;
    let mut replaced = Vec::new();
    for (page, mask) in pages.iter().zip(&masks) {
        for block in blocks(*mask) {
            replaced.extend_from_slice(&page[block * BLOCK..][..BLOCK]);
        }
    }
    let mut zstd = DCtx::create();
    zstd.set_parameter(DParameter::WindowLogMax(MAX_WINDOW_LOG))
        .map_err(zstd_error)?;
    zstd.ref_prefix(&replaced).map_err(zstd_error)?;
    let mut changed = Vec::with_capacity(replaced.len());
    let read = zstd.decompress(&mut changed, encoded).map_err(zstd_error)?;
    ensure!(read == replaced.len(), "Invalid page batch length");
    let mut result = reference.to_vec();
    let mut changed = changed.as_chunks::<BLOCK>().0.iter();
    for (page, mask) in result.as_chunks_mut::<PAGE>().0.iter_mut().zip(&masks) {
        for block in blocks(*mask) {
            page[block * BLOCK..][..BLOCK]
                .copy_from_slice(changed.next().context("Missing block")?);
        }
    }
    Ok(result)
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
