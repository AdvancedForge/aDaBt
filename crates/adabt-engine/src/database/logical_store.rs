//! `LogicalStore` for `Database`: collection management and single-record
//! reads and writes, keeping derived structures in step.

use adabt_core::error::Result;
use adabt_core::ids::RecordId;
use adabt_core::record::Record;
use adabt_core::schema::{Schema, SchemaMode};
use adabt_core::store::LogicalStore;
use adabt_telemetry::event::OpKind;
use std::time::Instant;

use crate::direct::DirectArray;

use super::Database;

impl LogicalStore for Database {
    fn create_collection(&mut self, name: &str, schema: Schema) -> Result<()> {
        let fixed = schema.mode() == SchemaMode::Fixed;
        self.store.create_collection(name, schema.clone())?;
        // A collection created while direct lookup is on must get an array too,
        // or it would silently miss the optimization it was promised.
        if fixed && self.config().is_enabled_anywhere("direct_lookup") {
            if let Some(arr) = DirectArray::new(schema) {
                self.direct.insert(name.to_string(), arr);
            }
        }
        Ok(())
    }

    fn drop_collection(&mut self, name: &str) -> Result<()> {
        self.store.drop_collection(name)?;
        self.indexes.remove(name);
        self.direct.remove(name);
        self.bump_epoch(name);
        self.plan_cache.clear();
        self.compiled.clear();
        Ok(())
    }

    fn collection_names(&self) -> Vec<String> {
        self.store.collection_names()
    }

    fn schema_of(&self, collection: &str) -> Result<&Schema> {
        self.store.schema_of(collection)
    }

    fn insert(&mut self, collection: &str, id: RecordId, rec: Record) -> Result<()> {
        let t = Instant::now();
        let mut stored = rec.clone();
        adabt_core::store::normalize_for_storage(&mut stored);
        self.check_unique_constraints(collection, None, &stored)?;
        let key = self
            .cluster_fields
            .get(collection)
            .and_then(|f| Self::cluster_key(collection, f, &rec));
        match key {
            Some(k) => self.store.insert_keyed(collection, id, rec, k)?,
            None => self.store.insert(collection, id, rec)?,
        }
        self.reindex_insert(collection, id, &stored);
        self.bump_epoch(collection);
        self.observe(collection, OpKind::Insert, t, 1);
        Ok(())
    }

    fn get(&mut self, collection: &str, id: RecordId) -> Result<Option<Record>> {
        let t = Instant::now();
        let r = if let Some(d) = self.direct.get(collection) {
            d.get(id)?
        } else {
            self.store.get(collection, id)?
        };
        self.note_touch(collection, id);
        self.observe(collection, OpKind::Get, t, r.is_some() as u64);
        Ok(r)
    }

    fn update(&mut self, collection: &str, id: RecordId, rec: Record) -> Result<bool> {
        let t = Instant::now();
        // The old record is needed to un-index it. This read is the standing
        // cost of maintaining any index, and it is why an index nobody queries
        // is a pure loss rather than merely a neutral one.
        let old = if self.needs_old_record(collection) {
            self.store.get(collection, id)?
        } else {
            None
        };
        let mut stored = rec.clone();
        adabt_core::store::normalize_for_storage(&mut stored);
        self.check_unique_constraints(collection, Some(id), &stored)?;
        let existed = self.store.update(collection, id, rec)?;
        if let Some(old) = old {
            self.reindex_remove(collection, id, &old);
        }
        self.reindex_insert(collection, id, &stored);
        self.bump_epoch(collection);
        self.observe(collection, OpKind::Update, t, existed as u64);
        Ok(existed)
    }

    fn delete(&mut self, collection: &str, id: RecordId) -> Result<bool> {
        let t = Instant::now();
        let old = if self.needs_old_record(collection) {
            self.store.get(collection, id)?
        } else {
            None
        };
        let existed = self.store.delete(collection, id)?;
        match old {
            Some(old) => self.reindex_remove(collection, id, &old),
            // Not every derived structure needs to be told *what* was removed.
            // A direct array and a column store are keyed by identity alone, so
            // an id is enough — and they must still be told, or a database with
            // a column store and no index goes on counting rows that are gone.
            None => {
                if let Some(d) = self.direct.get_mut(collection) {
                    d.remove(id);
                }
                if let Some(c) = self.columns.get_mut(collection) {
                    c.mark_dead(id);
                }
            }
        }
        self.bump_epoch(collection);
        self.observe(collection, OpKind::Delete, t, existed as u64);
        Ok(existed)
    }

    fn scan(&mut self, collection: &str) -> Result<Vec<(RecordId, Record)>> {
        let t = Instant::now();
        let rows = self.store.scan(collection)?;
        self.observe(collection, OpKind::Scan, t, rows.len() as u64);
        Ok(rows)
    }

    fn count(&mut self, collection: &str) -> Result<usize> {
        let t = Instant::now();
        let n = self.store.count(collection)?;
        self.observe(collection, OpKind::Count, t, n as u64);
        Ok(n)
    }

    /// Delegated, so the engine's own callers get the cheap path too.
    ///
    /// Without this the trait default applies and `Database::ids` quietly
    /// costs a full decode of the collection — the exact expense the override
    /// exists to remove, reintroduced one layer up.
    fn ids(&mut self, collection: &str) -> Result<Vec<RecordId>> {
        self.store.ids(collection)
    }
}
