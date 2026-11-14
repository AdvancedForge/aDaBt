//! Alternative physical layouts: materialized views, direct-lookup arrays,
//! schema freezing and alteration, and the column store.

use adabt_core::error::{Error, Result};
use adabt_core::record::Record;
use adabt_core::schema::{Schema, SchemaMode};
use adabt_core::store::LogicalStore;

use crate::column::ColumnStore;
use crate::direct::DirectArray;

use super::Database;

impl Database {
    /// Build a view for this query, if it is one a view can serve.
    pub(super) fn maybe_materialize(&mut self, op: &adabt_ir::plan::LogicalOp) -> Result<()> {
        if !self.views.is_enabled() || self.views.has_view_for(op) {
            return Ok(());
        }
        let Some((collection, _, _)) = crate::matview::MaterializedViews::definition_of(op) else {
            return Ok(());
        };
        let collection = collection.to_string();
        let rows = self.store.scan(&collection)?;
        self.views.materialize(op, rows.iter().map(|(_, r)| r));
        Ok(())
    }

    pub fn materialized_views(&self) -> usize {
        self.views.len()
    }

    pub fn explain_materialized_views(&self) -> String {
        self.views.describe()
    }

    /// Whether a write has to read what it is about to overwrite.
    ///
    /// The standing cost of maintaining anything derived from a record's
    /// *contents* rather than its identity: an index has to be told which key to
    /// remove, a view which group to decrement, and neither can be worked out
    /// from the new record. It is why a structure nobody queries is a loss
    /// rather than merely neutral.
    pub(super) fn needs_old_record(&self, collection: &str) -> bool {
        self.indexes.get(collection).is_some_and(|l| !l.is_empty())
            || self.views.watches(collection)
    }

    /// Build a direct array for every collection whose schema allows one.
    pub(super) fn enable_direct_lookup(&mut self) -> Result<()> {
        for name in self.store.collection_names() {
            let schema = self.store.schema_of(&name)?.clone();
            if schema.mode() != SchemaMode::Fixed {
                continue;
            }
            let rows = self.store.scan(&name)?;
            if let Some(arr) = DirectArray::rebuild(schema, rows.iter().map(|(i, r)| (*i, r)))? {
                self.direct.insert(name, arr);
            }
        }
        Ok(())
    }

    pub(super) fn disable_direct_lookup(&mut self) {
        self.direct.clear();
    }

    /// Raise a collection's schema to the most rigid its data supports.
    ///
    /// Returns what was inferred, so the caller can report what the collection
    /// gave up. Refuses to narrow a schema that is already at least as rigid,
    /// and refuses outright if any stored record would no longer fit.
    pub fn freeze_schema(&mut self, collection: &str) -> Result<crate::infer::InferredSchema> {
        let rows = self.store.scan(collection)?;
        let records: Vec<Record> = rows.into_iter().map(|(_, r)| r).collect();
        let inferred = crate::infer::infer(records.iter(), crate::infer::DEFAULT_HEADROOM);

        let current = self.store.schema_of(collection)?.mode();
        if inferred.schema.mode().rigidity() <= current.rigidity() {
            return Err(Error::InvalidOptimization(format!(
                "{collection} is already {current:?}; inference concluded {:?}",
                inferred.schema.mode()
            )));
        }

        self.store
            .alter_schema(collection, inferred.schema.clone())?;
        self.invalidate_after_schema_change(collection);
        Ok(inferred)
    }

    /// Change a collection's schema.
    ///
    /// Application-driven counterpart to `freeze_schema`: that one *infers*
    /// the new schema from the data and only ever tightens it, this one takes
    /// whatever schema the caller names, which is how a product actually adds
    /// a field as it grows. Both end up here because both need the same
    /// derived-structure invalidation, for the same reason — see
    /// `invalidate_after_schema_change`.
    ///
    /// `HeapStore::alter_schema` picks its own cost: a pure append or
    /// trailing drop is a catalog edit, everything else is copy-and-swap. The
    /// `Ok(usize)` it returns is rows physically rewritten, `0` meaning the
    /// cheap path was taken; that distinction is the storage layer's to make,
    /// this method just reports it.
    pub fn alter_schema(&mut self, collection: &str, schema: Schema) -> Result<usize> {
        let rewritten = self.store.alter_schema(collection, schema)?;
        self.invalidate_after_schema_change(collection);
        Ok(rewritten)
    }

    /// Drop everything derived from `collection`'s old layout.
    ///
    /// Necessary even for the in-place schema-change path: an index or column
    /// store built against the old field set is still self-consistent data,
    /// just data that no longer matches what the schema now promises a reader
    /// — a dropped field silently disappearing from a column store's own
    /// declared shape would be a quieter version of the schema-freeze bug,
    /// where a crash left a new codec reading bytes the old one wrote
    /// (`docs/history/m14-notes.md`). Cheap either way, since every one of these is rebuildable.
    fn invalidate_after_schema_change(&mut self, collection: &str) {
        self.indexes.remove(collection);
        self.direct.remove(collection);
        self.columns.remove(collection);
        self.plan_cache.clear();
        self.compiled.clear();
        self.bump_epoch(collection);
    }

    /// Build a columnar copy of every collection.
    pub(super) fn enable_column_store(&mut self) -> Result<()> {
        for name in self.store.collection_names() {
            let rows = self.store.scan(&name)?;
            self.columns
                .insert(name, ColumnStore::build(rows.iter().map(|(i, r)| (*i, r))));
        }
        self.plan_cache.clear();
        self.compiled.clear();
        Ok(())
    }

    pub(super) fn disable_column_store(&mut self) {
        self.columns.clear();
        self.plan_cache.clear();
        self.compiled.clear();
    }

    /// Fraction of columnar rows that are tombstones, worst across collections.
    pub fn column_store_dead_fraction(&self) -> f64 {
        self.columns
            .values()
            .map(|c| c.dead_fraction())
            .fold(0.0, f64::max)
    }

    pub fn has_column_store(&self, collection: &str) -> bool {
        self.columns.contains_key(collection)
    }
}
