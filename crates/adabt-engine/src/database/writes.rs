//! Transactions and record-id allocation: begin, commit, abort, and the
//! batched and auto-id insert paths.

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
    // -- transactions --------------------------------------------------------

    /// Begin a multi-statement transaction. See `crate::transaction` for the
    /// full account of what this guarantees and why.
    pub fn begin(&mut self) -> crate::transaction::Transaction {
        let id = adabt_core::ids::TransactionId(self.next_txn_id);
        self.next_txn_id += 1;
        crate::transaction::Transaction::new(id, self.store.snapshot())
    }

    /// Apply every buffered write, or none of them.
    ///
    /// Two passes, in this order and never interleaved. First, every key in the
    /// write-set is checked — for a first-committer-wins conflict against
    /// anything committed since the transaction's snapshot was taken, and, for
    /// a value being written, against the schema and any unique constraint —
    /// with nothing yet touched. Only once every key has cleared does the
    /// second pass apply any of them, through the ordinary `update`/`delete`
    /// paths so reindexing, epoch bumps and telemetry all happen exactly as
    /// they would for a standalone write.
    ///
    /// "Or none" holds against every check above, not against a crash: each
    /// write in the second pass is logged and made durable on its own, and
    /// there is no commit record, so a process killed partway through leaves
    /// a prefix of the sorted write-set behind (`tests/commit_window_chaos.rs`
    /// pins exactly that).
    ///
    /// Keys are visited in sorted order, which has nothing to do with
    /// correctness and everything to do with reproducibility: `HashMap`
    /// iteration order is not something a test — or a person reading a
    /// decision log — should have to tolerate as noise.
    pub fn commit(&mut self, txn: crate::transaction::Transaction) -> Result<()> {
        if txn.is_empty() {
            return Ok(());
        }
        let snapshot_at = txn.snapshot().at();
        let mut keys: Vec<(String, RecordId)> = txn.writes().keys().cloned().collect();
        keys.sort();

        for key in &keys {
            let (collection, id) = key;
            if let Some(ts) = self.store.latest_write_ts(collection, *id)? {
                if ts.0 > snapshot_at.0 {
                    return Err(Error::TransactionConflict {
                        collection: collection.clone(),
                        id: *id,
                    });
                }
            }
            if let crate::transaction::Write::Put(rec) = &txn.writes()[key] {
                let mut normalized = rec.clone();
                adabt_core::store::normalize_for_storage(&mut normalized);
                self.store
                    .schema_of(collection)?
                    .validate_record(&normalized)?;
                self.check_unique_constraints(collection, Some(*id), &normalized)?;
            }
        }

        // Serializable posture (`Consistency::Strict`): the read set gets the
        // same first-committer-wins check the write set already had. This is
        // what closes write skew — a transaction whose *observations* went
        // stale between its snapshot and its commit aborts instead of
        // committing a result no serial execution would have produced. Under
        // `Consistency::Snapshot` this pass does not run and the guarantee is
        // snapshot isolation exactly as documented. (The enum's ordinal runs
        // strongest-first, so an inequality here would silently include
        // `Eventual`; the guarantee is named exactly or not at all.)
        if self.policy.guarantees.consistency == adabt_core::policy::Consistency::Strict {
            for (collection, id) in txn.reads() {
                if !txn.writes().contains_key(&(collection.clone(), *id)) {
                    if let Some(ts) = self.store.latest_write_ts(collection, *id)? {
                        if ts.0 > snapshot_at.0 {
                            return Err(Error::TransactionConflict {
                                collection: collection.clone(),
                                id: *id,
                            });
                        }
                    }
                }
            }
        }

        // Every check above passed against state that cannot have changed in
        // between — nothing else can touch `&mut self` while this call is on
        // the stack — so nothing from here on can fail for a reason this
        // transaction could have anticipated.
        for key in keys {
            match &txn.writes()[&key] {
                crate::transaction::Write::Put(rec) => {
                    self.update(&key.0, key.1, rec.clone())?;
                }
                crate::transaction::Write::Delete => {
                    self.delete(&key.0, key.1)?;
                }
            }
        }
        Ok(())
    }

    /// Discard a transaction without applying it. Equivalent to letting it drop
    /// — nothing was ever written anywhere — kept as an explicit method for
    /// symmetry with `commit` and so the intent reads at the call site.
    pub fn abort(&mut self, txn: crate::transaction::Transaction) {
        drop(txn);
    }

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
    /// per-record fsync: either every one of these lands, or none does.
    pub fn insert_batch(
        &mut self,
        collection: &str,
        records: Vec<(RecordId, Record)>,
    ) -> Result<usize> {
        let t = Instant::now();
        // Normalised here, once, rather than trusted from the store: reindexing
        // has to see the same shape of record a plain `insert` would reindex,
        // and recomputing it from what was actually written would mean a second
        // scan for no reason — `insert_batch` is all-or-nothing, so if it
        // returns at all, every one of these was written exactly as given.
        let mut normalized = Vec::with_capacity(records.len());
        for (id, rec) in &records {
            let mut r = rec.clone();
            adabt_core::store::normalize_for_storage(&mut r);
            normalized.push((*id, r));
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
        let n = self.store.insert_batch(collection, records)?;
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
