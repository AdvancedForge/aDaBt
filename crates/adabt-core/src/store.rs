//! The logical API contract.
//!
//! This trait is the user's stable surface. Everything below it — heap files,
//! column stores, caches, directly-addressed fixed arrays — may be replaced at
//! runtime without any signature here changing. That invariance is the whole
//! point of the project, so this trait is deliberately hard to extend: adding
//! to it is a decision about the *logical* database, never about optimization.
//!
//! Both the reference model and the real engine implement it, which is what
//! makes differential testing across optimization levels possible.

use crate::error::Result;
use crate::ids::RecordId;
use crate::record::Record;
use crate::schema::Schema;
use crate::value::Value;

/// Drop top-level fields whose value is null.
///
/// **An explicit null and an absent field are the same thing.** Every
/// implementation must apply this before storing, and the rule lives here
/// rather than in any one store because it is a statement about the logical
/// database, not about a layout.
///
/// The rule is uniform across all four schema modes on purpose. A bitmap-based
/// layout physically cannot tell `{"a": null}` from `{}` — presence is one bit
/// — while a tag-length-value layout easily can. Letting each mode follow its
/// own layout's grain would mean that freezing a `Dynamic` collection into
/// `Declared` silently changed what its data meant, which breaks the promise
/// the whole project rests on: the physical representation may change freely,
/// the logical one may not.
///
/// This applies only at the top level. Inside a list or map, null is an
/// ordinary value and is preserved: `[1, null, 3]` keeps its shape.
pub fn normalize_for_storage(rec: &mut Record) {
    let nulls: Vec<String> = rec
        .iter()
        .filter(|(_, v)| matches!(v, Value::Null))
        .map(|(k, _)| k.to_string())
        .collect();
    for k in nulls {
        rec.remove(&k);
    }
}

/// The logical API contract.
///
/// Reads take `&mut self`. A real engine mutates on read — the buffer pool
/// faults pages in, hit counters move, the eviction policy is touched — and a
/// `&self` signature would be a lie maintained by a lock that buys nothing
/// until there are threads to contend for it. Concurrent reads will arrive as a
/// snapshot handle obtained from the engine, not by pretending reads are pure.
pub trait LogicalStore {
    fn create_collection(&mut self, name: &str, schema: Schema) -> Result<()>;
    fn drop_collection(&mut self, name: &str) -> Result<()>;
    fn collection_names(&self) -> Vec<String>;
    fn schema_of(&self, collection: &str) -> Result<&Schema>;

    /// Insert, failing if `id` is already present.
    fn insert(&mut self, collection: &str, id: RecordId, rec: Record) -> Result<()>;

    fn get(&mut self, collection: &str, id: RecordId) -> Result<Option<Record>>;

    /// Replace an existing record. Returns whether it existed.
    fn update(&mut self, collection: &str, id: RecordId, rec: Record) -> Result<bool>;

    /// Returns whether the record existed.
    fn delete(&mut self, collection: &str, id: RecordId) -> Result<bool>;

    /// Full scan in ascending `RecordId` order.
    ///
    /// Order is part of the contract: differential testing compares scan output
    /// directly, and an engine that returned physical order would diverge from
    /// the reference model for reasons that are not bugs.
    fn scan(&mut self, collection: &str) -> Result<Vec<(RecordId, Record)>>;

    fn count(&mut self, collection: &str) -> Result<usize>;

    /// The ids of every live record, ascending — without reading the records.
    ///
    /// The executor needs ids alone to drive a scan: it sorts them, then
    /// fetches each one. Serving that from `scan` means decoding the entire
    /// collection and throwing every record away, so a full scan decodes the
    /// collection twice and pays the second decode for nothing.
    ///
    /// The default is the honest one for a store that cannot do better: same
    /// ids, same order, same cost as before. Overriding it is an optimization,
    /// and the contract it must keep is that the ids are exactly `scan`'s.
    fn ids(&mut self, collection: &str) -> Result<Vec<RecordId>> {
        Ok(self
            .scan(collection)?
            .into_iter()
            .map(|(id, _)| id)
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn top_level_nulls_are_dropped() {
        let mut r = Record::new()
            .with("keep", 1i64)
            .with("gone", Value::Null)
            .with("also_gone", Value::Null);
        normalize_for_storage(&mut r);
        assert_eq!(r.len(), 1);
        assert_eq!(r.get("keep"), Some(&Value::I64(1)));
        assert_eq!(r.get("gone"), None);
    }

    #[test]
    fn an_explicit_null_becomes_indistinguishable_from_omission() {
        let mut explicit = Record::new().with("a", 1i64).with("b", Value::Null);
        let mut omitted = Record::new().with("a", 1i64);
        normalize_for_storage(&mut explicit);
        normalize_for_storage(&mut omitted);
        assert_eq!(explicit, omitted);
    }

    #[test]
    fn nested_nulls_are_preserved() {
        // Inside a list or map, null is an ordinary value: dropping it would
        // change the shape of the data, not merely its presence.
        let mut m = BTreeMap::new();
        m.insert("inner".to_string(), Value::Null);
        let mut r = Record::new()
            .with(
                "list",
                Value::List(vec![Value::I64(1), Value::Null, Value::I64(3)]),
            )
            .with("map", Value::Map(m));
        normalize_for_storage(&mut r);
        assert_eq!(r.len(), 2, "nested nulls must not remove their container");
        match r.get("list") {
            Some(Value::List(items)) => {
                assert_eq!(items.len(), 3, "list shape changed");
                assert_eq!(items[1], Value::Null);
            }
            other => panic!("expected a list, got {other:?}"),
        }
        match r.get("map") {
            Some(Value::Map(m)) => assert_eq!(m.get("inner"), Some(&Value::Null)),
            other => panic!("expected a map, got {other:?}"),
        }
    }

    #[test]
    fn normalizing_is_idempotent() {
        let mut r = Record::new().with("a", Value::Null).with("b", 2i64);
        normalize_for_storage(&mut r);
        let once = r.clone();
        normalize_for_storage(&mut r);
        assert_eq!(r, once);
    }

    #[test]
    fn a_record_with_no_nulls_is_untouched() {
        let orig = Record::new().with("a", 1i64).with("b", "x");
        let mut r = orig.clone();
        normalize_for_storage(&mut r);
        assert_eq!(r, orig);
    }
}
