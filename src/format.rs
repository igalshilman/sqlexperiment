//! Keys and magic numbers for the single current storage format.
//!
//! | Key | Whole value |
//! |---|---|
//! | `v13/meta` | `SM13` checkpoint record; see [`crate::log::Meta`] |
//! | `v13/log/<16 hex digits>` | Log segment by sequence number; see [`crate::log`] |
//! | `v13/page/<16 hex digits>` | Checkpointed page by page number; see [`crate::page_codec`] |
//! | `v13/hint` | `SH13` advisory list of pages a cold open fetches; see [`crate::hint`] |
//!
//! Every value ends in a CRC32C (Castagnoli) checksum, u32 LE, except pages,
//! which carry it in their envelope; see [`seal`] and [`unseal`].
//!
//! Log segments are the commit records. Pages are overwritten in place by
//! checkpoints and are only authoritative for pages absent from the log. The
//! namespace must be dedicated to one database.
pub(crate) const META: &str = "v13/meta";
pub(crate) const META_MAGIC: &[u8; 4] = b"SM13";
pub(crate) const HINT: &str = "v13/hint";
pub(crate) const HINT_MAGIC: &[u8; 4] = b"SH13";
pub(crate) const LOG_MAGIC: &[u8; 4] = b"SL13";
pub(crate) const LOG_PREFIX: &str = "v13/log/";
pub(crate) const PAGE_PREFIX: &str = "v13/page/";

pub(crate) fn log_key(seq: u64) -> String {
    format!("{LOG_PREFIX}{seq:016x}")
}

pub(crate) fn page_key(index: u64) -> String {
    format!("{PAGE_PREFIX}{index:016x}")
}

/// Bytes of a checksum.
pub(crate) const CHECKSUM: usize = 4;

/// The CRC32C checksum of `bytes`.
pub(crate) fn checksum(bytes: &[u8]) -> u32 {
    crc32c::crc32c(bytes)
}

/// Append the checksum of everything written so far.
pub(crate) fn seal(mut bytes: Vec<u8>) -> Vec<u8> {
    let sum = checksum(&bytes);
    bytes.extend_from_slice(&sum.to_le_bytes());
    bytes
}

/// The content of a sealed value, if its trailing checksum matches.
pub(crate) fn unseal(bytes: &[u8]) -> Option<&[u8]> {
    let (content, sum) = bytes.split_at_checked(bytes.len().checked_sub(CHECKSUM)?)?;
    (checksum(content).to_le_bytes() == sum).then_some(content)
}
