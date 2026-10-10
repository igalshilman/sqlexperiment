//! The single supported storage format. No migration or compatibility paths.
pub(crate) const HEAD: &str = "v9/head";
pub(crate) const PENDING: &str = "v9/pending_save";
pub(crate) const HEAD_MAGIC: &[u8; 4] = b"TKV9";
pub(crate) const PENDING_MAGIC: &[u8; 4] = b"TKP9";

pub(crate) fn key(kind: &str, id: &u64) -> String {
    format!("v9/{kind}/{id:016x}")
}
