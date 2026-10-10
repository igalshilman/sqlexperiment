//! Keys and magic numbers for the single current storage format.
//!
//! | Key | Whole value |
//! |---|---|
//! | `v12/meta` | `SM12` checkpoint record; see [`crate::log::Meta`] |
//! | `v12/log/<16 hex digits>` | Log segment by sequence number; see [`crate::log`] |
//! | `v12/page/<16 hex digits>` | Checkpointed page by page number; see [`crate::page_codec`] |
//!
//! Log segments are the commit records. Pages are overwritten in place by
//! checkpoints and are only authoritative for pages absent from the log. The
//! namespace must be dedicated to one database.
pub(crate) const META: &str = "v12/meta";
pub(crate) const META_MAGIC: &[u8; 4] = b"SM12";
pub(crate) const LOG_MAGIC: &[u8; 4] = b"SL12";
pub(crate) const LOG_PREFIX: &str = "v12/log/";
pub(crate) const PAGE_PREFIX: &str = "v12/page/";

pub(crate) fn log_key(seq: u64) -> String {
    format!("{LOG_PREFIX}{seq:016x}")
}

pub(crate) fn page_key(index: u64) -> String {
    format!("{PAGE_PREFIX}{index:016x}")
}
