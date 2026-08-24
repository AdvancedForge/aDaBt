//! Index kinds.
//!
//! Lives in `core` rather than in the index crate because the optimizer must be
//! able to *name* an index kind without depending on the code that implements
//! one. That constraint is deliberate: `adabt-opt` decides what should exist,
//! and something else builds it.

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum IndexKind {
    Hash,
    BTree,
    /// One bit per record id, per distinct value. Answers equality the same
    /// way a hash index does — same `lookup`, same semantics, nothing about
    /// what it returns differs — but stores presence rather than a list, so
    /// it is worth reaching for on a field with few distinct values and many
    /// rows per value, where `Hash`'s per-key `Vec<RecordId>` costs roughly a
    /// pointer's worth of overhead per entry that a bitmap does not pay.
    /// Never chosen automatically today: `adabt-opt`'s `auto_index` still
    /// only ever proposes `Hash` or `BTree` (see `index_kind_for` in
    /// `adabt-engine`'s optimization library) — this is reachable only by
    /// naming it explicitly, through `Database::create_index` or a manual
    /// policy override, until that heuristic is taught the low-cardinality
    /// signal `OptContext::density_of` already makes available.
    Bitmap,
}

impl IndexKind {
    pub fn as_str(self) -> &'static str {
        match self {
            IndexKind::Hash => "hash",
            IndexKind::BTree => "btree",
            IndexKind::Bitmap => "bitmap",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "hash" => Some(IndexKind::Hash),
            "btree" => Some(IndexKind::BTree),
            "bitmap" => Some(IndexKind::Bitmap),
            _ => None,
        }
    }

    /// A small integer form, for a caller that can only carry a number — an
    /// `adabt-opt` `Params` map, whose values are `i64`, has no other way to
    /// name one. `as_str`/`parse` stay the primary, human-facing pair; this
    /// exists only where a machine-readable slot forces the choice.
    pub fn as_ordinal(self) -> i64 {
        match self {
            IndexKind::Hash => 0,
            IndexKind::BTree => 1,
            IndexKind::Bitmap => 2,
        }
    }

    pub fn from_ordinal(n: i64) -> Option<Self> {
        match n {
            0 => Some(IndexKind::Hash),
            1 => Some(IndexKind::BTree),
            2 => Some(IndexKind::Bitmap),
            _ => None,
        }
    }

    /// Only an ordered structure can answer a range.
    ///
    /// A bitmap could in principle answer one too — union the bitmaps of
    /// every key in range — but that needs the keys visited in sorted order,
    /// which a `HashMap`-backed bitmap index does not keep and a hash index
    /// does not either; both decline for the same reason `Hash` already
    /// does, not a new one this variant invents.
    pub fn supports_range(self) -> bool {
        self == IndexKind::BTree
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [IndexKind; 3] = [IndexKind::Hash, IndexKind::BTree, IndexKind::Bitmap];

    #[test]
    fn names_round_trip() {
        for k in ALL {
            assert_eq!(IndexKind::parse(k.as_str()), Some(k));
        }
        assert_eq!(IndexKind::parse("nope"), None);
    }

    #[test]
    fn only_btree_answers_ranges() {
        assert!(IndexKind::BTree.supports_range());
        assert!(!IndexKind::Hash.supports_range());
        assert!(!IndexKind::Bitmap.supports_range());
    }

    #[test]
    fn ordinals_round_trip() {
        for k in ALL {
            assert_eq!(IndexKind::from_ordinal(k.as_ordinal()), Some(k));
        }
        assert_eq!(IndexKind::from_ordinal(99), None);
    }
}
