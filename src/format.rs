//! Names and markers for the single current storage format.
//!
//! | Key | Whole value |
//! |---|---|
//! | `v10/head` | `SK10`, size u64 LE, high-water ID u64 LE, height u8, encoded root |
//! | `v10/pending_save` | `SP10` publication intent; see [`crate::pending`] |
//! | `v10/node/<16 hex digits>` | Immutable radix node; see [`crate::map_codec`] |
//! | `v10/block/<16 hex digits>` | Immutable encoded page; see [`crate::page_codec`] |
//!
//! IDs are monotonically allocated names, not content hashes. The committed counter
//! prevents reuse of published IDs; recovery can reuse abandoned IDs only after
//! cleaning the complete created set. Node and page IDs share the counter. Fixed-width
//! hex keeps object keys predictable for measurement and backend naming.
//!
//! There is no scan, migration, alternate decoder or probing of earlier prefixes.
//! The backend is scoped to one dedicated namespace; the version prefix alone is not
//! an ownership or tenant-isolation mechanism.
pub(crate) const HEAD: &str = "v10/head";
pub(crate) const PENDING: &str = "v10/pending_save";
pub(crate) const HEAD_MAGIC: &[u8; 4] = b"SK10";
pub(crate) const PENDING_MAGIC: &[u8; 4] = b"SP10";

pub(crate) fn key(kind: &str, id: &u64) -> String {
    format!("v10/{kind}/{id:016x}")
}
