//! `Source` for `Database`: the record, index and column access the
//! executor reads through.

use adabt_core::error::Result;
use adabt_core::ids::RecordId;
use adabt_core::record::Record;
use adabt_core::store::LogicalStore;
use adabt_core::value::Value;
use adabt_exec::exec::Source;
use std::ops::Bound;

use super::Database;

impl Source for Database {
    fn peek_field(
        &mut self,
        collection: &str,
        id: RecordId,
        field: &str,
    ) -> Result<Option<Option<adabt_core::value::Value>>> {
        self.note_touch(collection, id);
        if !self
            .masked()
            .is_some_and(|h| h.hides_direct(self.revealed(), collection))
        {
            if let Some(d) = self.direct.get(collection) {
                // Row liveness first, from one bit — otherwise a dead row and
                // a live row missing the field would be indistinguishable.
                if !d.contains(id) {
                    return Ok(None);
                }
                return Ok(Some(d.field_at(id, field)?));
            }
        }
        // The heap-backed fallback answers through the codec's single-field
        // walk: a wide record's other text never decodes, let alone
        // allocates. The trait default (fetch-and-discard) still backs any
        // store that has not overridden it.
        self.store.peek_field(collection, id, field)
    }

    fn fetch_projected(
        &mut self,
        collection: &str,
        id: RecordId,
        fields: &[&str],
    ) -> Result<Option<Record>> {
        self.note_touch(collection, id);
        if !self
            .masked()
            .is_some_and(|h| h.hides_direct(self.revealed(), collection))
        {
            if let Some(d) = self.direct.get(collection) {
                if !d.contains(id) {
                    return Ok(None);
                }
                if fields.is_empty() {
                    return Ok(Some(Record::new()));
                }
                let mut rec = Record::new();
                for f in fields {
                    if let Some(v) = d.field_at(id, f)? {
                        // Direct arrays decode fixed fields without TLV — still
                        // per-field O(1), skipping non-requested strides.
                        rec.set((*f).to_string(), v);
                    }
                }
                return Ok(Some(rec));
            }
        }
        self.store.get_projected(collection, id, fields)
    }

    fn fetch(&mut self, collection: &str, id: RecordId) -> Result<Option<Record>> {
        self.note_touch(collection, id);
        // The Level 10 path: no page directory, no slot table, just an address
        // calculation. Falls through to the heap when no array exists — or when
        // one exists but is a candidate this query is not allowed to see.
        if !self
            .masked()
            .is_some_and(|h| h.hides_direct(self.revealed(), collection))
        {
            if let Some(d) = self.direct.get(collection) {
                return d.get(id);
            }
        }
        self.store.get(collection, id)
    }

    fn all_ids(&mut self, collection: &str) -> Result<Vec<RecordId>> {
        // Ids only. Routing this through `scan` decoded every record in the
        // collection so the executor could re-fetch and re-decode each one
        // immediately after — a full scan paying for itself twice.
        self.store.ids(collection)
    }

    fn column_aggregate(
        &mut self,
        collection: &str,
        group_by: &[String],
        aggs: &[adabt_ir::plan::Agg],
        predicate: Option<&adabt_ir::Expr>,
    ) -> Result<Option<Vec<(RecordId, Record)>>> {
        use adabt_ir::plan::AggKind;
        let Some(store) = self.columns.get(collection) else {
            return Ok(None);
        };
        let col_aggs: Vec<crate::column::ColumnAgg> = aggs
            .iter()
            .map(|a| crate::column::ColumnAgg {
                field: a.field.clone(),
                counts_rows: a.kind == AggKind::Count && a.field.is_none(),
            })
            .collect();

        let mut fields = Vec::new();
        if let Some(p) = predicate {
            p.referenced_fields(&mut fields);
        }
        let test = |r: &Record| predicate.map(|p| p.matches(r)).unwrap_or(true);
        let groups = store.aggregate(
            group_by,
            &col_aggs,
            predicate.map(|_| (fields.as_slice(), &test as &dyn Fn(&Record) -> bool)),
        );

        let mut out: Vec<(RecordId, Record)> = Vec::with_capacity(groups.len());
        for (i, (key, accs)) in groups.into_iter().enumerate() {
            let mut rec = Record::new();
            for (g, v) in group_by.iter().zip(key) {
                rec.set(g.clone(), v);
            }
            for (a, acc) in aggs.iter().zip(accs) {
                let v = match a.kind {
                    AggKind::Count => Value::U64(acc.count),
                    AggKind::Sum => {
                        if acc.saw_value {
                            Value::F64(acc.sum)
                        } else {
                            Value::Null
                        }
                    }
                    AggKind::Avg => {
                        if acc.count > 0 {
                            Value::F64(acc.sum / acc.count as f64)
                        } else {
                            Value::Null
                        }
                    }
                    AggKind::Min => acc.min.unwrap_or(Value::Null),
                    AggKind::Max => acc.max.unwrap_or(Value::Null),
                };
                rec.set(a.output.clone(), v);
            }
            out.push((RecordId(i as u64), rec));
        }
        Ok(Some(out))
    }

    fn column_scan(
        &mut self,
        collection: &str,
        fields: &[String],
    ) -> Result<Option<Vec<(RecordId, Record)>>> {
        let Some(c) = self.columns.get(collection) else {
            return Ok(None);
        };
        let refs: Vec<&str> = fields.iter().map(|s| s.as_str()).collect();
        Ok(Some(c.project(&refs)))
    }

