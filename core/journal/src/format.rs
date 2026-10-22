//! Keys and magic numbers for the single current storage format.
//!
//! | Key | Whole value |
//! |---|---|
//! | `v15/commit` | `SC15` commit record; see [`crate::log::Commit`] |
//! | `v15/log/<16 hex digits>` | `SL15` stored log segment by number; see [`crate::log`] |
//! | `v15/page/<8 hex digits>/<16 hex digits>` | Checkpointed page by file id and page number; see [`crate::page_codec`] |
//! | `v15/hint` | `SH15` advisory list of pages a cold open fetches; see [`crate::hint`] |
//!
//! The commit record is the only key whose write commits. It holds the
//! directory of files (records and paged files) and the log since the last
//! checkpoint, inline or naming stored segments. Pages are overwritten in
//! place by checkpoints and are only authoritative for pages absent from the
//! log. The namespace must be dedicated to one journal.
pub(crate) const COMMIT: &str = "v15/commit";
pub(crate) const COMMIT_MAGIC: &[u8; 4] = b"SC15";
pub(crate) const HINT: &str = "v15/hint";
pub(crate) const HINT_MAGIC: &[u8; 4] = b"SH15";
pub(crate) const LOG_MAGIC: &[u8; 4] = b"SL15";
pub(crate) const LOG_PREFIX: &str = "v15/log/";
pub(crate) const PAGE_PREFIX: &str = "v15/page/";

pub(crate) fn log_key(n: u64) -> String {
    format!("{LOG_PREFIX}{n:016x}")
}

/// The key of the page `id` (see [`crate::log::page_id`]).
pub(crate) fn page_key(id: u64) -> String {
    let (file, page) = (crate::log::file_of(id), crate::log::page_of(id));
    format!("{PAGE_PREFIX}{file:08x}/{page:016x}")
}

/// The page id a page key names.
#[cfg(feature = "simulation")]
pub(crate) fn parse_page_key(key: &str) -> Option<u64> {
    let (file, page) = key.strip_prefix(PAGE_PREFIX)?.split_once('/')?;
    let file = u32::from_str_radix(file, 16).ok()?;
    let page = u64::from_str_radix(page, 16)
        .ok()
        .filter(|&page| page < 1 << 32)?;
    Some(crate::log::page_id(file, page))
}
