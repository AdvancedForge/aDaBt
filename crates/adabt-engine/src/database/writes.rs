//! Record-id allocation and the batched and auto-id insert paths.
//! Transactions live in `commit`.

use adabt_core::error::{Error, Result};
use adabt_core::ids::RecordId;
use adabt_core::record::Record;
use adabt_core::store::LogicalStore;
use adabt_core::value::Value;
use adabt_telemetry::event::OpKind;
use std::collections::HashMap;
use std::time::Instant;

use super::Database;

impl Database {
    // -- record id allocation ----------------------------------------------

    /// The id an auto-allocated insert into `collection` will use next.
    ///
    /// A peek: nothing is reserved until [`Database::insert_auto`] actually
    /// writes with it.
    pub fn next_id(&self, collection: &str) -> Result<RecordId> {
        self.store.next_id(collection)
    }

    /// Insert many records in one call, at one fsync instead of one per row.
    ///
    /// Every derived structure is maintained exactly as it would be for the
    /// same records inserted one at a time — reindexed, counted into telemetry
    /// as ordinary inserts — the difference is entirely in how many times the
    /// log is made durable. See [`adabt_storage::heap::HeapStore::insert_batch`]
    /// for the all-or-nothing guarantee that makes it safe to skip the
    /// per-record fsync: either every one of these lands, or none does — on
    /// a refused record and across a crash alike, because the batch is logged
    /// under one commit record exactly as a transaction is.
    pub fn insert_batch(
        &mut self,
        collection: &str,
        records: Vec<(RecordId, Record)>,
    ) -> Result<usize> {
        let t = Instant::now();
        // Normalised here, once and in place, rather than trusted from the
        // store: reindexing has to see the same shape of record a plain
        // `insert` would reindex, and recomputing it from what was actually
        // written would mean a second scan for no reason — the batch is
        // all-or-nothing, so if it returns at all, every one of these was
        // written exactly as given. The store is then lent these same records,
        // already in the shape it stores, so nothing is copied.
        let mut normalized = records;
        for (_, rec) in &mut normalized {
            adabt_core::store::normalize_for_storage(rec);
        }
        // Checked against existing data *and* against the rest of the batch,
        // before a single row is written — a batch is all-or-nothing, and two
        // records within it sharing a constrained value is exactly as much a
        // violation as one of them colliding with something already stored.
        if !self.unique_constraints.is_empty() {
            let mut seen_in_batch: HashMap<(&str, &Value), RecordId> = HashMap::new();
            for (id, rec) in &normalized {
                self.check_unique_constraints(collection, None, rec)?;
                for field in self.unique_constraints.on(collection) {
                    let Some(value) = rec.get(field).filter(|v| !v.is_null()) else {
                        continue;
                    };
                    if let Some(&other) = seen_in_batch.get(&(field, value)) {
                        return Err(Error::UniqueViolation {
                            collection: collection.to_string(),
                            field: field.to_string(),
                            value: format!(
                                "{value:?} (shared within this batch by records {other} and {id})"
                            ),
                        });
                    }
                    seen_in_batch.insert((field, value), *id);
                }
            }
        }
        let n = self.store.insert_batch_from(collection, &normalized)?;
        for (id, rec) in &normalized {
            self.derived
                .on_insert(collection, *id, rec, &self.telemetry.probe);
        }
        self.caches.bump_epoch(collection);
        self.telemetry
            .observe(collection, OpKind::Insert, t, n as u64);
        Ok(n)
    }

    /// Insert without naming an id, and learn which one was used.
    ///
    /// Every user of `insert(collection, id, rec)` must otherwise invent a
    /// global id allocator before writing a first record — and a bad one skews
    /// `ShardedDatabase`'s `id % shards` routing towards whichever shard its ids
    /// happen to land on. This is the ordinary answer: monotonic within a
    /// collection, persisted so a restart never reuses one, and advanced by a
    /// manual insert too, so the two never collide.
    pub fn insert_auto(&mut self, collection: &str, rec: Record) -> Result<RecordId> {
        let id = self.next_id(collection)?;
        self.insert(collection, id, rec)?;
        Ok(id)
    }
}
