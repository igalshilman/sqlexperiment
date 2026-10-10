//! Keys and magic numbers for the single current storage format.
//!
//! | Key | Whole value |
//! |---|---|
//! | `v10/head` | `SK10` snapshot head; see [`crate::snapshot`] |
//! | `v10/pending_save` | `SP10` publication intent; see [`crate::pending`] |
//! | `v10/node/<16 hex digits>` | Immutable radix node; see [`crate::map_codec`] |
//! | `v10/block/<16 hex digits>` | Immutable encoded page; see [`crate::page_codec`] |
//!
//! IDs come from one monotonic counter shared by nodes and blocks. Published IDs
//! are never reused; IDs of an aborted attempt are reused only after recovery has
//! deleted them. The namespace must be dedicated to one database.
pub(crate) const HEAD: &str = "v10/head";
pub(crate) const PENDING: &str = "v10/pending_save";
pub(crate) const HEAD_MAGIC: &[u8; 4] = b"SK10";
pub(crate) const PENDING_MAGIC: &[u8; 4] = b"SP10";

/// Immutable object identity, shared by map nodes and pages.
pub(crate) type Id = u64;

/// The two kinds of immutable objects. The discriminant is the pending-record tag.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum Kind {
    Block = 0,
    Node = 1,
}

impl Kind {
    pub(crate) const fn prefix(self) -> &'static str {
        match self {
            Kind::Block => "v10/block/",
            Kind::Node => "v10/node/",
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