    fn column_topk(
        &mut self,
        collection: &str,
        field: &str,
        descending: bool,
        k: usize,
    ) -> Result<Option<Vec<RecordId>>> {
        let Some(c) = self.columns.get(collection) else {
            return Ok(None);
        };
        Ok(c.topk_ids(field, descending, k))
    }

    fn index_lookup(
        &mut self,
        collection: &str,
        field: &str,
        key: &Value,
    ) -> Result<Option<Vec<RecordId>>> {
        Ok(self
            .indexes
            .get(collection)
            .and_then(|l| {
                let candidates: Vec<_> = l.iter().filter(|i| i.field() == field).collect();
                if candidates.is_empty() {
                    return None;
                }
                // The measured rule, applied where it can act: on a
                // low-cardinality field a bitmap answers at hash's latency
                // for ~6% of the memory (adabt-bench index-scale), so when
                // one exists take it. Otherwise first-created wins, the
                // shipped order.
                candidates
                    .iter()
                    .find(|i| {
                        matches!(i.kind(), adabt_index::IndexKind::Bitmap)
                            && i.key_count() <= adabt_index::LOW_CARDINALITY_KEY_COUNT
                    })
                    .or_else(|| candidates.first())
                    .copied()
            })
            .map(|i| i.lookup(key)))
    }

    /// Rows straight from a covering index, or `None` when none covers this
    /// query.
    ///
    /// Two conditions, and both are refusals rather than approximations. The
    /// index must be on `field`, and its projection must contain *every* field
    /// the plan above will read. A projection that covers most of what is
    /// needed is not a partial answer to be topped up — it is not an answer,
    /// and saying `None` sends the query down the ordinary indexed path.
    fn covering_lookup(
        &mut self,
        collection: &str,
        field: &str,
        key: &Value,
        needed: &[String],
    ) -> Result<Option<Vec<(RecordId, Record)>>> {
        let Some(list) = self.indexes.get(collection) else {
            return Ok(None);
        };
        let found = list.iter().find(|i| {
            let covers = i.covers();
            !covers.is_empty()
                && adabt_index::covering_parts(i.field()).0 == field
                && needed.iter().all(|n| covers.contains(n))
        });
        let Some(idx) = found else {
            return Ok(None);
        };
        let mut rows = Vec::new();
        for id in idx.lookup(key) {
            // An id from the index with no projection beside it would mean the
            // two halves had drifted apart. `CoveringIndex` maintains them
            // together precisely so this cannot happen; skipping rather than
            // fabricating an empty record keeps that a missing row rather than
            // a wrong one if it ever does.
            if let Some(rec) = idx.covered(id) {
                rows.push((id, rec.clone()));
            }
        }
        Ok(Some(rows))
    }

    /// The range sibling of `covering_lookup`: ids from the inner index's
    /// range scan, rows from the projections beside them, and no fetch.
    ///
    /// Two refusals, both load-bearing. The backing must be range-capable —
    /// a hash-backed covering index holds no ordering to walk, and asking it
    /// for a range would be a silent empty answer rather than an error. And
    /// the projection must contain every field the plan above reads, for the
    /// same reason `covering_lookup` demands it: a partial row is not an
    /// answer to be topped up, it is not an answer.
    fn covering_range(
        &mut self,
        collection: &str,
        field: &str,
        lo: Bound<&Value>,
        hi: Bound<&Value>,
        needed: &[String],
    ) -> Result<Option<Vec<(RecordId, Record)>>> {
        let Some(list) = self.indexes.get(collection) else {
            return Ok(None);
        };
        let found = list.iter().find(|i| {
            i.kind().supports_range() && {
                let covers = i.covers();
                !covers.is_empty()
                    && adabt_index::covering_parts(i.field()).0 == field
                    && needed.iter().all(|n| covers.contains(n))
            }
        });
        let Some(idx) = found else {
            return Ok(None);
        };
        let Some(ids) = idx.range(lo, hi) else {
            return Ok(None);
        };
        let mut rows = Vec::new();
        for id in ids {
            // Same drift guard as `covering_lookup`: skip rather than
            // fabricate.
            if let Some(rec) = idx.covered(id) {
                rows.push((id, rec.clone()));
            }
        }
        Ok(Some(rows))
    }

    /// A composite index answers to the NUL-joined name of its fields, so
    /// finding one is the same lookup by a different name — no separate
    /// registry, and no way for a single-field probe to reach one by
    /// accident.
    fn composite_lookup(
        &mut self,
        collection: &str,
        fields: &[String],
        key: &Value,
    ) -> Result<Option<Vec<RecordId>>> {
        let name = adabt_index::composite_name(fields);
        Ok(self
            .indexes
            .get(collection)
            .and_then(|l| l.iter().find(|i| i.field() == name))
            .map(|i| i.lookup(key)))
    }

    fn index_range(
        &mut self,
        collection: &str,
        field: &str,
        lo: Bound<&Value>,
        hi: Bound<&Value>,
    ) -> Result<Option<Vec<RecordId>>> {
        Ok(self
            .indexes
            .get(collection)
            .and_then(|l| {
                l.iter()
                    .find(|i| i.field() == field && i.kind().supports_range())
            })
            .and_then(|i| i.range(lo, hi)))
    }
}
