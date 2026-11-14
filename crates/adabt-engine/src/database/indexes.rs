//! Index management: creating and dropping every index shape, index queries
//! and memory accounting, and clustering hints.

use adabt_core::error::{Error, Result};
use adabt_core::ids::RecordId;
use adabt_core::index_kind::IndexKind;
use adabt_core::store::LogicalStore;
use adabt_core::value::Value;
use adabt_index::{BTreeIndex, BitmapIndex, HashIndex, Index};

use super::{Database, IndexSpec};

impl Database {
    // -- index management -------------------------------------------------

    pub fn create_index(&mut self, collection: &str, field: &str, kind: IndexKind) -> Result<()> {
        self.create_index_from(collection, field, kind, None)
    }

    /// Build an index over several fields at once.
    ///
    /// Serves an equality predicate that constrains *every* covered field —
    /// `country = 'NO' AND age = 30` for an index over `(country, age)`.
    /// It does not serve a prefix of them; see `CompositeIndex`. The index
    /// answers to the joined name `composite_name(fields)`, which cannot
    /// collide with a single-field index because the separator is a NUL, so
    /// the existing single-field planner path simply never selects one
    /// rather than selecting one wrongly.
    pub fn create_composite_index(&mut self, collection: &str, fields: &[String]) -> Result<()> {
        if self.store.schema_of(collection).is_err() {
            return Err(Error::NoSuchCollection(collection.to_string()));
        }
        if fields.len() < 2 {
            return Err(Error::InvalidOptimization(
                "a composite index needs at least two fields; use create_index for one".into(),
            ));
        }
        let name = adabt_index::composite_name(fields);
        if self
            .derived
            .index_exists(collection, &name, IndexKind::Hash)
        {
            return Ok(());
        }
        let rows = self.store.scan(collection)?;
        let idx =
            adabt_index::CompositeIndex::build(fields.to_vec(), rows.iter().map(|(i, r)| (*i, r)));
        self.derived
            .indexes
            .entry(collection.to_string())
            .or_default()
            .push(Box::new(idx));
        self.store
            .record_index(collection, &name, IndexKind::Hash.as_str())?;
        self.caches.bump_epoch(collection);
        self.caches.plans.clear();
        Ok(())
    }

    /// Build an index on `field` that also carries `covers` for every record
    /// it indexes.
    ///
    /// A query that filters on `field` and needs nothing outside `covers` is
    /// then answered from the index alone — no page directory, no buffer pool,
    /// no decode. On this engine that removes the majority of what a lookup
    /// costs rather than trimming it, because the fetch *is* the cost.
    ///
    /// The trade is a second copy of the covered data and upkeep on every
    /// write to any covered field, not just the indexed one. Worth it when the
    /// read side uses it and a straight loss when it does not, which is why
    /// this is an explicit request rather than something inferred.
    pub fn create_covering_index(
        &mut self,
        collection: &str,
        field: &str,
        covers: &[String],
        kind: IndexKind,
    ) -> Result<()> {
        if self.store.schema_of(collection).is_err() {
            return Err(Error::NoSuchCollection(collection.to_string()));
        }
        if covers.is_empty() {
            return Err(Error::InvalidOptimization(
                "a covering index must carry at least one field; use create_index for none".into(),
            ));
        }
        // The indexed field is always carried, whether or not the caller asked
        // for it. Not a convenience — a correctness requirement. The plan puts
        // a `Filter` above the lookup, because the predicate may constrain
        // fields beyond the indexed one, and that filter re-evaluates the
        // whole predicate against the row the index produced. A row missing
        // the field the predicate tests on evaluates to `Unknown`, not `True`,
        // so every row would be dropped — the index would return nothing and
        // be perfectly consistent about it.
        let mut covers: Vec<String> = covers.to_vec();
        covers.push(field.to_string());
        covers.sort();
        covers.dedup();
        let name = adabt_index::covering_name(field, &covers);
        if self.derived.index_exists(collection, &name, kind) {
            return Ok(());
        }
        let rows = self.store.scan(collection)?;
        let idx = adabt_index::CoveringIndex::build(
            field,
            covers,
            kind,
            rows.iter().map(|(i, r)| (*i, r)),
        );
        self.derived
            .indexes
            .entry(collection.to_string())
            .or_default()
            .push(Box::new(idx));
        self.store.record_index(collection, &name, kind.as_str())?;
        self.caches.bump_epoch(collection);
        self.caches.invalidate_plans();
        Ok(())
    }

