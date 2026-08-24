//! The logical record.
//!
//! Fields are held in a `BTreeMap` so iteration order is deterministic — the
//! differential test rig compares whole records, and a hash-ordered map would
//! make failures irreproducible across runs.

use crate::value::Value;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Record {
    fields: BTreeMap<String, Value>,
}

impl Record {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(&mut self, name: impl Into<String>, v: impl Into<Value>) -> &mut Self {
        self.fields.insert(name.into(), v.into());
        self
    }

    pub fn with(mut self, name: impl Into<String>, v: impl Into<Value>) -> Self {
        self.set(name, v);
        self
    }

    pub fn get(&self, name: &str) -> Option<&Value> {
        self.fields.get(name)
    }

    pub fn remove(&mut self, name: &str) -> Option<Value> {
        self.fields.remove(name)
    }

    pub fn field_names(&self) -> impl Iterator<Item = &str> {
        self.fields.keys().map(|s| s.as_str())
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &Value)> {
        self.fields.iter().map(|(k, v)| (k.as_str(), v))
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
        for n in names {
            if let Some(v) = self.fields.get(*n) {
                out.fields.insert((*n).to_string(), v.clone());
            }
        }
        out
    }
}

impl FromIterator<(String, Value)> for Record {
    fn from_iter<T: IntoIterator<Item = (String, Value)>>(iter: T) -> Self {
        Record {
            fields: iter.into_iter().collect(),
        }
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
}
