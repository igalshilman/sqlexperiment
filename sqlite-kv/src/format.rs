//! Keys and magic numbers for the single current storage format.
//!
//! | Key | Whole value |
//! |---|---|
//! | `v14/meta` | `SM14` checkpoint record; see [`crate::log::Meta`] |
//! | `v14/log/<16 hex digits>` | Log segment by sequence number; see [`crate::log`] |
//! | `v14/page/<16 hex digits>` | Checkpointed page by page number; see [`crate::page_codec`] |
//! | `v14/hint` | `SH14` advisory list of pages a cold open fetches; see [`crate::hint`] |
//!
//! Log segments are the commit records. Pages are overwritten in place by
//! checkpoints and are only authoritative for pages absent from the log. The
//! namespace must be dedicated to one database.
pub(crate) const META: &str = "v14/meta";
pub(crate) const META_MAGIC: &[u8; 4] = b"SM14";
pub(crate) const HINT: &str = "v14/hint";
pub(crate) const HINT_MAGIC: &[u8; 4] = b"SH14";
pub(crate) const LOG_MAGIC: &[u8; 4] = b"SL14";
pub(crate) const LOG_PREFIX: &str = "v14/log/";
pub(crate) const PAGE_PREFIX: &str = "v14/page/";

pub(crate) fn log_key(seq: u64) -> String {
    format!("{LOG_PREFIX}{seq:016x}")
}

pub(crate) fn page_key(index: u64) -> String {
    format!("{PAGE_PREFIX}{index:016x}")
}