    /// Build an index on `field` holding only records satisfying `condition`.
    ///
    /// Smaller than a full index and cheaper to maintain in proportion to how
    /// selective the condition is: a write to a record the condition excludes
    /// touches nothing at all.
    ///
    /// The engine will only *use* one for a query whose predicate contains a
    /// syntactically identical conjunct. That is much weaker than real
    /// predicate implication — an index conditioned on `age > 18` will not
    /// serve a query asking `age > 20`, though it plainly could — and it is
    /// weak on purpose. Being too weak costs a slower plan; being too clever
    /// costs correct answers.
    pub fn create_partial_index(
        &mut self,
        collection: &str,
        field: &str,
        condition: adabt_ir::Expr,
        kind: IndexKind,
    ) -> Result<()> {
        if self.store.schema_of(collection).is_err() {
            return Err(Error::NoSuchCollection(collection.to_string()));
        }
        // Hex, not `Debug` text: the name is the only channel a persisted
        // index definition has, and a condition that cannot be read back is a
        // partial index that returns as a *full* one after a restart —
        // holding a subset of the rows and claiming to hold all of them.
        let encoded = crate::exprcodec::encode_expr_hex(&condition)?;
        let name = adabt_index::partial_name(field, &encoded);
        if self.derived.index_exists(collection, &name, kind) {
            return Ok(());
        }
        let rows = self.store.scan(collection)?;
        let idx = adabt_index::PartialIndex::build(
            field,
            condition,
            encoded,
            kind,
            rows.iter().map(|(i, r)| (*i, r)),
        );
        self.derived
            .indexes
            .entry(collection.to_string())
            .or_default()
            .push(Box::new(idx));
        self.store.record_index(collection, &name, kind.as_str())?;
        self.caches.bump_epoch(collection);
        self.caches.invalidate_plans();
        Ok(())
    }

    /// Build an index, from cached entries when they are available.
    ///
    /// The two paths produce the same structure. The difference is what they
    /// cost: rebuilding decodes every record in the collection, while restoring
    /// reads back the keys directly and never touches a heap page.
    pub(super) fn create_index_from(
        &mut self,
        collection: &str,
        field: &str,
        kind: IndexKind,
        cached: Option<Vec<(Value, Vec<RecordId>)>>,
    ) -> Result<()> {
        if self.store.schema_of(collection).is_err() {
            return Err(Error::NoSuchCollection(collection.to_string()));
        }
        if self.derived.index_exists(collection, field, kind) {
            return Ok(());
        }
        // A composite index answers to the NUL-joined name of its fields, so
        // that name is also how one is recognised when rebuilt at startup.
        // Without this the restore path constructed a *single-field* index
        // over a field literally called "country\0age" — a field no record
        // has — so the index came back empty and every query through it
        // returned nothing. Silently, since an empty index is a valid index.
        //
        // A covering index has the same hazard in a sharper form. Its name
        // carries the projection after a `\u{1}`, and its payload — the
        // projected fields — is *not* in the cached key snapshot, which holds
        // only keys and ids. Restoring one from cache would produce an index
        // that finds the right ids and has no rows to serve them from. So a
        // covering index always rebuilds from the heap, and the cache is
        // ignored rather than half-used.
        // A partial index carries its condition, hex-encoded, after a
        // `\u{2}`. Rebuilding one as an ordinary index would produce an index
        // holding a subset of the rows that claims to hold all of them — the
        // worst version of the composite restore bug, since the wrong answers
        // would look like correct ones.
        let partial = field.contains(adabt_index::PARTIAL_SEP);
        let covering = field.contains(adabt_index::COVER_SEP);
        let mut idx: Box<dyn Index> = if partial {
            let (base, encoded) = adabt_index::partial_parts(field);
            let encoded = encoded.unwrap_or_default();
            let condition = crate::exprcodec::decode_expr_hex(&encoded)?;
            Box::new(adabt_index::PartialIndex::new(
                base, condition, encoded, kind,
            ))
        } else if covering {
            let (base, covers) = adabt_index::covering_parts(field);
            Box::new(adabt_index::CoveringIndex::new(base, covers, kind))
        } else if field.contains(adabt_index::COMPOSITE_SEP) {
            Box::new(adabt_index::CompositeIndex::new(
                adabt_index::composite_fields(field),
            ))
        } else {
            match kind {
                IndexKind::Hash => Box::new(HashIndex::new(field)),
                IndexKind::BTree => Box::new(BTreeIndex::new(field)),
                IndexKind::Bitmap => Box::new(BitmapIndex::new(field)),
            }
        };
        // A partial index cannot be restored from the key cache either: the
        // cache records which keys map to which ids, not which records passed
        // the condition, and re-inserting every cached entry would admit rows
        // the condition excludes.
        match cached.filter(|_| !covering && !partial) {
            Some(entries) => {
                for (key, ids) in entries {
                    for id in ids {
                        idx.insert(key.clone(), id);
                    }
                }
            }
            None => {
                for (id, rec) in &self.store.scan(collection)? {
                    idx.index_record(*id, rec);
                }
            }
        }
        self.derived
            .indexes
            .entry(collection.to_string())
            .or_default()
            .push(idx);
        self.store.record_index(collection, field, kind.as_str())?;
        // A cached plan may encode an access path that is now suboptimal, or
        // one that referenced a structure that has changed.
        self.caches.invalidate_plans();
        Ok(())
    }

