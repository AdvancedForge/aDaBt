//! Settings the optimizer flips that carry no derived structure of their own.

use adabt_core::record::Record;
use std::collections::HashMap;

/// Per-collection clustering hints and the engine-wide feature switches.
pub(super) struct Tuning {
    /// Per-collection clustering hints, set with [`Database::declare_cluster_field`](super::Database::declare_cluster_field):
    /// the field whose integer value steers *where* a record is placed. A hint,
    /// not a constraint — see that method for exactly what is and is not promised.
    pub(super) cluster_fields: HashMap<String, String>,
    /// Whether delta-varint encoding is enabled for column stores.
    pub(super) delta_encoding: bool,
    /// Whether thread-per-core execution is enabled.
    pub(super) thread_per_core: bool,
    /// Whether join order optimization is enabled. Recorded and reported
    /// through `is_join_order`; nothing in planning or execution reads it.
    pub(super) join_order: bool,
    /// Whether data-driven partitioning is enabled. Recorded and reported
    /// through `is_data_partitioning`; the placement itself comes from the
    /// clustering field the same decision declares.
    pub(super) data_partitioning: bool,
}

impl Tuning {
    /// The integer key a record contributes to the clustering hint, or `None`
    /// when the collection has no hint or the value is not an integer.
    pub(super) fn cluster_key(&self, collection: &str, rec: &Record) -> Option<i64> {
        let field = self.cluster_fields.get(collection)?;
        match rec.get(field) {
            Some(adabt_core::value::Value::I64(v)) => Some(*v),
            Some(adabt_core::value::Value::U64(v)) => i64::try_from(*v).ok(),
            _ => None,
        }
    }
}
