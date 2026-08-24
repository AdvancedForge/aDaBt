//! The reference implementation of `LogicalStore`.
//!
//! Deliberately the most boring code in the repository: `BTreeMap` all the way
//! down, no caching, no indexes, no cleverness. Its only job is to be so
//! obviously correct that a disagreement between it and the engine is always
//! the engine's fault.
//!
//! Every optimization level is validated by running the same operation sequence
//! against this and against the engine and demanding identical results. That is
//! what turns "optimizations must not change logical semantics" from a stated
//! principle into an enforced one.

use adabt_core::error::{Error, Result};
use adabt_core::ids::RecordId;
use adabt_core::record::Record;
use adabt_core::schema::Schema;
use adabt_core::store::{normalize_for_storage, LogicalStore};
use std::collections::BTreeMap;

struct Collection {
    schema: Schema,
    records: BTreeMap<RecordId, Record>,
}

#[derive(Default)]
pub struct ReferenceStore {
    collections: BTreeMap<String, Collection>,
}

impl ReferenceStore {
    pub fn new() -> Self {
        Self::default()
    }

    fn coll(&self, name: &str) -> Result<&Collection> {
        self.collections
            .get(name)
            .ok_or_else(|| Error::NoSuchCollection(name.to_string()))
    }

    fn coll_mut(&mut self, name: &str) -> Result<&mut Collection> {
        self.collections
            .get_mut(name)
            .ok_or_else(|| Error::NoSuchCollection(name.to_string()))
    }
}

impl LogicalStore for ReferenceStore {
    fn create_collection(&mut self, name: &str, schema: Schema) -> Result<()> {
        if self.collections.contains_key(name) {
            return Err(Error::CollectionExists(name.to_string()));
        }
        self.collections.insert(
            name.to_string(),
            Collection {
                schema,
                records: BTreeMap::new(),
            },
        );
        Ok(())
    }

    fn drop_collection(&mut self, name: &str) -> Result<()> {
        self.collections
            .remove(name)
            .map(|_| ())
            .ok_or_else(|| Error::NoSuchCollection(name.to_string()))
    }

    fn collection_names(&self) -> Vec<String> {
        self.collections.keys().cloned().collect()
    }

    fn schema_of(&self, collection: &str) -> Result<&Schema> {
        Ok(&self.coll(collection)?.schema)
    }

    fn insert(&mut self, collection: &str, id: RecordId, mut rec: Record) -> Result<()> {
        normalize_for_storage(&mut rec);
        let c = self.coll_mut(collection)?;
        // Validate before the existence check so that a schema-invalid insert
        // reports the schema problem regardless of whether the id is taken.
        c.schema.validate_record(&rec)?;
        if c.records.contains_key(&id) {
            return Err(Error::RecordExists(id));
        }
        c.records.insert(id, rec);
        Ok(())
    }

    fn get(&mut self, collection: &str, id: RecordId) -> Result<Option<Record>> {
        Ok(self.coll(collection)?.records.get(&id).cloned())
    }

    fn update(&mut self, collection: &str, id: RecordId, mut rec: Record) -> Result<bool> {
        normalize_for_storage(&mut rec);
        let c = self.coll_mut(collection)?;
        c.schema.validate_record(&rec)?;
        Ok(c.records.insert(id, rec).is_some())
    }

    fn delete(&mut self, collection: &str, id: RecordId) -> Result<bool> {
        Ok(self.coll_mut(collection)?.records.remove(&id).is_some())
    }

    fn scan(&mut self, collection: &str) -> Result<Vec<(RecordId, Record)>> {
        Ok(self
            .coll(collection)?
            .records
            .iter()
            .map(|(k, v)| (*k, v.clone()))
            .collect())
    }

    fn count(&mut self, collection: &str) -> Result<usize> {
        Ok(self.coll(collection)?.records.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use adabt_core::schema::{FieldDef, FieldType, SchemaMode};
    use adabt_core::value::Value;

    fn store() -> ReferenceStore {
        let mut s = ReferenceStore::new();
        s.create_collection(
            "users",
            Schema::new(SchemaMode::Strict, vec![FieldDef::new("n", FieldType::I64)]).unwrap(),
        )
        .unwrap();
        s
    }

    fn rec(n: i64) -> Record {
        Record::new().with("n", n)
    }

    #[test]
    fn insert_then_get_round_trips() {
        let mut s = store();
        s.insert("users", RecordId(1), rec(7)).unwrap();
        assert_eq!(s.get("users", RecordId(1)).unwrap(), Some(rec(7)));
        assert_eq!(s.get("users", RecordId(2)).unwrap(), None);
    }

    #[test]
    fn duplicate_insert_is_rejected() {
        let mut s = store();
        s.insert("users", RecordId(1), rec(1)).unwrap();
        assert!(matches!(
            s.insert("users", RecordId(1), rec(2)),
            Err(Error::RecordExists(RecordId(1)))
        ));
    }

    #[test]
    fn schema_is_checked_before_duplicate_detection() {
        let mut s = store();
        s.insert("users", RecordId(1), rec(1)).unwrap();
        let bad = Record::new().with("n", Value::Str("no".into()));
        assert!(matches!(
            s.insert("users", RecordId(1), bad),
            Err(Error::Schema(_))
        ));
    }

    #[test]
    fn update_reports_whether_the_record_existed() {
        let mut s = store();
        assert!(!s.update("users", RecordId(1), rec(1)).unwrap());
        assert!(s.update("users", RecordId(1), rec(2)).unwrap());
        assert_eq!(s.get("users", RecordId(1)).unwrap(), Some(rec(2)));
    }

    #[test]
    fn delete_reports_whether_the_record_existed() {
        let mut s = store();
        s.insert("users", RecordId(1), rec(1)).unwrap();
        assert!(s.delete("users", RecordId(1)).unwrap());
        assert!(!s.delete("users", RecordId(1)).unwrap());
    }

    #[test]
    fn scan_is_ordered_by_record_id() {
        let mut s = store();
        for id in [5u64, 1, 3, 2, 4] {
            s.insert("users", RecordId(id), rec(id as i64)).unwrap();
        }
        let ids: Vec<u64> = s.scan("users").unwrap().iter().map(|(i, _)| i.0).collect();
        assert_eq!(ids, vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn operations_on_a_missing_collection_fail() {
        let mut s = store();
        assert!(s.get("ghost", RecordId(1)).is_err());
        assert!(s.insert("ghost", RecordId(1), rec(1)).is_err());
        assert!(s.drop_collection("ghost").is_err());
    }

    #[test]
    fn dropping_a_collection_removes_its_records() {
        let mut s = store();
        s.insert("users", RecordId(1), rec(1)).unwrap();
        s.drop_collection("users").unwrap();
        assert!(s.collection_names().is_empty());
        assert!(s.get("users", RecordId(1)).is_err());
    }
}
