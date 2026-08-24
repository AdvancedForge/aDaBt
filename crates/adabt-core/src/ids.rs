//! Newtype identifiers.
//!
//! `RecordId` is deliberately a dense-able `u64`: the Level 10 `DirectLookup`
//! optimization computes `BASE + id * RECORD_SIZE` directly from it, so the
//! logical identifier and the physical address share a domain by design.

macro_rules! id_type {
    ($(#[$m:meta])* $name:ident, $inner:ty) => {
        $(#[$m])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
        pub struct $name(pub $inner);

        impl $name {
            #[inline]
            pub const fn new(v: $inner) -> Self { Self(v) }
            #[inline]
            pub const fn get(self) -> $inner { self.0 }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}", self.0)
            }
        }

        impl From<$inner> for $name {
            #[inline]
            fn from(v: $inner) -> Self { Self(v) }
        }
    };
}

id_type!(/// Stable logical identity of a record within a collection.
    RecordId, u64);
id_type!(/// Catalog identity of a collection.
    CollectionId, u32);
id_type!(/// Identity of one physical representation within a `RepresentationSet`.
    RepId, u32);
id_type!(/// Identity of a secondary index.
    IndexId, u32);
id_type!(/// Identity of a materialized view.
    ViewId, u32);
id_type!(/// A monotonic write/visibility stamp, assigned fresh to every physical
    /// write for MVCC ordering. Despite the name, this is not a multi-statement
    /// transaction's identity — see [`TransactionId`] for that — it predates
    /// multi-statement transactions and named the closest concept available at
    /// the time: "the write that is happening now."
    TxnId, u64);
id_type!(/// The identity of a multi-statement transaction, as the caller sees
    /// it — allocated once at `begin`, held for the transaction's lifetime, and
    /// never itself written to a version chain. Distinct from [`TxnId`], which
    /// stamps each individual physical write.
    TransactionId, u64);
id_type!(/// Write-ahead log sequence number.
    Lsn, u64);
