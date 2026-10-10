//! Keys and magic numbers for the single current storage format.
//!
//! | Key | Whole value |
//! |---|---|
//! | `v11/head` | `SK11` snapshot head; see [`crate::snapshot`] |
//! | `v11/pending_save` | `SP11` publication intent; see [`crate::pending`] |
//! | `v11/node/<16 hex digits>` | Immutable radix node; see [`crate::map_codec`] |
//! | `v11/block/<16 hex digits>` | Immutable encoded page; see [`crate::page_codec`] |
//! | `v11/log/<16 hex digits>` | Delta-log segment by sequence number; see [`crate::log`] |
//!
//! IDs come from one monotonic counter shared by nodes and blocks. Published IDs
//! are never reused; IDs of an aborted attempt are reused only after recovery has
//! deleted them. The namespace must be dedicated to one database.
pub(crate) const HEAD: &str = "v11/head";
pub(crate) const PENDING: &str = "v11/pending_save";
pub(crate) const HEAD_MAGIC: &[u8; 4] = b"SK11";
pub(crate) const PENDING_MAGIC: &[u8; 4] = b"SP11";

/// Immutable object identity, shared by map nodes and pages.
pub(crate) type Id = u64;

/// Kinds of immutable objects. The discriminant is the pending-record tag.
/// Blocks and nodes are named by allocated IDs; log segments by sequence number.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum Kind {
    Block = 0,
    Node = 1,
    Log = 2,
}

impl Kind {
    pub(crate) const fn prefix(self) -> &'static str {
        match self {
            Kind::Block => "v11/block/",
            Kind::Node => "v11/node/",
            Kind::Log => "v11/log/",
        }
    }

    /// Children at radix level one are pages; above that they are nodes.
    pub(crate) fn of_child(level: u8) -> Self {
        if level == 1 { Kind::Block } else { Kind::Node }
    }

    pub(crate) fn key(self, id: Id) -> String {
        format!("{}{id:016x}", self.prefix())
    }
}
