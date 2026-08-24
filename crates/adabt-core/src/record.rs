//! The logical record.
//!
//! Fields are kept sorted by name, and iteration order is that order — the
//! differential test rig compares whole records, and an order that depended on
//! how a record was built would make failures irreproducible across runs.
//!
//! # Why a sorted `Vec` and not a `BTreeMap`
//!
//! A record has a handful of fields, not thousands, and it is built once per
//! row read. Those two facts make a tree the wrong shape:
//!
//! - A `BTreeMap` allocates a node per record. A `Vec` allocates once and, at
//!   these sizes, binary search over contiguous memory beats pointer-chasing.
//! - Field names are `Arc<str>` rather than `String`. In every schema mode but
//!   `Dynamic` the names come from the schema and are identical for every row
//!   in the collection, so the decoder keeps one `Arc` per field and clones a
//!   refcount instead of allocating a fresh `String` per field per row.
//!
//! Measured on a three-field record: those two changes are three of the five
//! heap allocations a scanned row used to cost. See
//! `adabt-engine/tests/allocations.rs`, which asserts the budget so it cannot
//! quietly grow back.
//!
//! The ordering, equality and iteration contracts are unchanged. `Ord` on a
//! sorted `Vec<(Arc<str>, Value)>` compares element-wise over the same
//! sequence a `BTreeMap`'s iterator would yield, so records compare exactly as
//! before.

use crate::value::Value;
use std::sync::Arc;

#[derive(Clone, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Record {
    /// Sorted by name, and always kept that way. Every method that inserts
    /// goes through `slot`, so the invariant has one place to hold.
    fields: Vec<(Arc<str>, Value)>,
}

impl Record {
    pub fn new() -> Self {
        Self::default()
    }

    /// Where `name` is, or where it would go.
    fn slot(&self, name: &str) -> std::result::Result<usize, usize> {
        self.fields.binary_search_by(|(k, _)| (**k).cmp(name))
    }

    pub fn set(&mut self, name: impl Into<String>, v: impl Into<Value>) -> &mut Self {
        let name = name.into();
        match self.slot(&name) {
            Ok(i) => self.fields[i].1 = v.into(),
            Err(i) => self.fields.insert(i, (Arc::from(name), v.into())),
        }
        self
    }

    /// Set a field whose name is already shared.
    ///
    /// The decoder's path: schema field names live for the life of the
    /// collection, so cloning the `Arc` costs a refcount bump rather than a
    /// string allocation. Identical in behaviour to [`Record::set`] — the only
    /// difference is who owns the name.
    pub fn set_shared(&mut self, name: Arc<str>, v: Value) -> &mut Self {
        match self.slot(&name) {
            Ok(i) => self.fields[i].1 = v,
            Err(i) => self.fields.insert(i, (name, v)),
        }
        self
    }

    /// Reserve room for `n` fields, so building a record allocates once.
    pub fn reserve(&mut self, n: usize) {
        self.fields.reserve(n);
    }

    pub fn with(mut self, name: impl Into<String>, v: impl Into<Value>) -> Self {
        self.set(name, v);
        self
    }

    pub fn get(&self, name: &str) -> Option<&Value> {
        self.slot(name).ok().map(|i| &self.fields[i].1)
    }

    pub fn remove(&mut self, name: &str) -> Option<Value> {
        self.slot(name).ok().map(|i| self.fields.remove(i).1)
    }

    pub fn field_names(&self) -> impl Iterator<Item = &str> {
        self.fields.iter().map(|(k, _)| &**k)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &Value)> {
        self.fields.iter().map(|(k, v)| (&**k, v))
    }

    pub fn len(&self) -> usize {
        self.fields.len()
    }

    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }

    /// Rough in-memory footprint, in bytes. See `Value::approx_size` — the
    /// same "close enough for a circuit breaker" contract applies here.
    pub fn approx_size(&self) -> usize {
        self.fields
            .iter()
            .map(|(k, v)| k.len() + v.approx_size())
            .sum()
    }

    /// Keep only the named fields. Used by projection pushdown, and by the
    /// column-store representation when serving a partial read.
    pub fn project(&self, names: &[&str]) -> Record {
        let mut out = Record::new();
        out.reserve(names.len());
        for n in names {
            // Clone the shared name rather than re-allocating it: a projection
            // of a decoded row keeps the schema's names.
            if let Ok(i) = self.slot(n) {
                let (k, v) = &self.fields[i];
                out.set_shared(Arc::clone(k), v.clone());
            }
        }
        out
    }
}

