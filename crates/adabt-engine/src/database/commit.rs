//! Transactions: begin, commit and abort.
//!
//! # How a commit is made crash-atomic
//!
//! A commit has two halves, and the line between them is the one point at
//! which the transaction becomes true.
//!
//! **Decide**, against state nothing else can change while `&mut self` is
//! held: first-committer-wins over the write-set (and, under
//! `Consistency::Strict`, the ids read and the collections scanned), the
//! schema, and unique constraints judged against the state the whole
//! write-set would produce. Any failure here returns an error with nothing
//! written anywhere.
//!
//! **Apply**, which cannot be refused for any reason the transaction could
//! have anticipated: the write-set goes to the store as one group
//! ([`HeapStore::commit_writes`](adabt_storage::heap::HeapStore::commit_writes)),
//! which logs every write under a single commit record, makes the log durable
//! once, and only then changes any page. Recovery applies a group whose
//! commit record it finds and discards one whose record it does not, so a
//! crash anywhere in the commit recovers all of the transaction or none of
//! it — never the prefix that applying writes one by one, each logged on its
//! own, used to leave. The store's `commit` module carries the full argument,
//! including why it holds although the buffer pool may write a dirty page to
//! disk at any moment: no page is touched before the commit record is in the
//! log, so no page can carry an uncommitted change. Derived structures —
//! indexes, direct arrays, column stores, caches and telemetry — are updated
//! afterwards, exactly as a standalone `update` or `delete` updates them;
//! they are rebuilt from the store on open, so they need no crash story of
//! their own.

use adabt_core::error::{Error, Result};
use adabt_core::ids::RecordId;
use adabt_core::record::Record;
use adabt_core::store::LogicalStore;
use adabt_storage::heap::TxnWrite;
use std::time::Instant;

use super::Database;
use crate::transaction::{Transaction, Write};

/// One write of a transaction that has passed every check.
struct Staged {
    collection: String,
    id: RecordId,
    /// The record as given, and normalised the way the store will store it;
    /// `None` for a delete.
    put: Option<(Record, Record)>,
}

impl Staged {
    fn to_store_write(&self) -> TxnWrite {
        match &self.put {
            Some((given, _)) => TxnWrite::Put {
                collection: self.collection.clone(),
                id: self.id,
                record: given.clone(),
            },
            None => TxnWrite::Delete {
                collection: self.collection.clone(),
                id: self.id,
            },
        }
    }
}

impl Database {
    /// Begin a multi-statement transaction. See `crate::transaction` for the
    /// full account of what this guarantees and why.
    pub fn begin(&mut self) -> Transaction {
        let id = adabt_core::ids::TransactionId(self.next_txn_id);
        self.next_txn_id += 1;
        Transaction::new(id, self.store.snapshot())
    }

    /// Apply every buffered write, or none of them — including across a crash.
    ///
    /// Every key in the write-set is first checked, with nothing yet touched:
    /// for a first-committer-wins conflict against anything committed since
    /// the transaction's snapshot was taken; for a value being written,
    /// against the schema; and the write-set as a whole against unique
    /// constraints, as the state it would leave behind. Under
    /// `Consistency::Strict` every id the transaction read gets the same
    /// conflict check, and every collection it scanned must have received no
    /// committed write since the snapshot. Only once all of that has passed is
    /// anything written.
    ///
    /// The writes are then logged as one group under a single commit record
    /// and made durable once — one fsync under `Durability::Strict`, however
    /// many writes — before any of them reaches a page. A crash at any point
    /// during the commit recovers either the whole transaction or none of it;
    /// the module documentation explains why. Derived structures are then
    /// updated exactly as standalone `update` and `delete` calls would update
    /// them.
    ///
    /// Keys are visited in sorted order, which has nothing to do with
    /// correctness and everything to do with reproducibility: `HashMap`
    /// iteration order is not something a test — or a person reading a
    /// decision log — should have to tolerate as noise.
    pub fn commit(&mut self, txn: Transaction) -> Result<()> {
        if txn.is_empty() {
            return Ok(());
        }
        let keys = sorted_keys(&txn);
        self.check_write_conflicts(&txn, &keys)?;
        let staged = self.stage_writes(&txn, keys)?;
        let as_written: Vec<(&str, RecordId, Option<&Record>)> = staged
            .iter()
            .map(|s| (s.collection.as_str(), s.id, s.put.as_ref().map(|(_, n)| n)))
            .collect();
        self.check_unique_constraints_for_write_set(&as_written)?;
        self.check_read_conflicts(&txn)?;
        // Every check above passed against state that cannot have changed in
        // between — nothing else can touch `&mut self` while this call is on
        // the stack — so nothing from here on can fail for a reason this
        // transaction could have anticipated.
        self.apply_atomically(staged)
    }

    /// Discard a transaction without applying it. Equivalent to letting it drop
    /// — nothing was ever written anywhere — kept as an explicit method for
    /// symmetry with `commit` and so the intent reads at the call site.
    pub fn abort(&mut self, txn: Transaction) {
        drop(txn);
    }

    /// First-committer-wins over the write-set: refuse if anything this
    /// transaction writes was committed by someone else after its snapshot.
    fn check_write_conflicts(&self, txn: &Transaction, keys: &[(String, RecordId)]) -> Result<()> {
        keys.iter()
            .try_for_each(|(collection, id)| self.check_unchanged_since(txn, collection, *id))
    }

