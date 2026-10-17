use crate::model::{MAX_MEMORY, PAGE};
use anyhow::{Result, ensure};
use std::io::Read;

const MAX_RAW: usize = 8 + MAX_MEMORY / PAGE * (PAGE + 4);

pub(crate) fn compress(bytes: &[u8]) -> Result<Vec<u8>> {
    Ok(zstd::bulk::compress(bytes, 3)?)
}

/// Bound decoded output to the experiment's memory limit.
pub(crate) fn decompress(bytes: &[u8], limit: usize) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    zstd::stream::read::Decoder::new(bytes)?
        .take((limit + 1) as u64)
        .read_to_end(&mut output)?;
    ensure!(output.len() <= limit, "Decoded blob exceeds size limit");
    Ok(output)
}

/// Encode a sorted batch of full-page XORs against the golden image. Keeping
/// page indexes inside the blob catches incorrect slot references.
pub(crate) fn encode_pages(pages: &[(u32, Vec<u8>)]) -> Result<Vec<u8>> {
    ensure!(pages.len() <= MAX_MEMORY / PAGE, "Too many pages");
    let mut raw = Vec::with_capacity(8 + pages.len() * (PAGE + 4));
    raw.extend_from_slice(b"QPD1");
    raw.extend_from_slice(&(pages.len() as u32).to_le_bytes());
    for (i, (index, page)) in pages.iter().enumerate() {
        ensure!(
            page.len() == PAGE && *index < (MAX_MEMORY / PAGE) as u32,
            "Invalid page"
        );
        ensure!(
            i == 0 || pages[i - 1].0 < *index,
            "Pages must be sorted and unique"
        );
        raw.extend_from_slice(&index.to_le_bytes());
        raw.extend_from_slice(page);
    }
    compress(&raw)
}

pub(crate) fn decode_pages(bytes: &[u8]) -> Result<Vec<(u32, Vec<u8>)>> {
    let raw = decompress(bytes, MAX_RAW)?;
    ensure!(raw.len() >= 8 && &raw[..4] == b"QPD1", "Invalid page blob");
    let count = u32::from_le_bytes(raw[4..8].try_into()?) as usize;
    ensure!(
        count <= MAX_MEMORY / PAGE && raw.len() == 8 + count * (PAGE + 4),
        "Invalid page blob length"
    );
    let mut result = Vec::with_capacity(count);
    for chunk in raw[8..].as_chunks::<{ PAGE + 4 }>().0 {
        let index = u32::from_le_bytes(chunk[..4].try_into()?);
        ensure!(
            index < (MAX_MEMORY / PAGE) as u32
                && result.last().is_none_or(|(prev, _)| *prev < index),
            "Invalid page index"
        );
        result.push((index, chunk[4..].to_vec()));
    }
    Ok(result)
}