/// Rendered like a map, because that is what it is. A derived `Vec` debug
/// would change every existing failure message for no reason.
impl std::fmt::Debug for Record {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}

impl FromIterator<(String, Value)> for Record {
    fn from_iter<T: IntoIterator<Item = (String, Value)>>(iter: T) -> Self {
        let mut r = Record::new();
        for (k, v) in iter {
            r.set(k, v);
        }
        r
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn projection_keeps_only_named_fields_and_ignores_absent_ones() {
        let r = Record::new()
            .with("a", 1i64)
            .with("b", 2i64)
            .with("c", 3i64);
        let p = r.project(&["a", "c", "missing"]);
        assert_eq!(p.len(), 2);
        assert_eq!(p.get("a"), Some(&Value::I64(1)));
        assert_eq!(p.get("c"), Some(&Value::I64(3)));
        assert_eq!(p.get("b"), None);
    }

    #[test]
    fn field_order_is_deterministic_regardless_of_insertion_order() {
        let a = Record::new().with("z", 1i64).with("a", 2i64);
        let b = Record::new().with("a", 2i64).with("z", 1i64);
        assert_eq!(a, b);
        assert_eq!(a.field_names().collect::<Vec<_>>(), vec!["a", "z"]);
    }

    /// The sorted-`Vec` invariant, stated directly. Every other guarantee here
    /// — ordering, equality, `get` — rests on it, and it is maintained by hand
    /// rather than by a container, so it gets its own test.
    #[test]
    fn fields_stay_sorted_through_any_sequence_of_writes() {
        let mut r = Record::new();
        for name in ["m", "c", "z", "a", "q", "c", "b"] {
            r.set(name, 1i64);
        }
        r.remove("q");
        r.set_shared(Arc::from("d"), Value::I64(9));
        let names: Vec<&str> = r.field_names().collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted, "field order must always be sorted");
        assert_eq!(names, vec!["a", "b", "c", "d", "m", "z"]);
    }

    /// Setting an existing field replaces it rather than duplicating it —
    /// the behaviour a map gave for free and a `Vec` does not.
    #[test]
    fn setting_a_field_twice_replaces_it() {
        let mut r = Record::new().with("a", 1i64);
        r.set("a", 2i64);
        r.set_shared(Arc::from("a"), Value::I64(3));
        assert_eq!(r.len(), 1);
        assert_eq!(r.get("a"), Some(&Value::I64(3)));
    }

    /// `set` and `set_shared` must be indistinguishable from the outside; the
    /// only difference is who owns the name.
    #[test]
    fn shared_and_owned_names_produce_equal_records() {
        let mut owned = Record::new();
        owned.set("b", 2i64).set("a", 1i64);
        let mut shared = Record::new();
        shared.set_shared(Arc::from("b"), Value::I64(2));
        shared.set_shared(Arc::from("a"), Value::I64(1));
        assert_eq!(owned, shared);
        assert_eq!(
            format!("{owned:?}"),
            format!("{shared:?}"),
            "debug output must not reveal which was used"
        );
    }

    /// Ordering is compared across whole records by the differential rig, so
    /// it has to agree with what a sorted map would have produced.
    #[test]
    fn records_order_by_field_name_then_value() {
        let a = Record::new().with("a", 1i64);
        let b = Record::new().with("a", 2i64);
        let c = Record::new().with("b", 0i64);
        assert!(a < b, "same name, lower value sorts first");
        assert!(b < c, "earlier name sorts first regardless of value");
    }

    /// A later duplicate wins, which is what collecting into a map did.
    #[test]
    fn collecting_duplicate_names_keeps_the_last() {
        let r: Record = vec![
            ("a".to_string(), Value::I64(1)),
            ("b".to_string(), Value::I64(2)),
            ("a".to_string(), Value::I64(3)),
        ]
        .into_iter()
        .collect();
        assert_eq!(r.len(), 2);
        assert_eq!(r.get("a"), Some(&Value::I64(3)));
    }
}