    /// Serializable posture (`Consistency::Strict`): everything the
    /// transaction observed gets the first-committer-wins check the write set
    /// already had.
    ///
    /// This is what makes `Strict` serializable — a transaction whose
    /// *observations* went stale between its snapshot and its commit aborts
    /// instead of committing a result no serial execution would have
    /// produced. Point reads close write skew; scans close predicate phantoms.
    /// Under `Consistency::Snapshot` it does nothing, costs nothing, and the
    /// guarantee is snapshot isolation exactly as documented.
    fn check_read_conflicts(&self, txn: &Transaction) -> Result<()> {
        if !self.validates_reads() {
            return Ok(());
        }
        self.check_point_reads(txn)?;
        self.check_scans(txn)
    }

    /// Whether commit validates what a transaction read, not only what it
    /// wrote. (The enum's ordinal runs strongest-first, so an inequality here
    /// would silently include `Eventual`; the guarantee is named exactly or
    /// not at all.)
    fn validates_reads(&self) -> bool {
        self.policy.guarantees.consistency == adabt_core::policy::Consistency::Strict
    }

    /// Refuse if any id read by `get` was written after the snapshot. Ids the
    /// transaction also writes were already checked with the write set.
    fn check_point_reads(&self, txn: &Transaction) -> Result<()> {
        txn.reads()
            .iter()
            .filter(|(collection, id)| !txn.writes().contains_key(&(collection.clone(), *id)))
            .try_for_each(|(collection, id)| self.check_unchanged_since(txn, collection, *id))
    }

    /// Refuse if any scanned collection received a committed write — insert,
    /// update or delete, of any row — after the snapshot.
    ///
    /// Collection granularity is deliberate. A scan's caller may have
    /// evaluated any predicate over the rows; the only way its answer can
    /// change is for the set of rows to change, and every such change advances
    /// the collection's newest write. That includes a row inserted after the
    /// snapshot, which no per-id check can see because the scan never saw the
    /// id — the phantom. The cost is that a write no predicate would have
    /// matched conflicts too.
    fn check_scans(&self, txn: &Transaction) -> Result<()> {
        txn.scanned()
            .iter()
            .try_for_each(|collection| self.check_collection_unchanged_since(txn, collection))
    }

    /// Refuse if `collection`/`id` was written after `txn`'s snapshot.
    fn check_unchanged_since(
        &self,
        txn: &Transaction,
        collection: &str,
        id: RecordId,
    ) -> Result<()> {
        match self.store.latest_write_ts(collection, id)? {
            Some(ts) if ts.0 > txn.snapshot().at().0 => Err(Error::TransactionConflict {
                collection: collection.to_string(),
                id,
            }),
            _ => Ok(()),
        }
    }

    /// Refuse if anything in `collection` was written after `txn`'s snapshot.
    /// The conflict names the newest such write, which is the one that made
    /// the scan stale whichever row the caller cared about.
    fn check_collection_unchanged_since(&self, txn: &Transaction, collection: &str) -> Result<()> {
        match self.store.newest_write_in(collection)? {
            Some((ts, id)) if ts.0 > txn.snapshot().at().0 => Err(Error::TransactionConflict {
                collection: collection.to_string(),
                id,
            }),
            _ => Ok(()),
        }
    }

    /// Normalise and schema-check every put, in key order.
    fn stage_writes(
        &self,
        txn: &Transaction,
        keys: Vec<(String, RecordId)>,
    ) -> Result<Vec<Staged>> {
        let mut staged = Vec::with_capacity(keys.len());
        for key in keys {
            let put = match &txn.writes()[&key] {
                Write::Put(rec) => {
                    let mut normalized = rec.clone();
                    adabt_core::store::normalize_for_storage(&mut normalized);
                    self.store.schema_of(&key.0)?.validate_record(&normalized)?;
                    Some((rec.clone(), normalized))
                }
                Write::Delete => None,
            };
            let (collection, id) = key;
            staged.push(Staged {
                collection,
                id,
                put,
            });
        }
        Ok(staged)
    }

    /// Hand the write-set to the store as one crash-atomic group, then bring
    /// every derived structure into step with it.
    fn apply_atomically(&mut self, staged: Vec<Staged>) -> Result<()> {
        let started = Instant::now();
        // Read before the store changes: un-indexing needs what was there.
        let olds = staged
            .iter()
            .map(|s| self.old_record_for_derived(&s.collection, s.id))
            .collect::<Result<Vec<_>>>()?;
        let writes = staged.iter().map(Staged::to_store_write).collect();
        let existed = self.store.commit_writes(writes)?;
        for ((s, old), existed) in staged.into_iter().zip(olds).zip(existed) {
            match &s.put {
                Some((_, stored)) => {
                    self.reflect_put(&s.collection, s.id, old, stored, started, existed)
                }
                None => self.reflect_delete(&s.collection, s.id, old, started, existed),
            }
        }
        Ok(())
    }
}

/// The write-set's keys in sorted order.
fn sorted_keys(txn: &Transaction) -> Vec<(String, RecordId)> {
    let mut keys: Vec<(String, RecordId)> = txn.writes().keys().cloned().collect();
    keys.sort();
    keys
}