    pub fn drop_index(&mut self, collection: &str, field: &str, kind: IndexKind) -> bool {
        let Some(list) = self.derived.indexes.get_mut(collection) else {
            return false;
        };
        let before = list.len();
        list.retain(|i| !(i.field() == field && i.kind() == kind));
        let changed = before != list.len();
        if changed {
            self.caches.invalidate_plans();
            let _ = self.store.forget_index(collection, field, kind.as_str());
        }
        changed
    }

    pub fn index_specs(&self) -> Vec<IndexSpec> {
        let mut out = Vec::new();
        for (c, list) in &self.derived.indexes {
            for i in list {
                out.push(IndexSpec {
                    collection: c.clone(),
                    field: i.field().to_string(),
                    kind: i.kind(),
                });
            }
        }
        out.sort_by(|a, b| {
            (&a.collection, &a.field, a.kind.as_str()).cmp(&(
                &b.collection,
                &b.field,
                b.kind.as_str(),
            ))
        });
        out
    }

    pub fn has_index(&self, collection: &str, field: &str) -> bool {
        self.derived
            .indexes
            .get(collection)
            .is_some_and(|list| list.iter().any(|i| i.field() == field))
    }

    pub fn index_kind(&self, collection: &str, field: &str) -> Option<IndexKind> {
        self.derived
            .indexes
            .get(collection)
            .and_then(|list| list.iter().find(|i| i.field() == field).map(|i| i.kind()))
    }

    /// Memory held by every derived structure, for the resource axis.
    pub fn derived_memory_bytes(&self) -> usize {
        self.index_memory_bytes()
            + self.caches.results.memory_bytes()
            + self
                .derived
                .direct
                .values()
                .map(|d| d.memory_bytes())
                .sum::<usize>()
    }

    /// Bytes occupied by live records in the primary representation.
    pub fn stored_bytes(&mut self) -> Result<u64> {
        self.store.stored_bytes()
    }

    pub fn compression_enabled(&self) -> bool {
        self.store.compression_enabled()
    }

    /// Declare `field` as the collection's clustering hint: subsequent inserts
    /// are *placed* so records with nearby integer values land on the same
    /// pages, which is what turns a range scan over that field from a
    /// scatter of page reads into a run of consecutive ones.
    ///
    /// What is promised, precisely:
    /// - answers never change — clustering is placement, not content, and
    ///   every read path is identical to an unclustered collection's;
    /// - the hint is advisory. Updates may move a record to any page with
    ///   room; deletes leave holes. The *declaration* persists across
    ///   restarts (it is catalog state), but the placement ranges re-derive:
    ///   locality rebuilds as new keyed inserts arrive, it is not rebuilt
    ///   retroactively for old data.
    ///
    /// Only integer fields can steer placement today: the key is the field's
    /// value cast to `i64`, and anything else is silently inserted unclustered.
    pub fn declare_cluster_field(&mut self, collection: &str, field: &str) -> Result<()> {
        self.store.schema_of(collection)?;
        self.store.set_cluster_field(collection, Some(field))?;
        self.tuning
            .cluster_fields
            .insert(collection.to_string(), field.to_string());
        Ok(())
    }

    /// Drop a collection's clustering hint and forget its declaration.
    /// Existing placements stay where they are; only future keyed inserts
    /// revert to first-fit.
    pub fn clear_cluster_field(&mut self, collection: &str) -> Result<()> {
        self.store.set_cluster_field(collection, None)?;
        self.tuning.cluster_fields.remove(collection);
        Ok(())
    }

    /// Distinct pages `get` has touched since the last clear — the number a
    /// locality claim is measured against. See [`HeapStore::touched_pages`](adabt_storage::heap::HeapStore::touched_pages).
    pub fn touched_pages(&self) -> usize {
        self.store.touched_pages()
    }

    /// Reset the touched-page diagnostic.
    pub fn clear_page_touches(&mut self) {
        self.store.clear_page_touches();
    }

    /// The declared clustering hint for `collection`, if any.
    pub fn cluster_field(&self, collection: &str) -> Option<&str> {
        self.tuning
            .cluster_fields
            .get(collection)
            .map(String::as_str)
    }

    pub fn index_memory_bytes(&self) -> usize {
        self.derived.index_memory_bytes()
    }
}
