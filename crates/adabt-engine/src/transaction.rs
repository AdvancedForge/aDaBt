//! Multi-statement transactions on one shard: snapshot isolation, or
//! serializable under `Consistency::Strict`.
//!
//! A [`Transaction`] is a value: [`Database::begin`](crate::database::Database::begin) hands one out, its reads
//! and writes accumulate in memory against a fixed snapshot, and
//! [`Database::commit`](crate::database::Database::commit) is the one moment any of it touches the database at
//! all. Dropping it without committing — or calling [`Database::abort`](crate::database::Database::abort), which
//! exists for the same purpose plus symmetry with `commit` — does exactly
//! nothing, because nothing was ever done.
//!
//! # Why buffering, not logging as you go
//!
//! The alternative — writing each statement to the log as it happens, and
//! sorting out at recovery time whether the transaction that wrote them ever
//! committed — is the design most write-ahead logs use, and it needs a third
//! recovery outcome beyond "committed" and "aborted": *in-doubt*, for a
//! transaction whose fate was never recorded because the process died before
//! it could be. That mechanism exists to serve a specific need: a participant in
//! a two-phase commit must durably remember an in-flight transaction's writes
//! *before* voting to commit, so that a crash after voting yes does not lose
//! them while the coordinator is still deciding.
//!
//! Nothing here votes. A `Transaction` lives on one `Database`, and the one
//! cross-shard path — [`ShardedDatabase::commit_coordinated`](crate::sharded::ShardedDatabase::commit_coordinated)
//! — journals its whole write-set before applying any of it, so it has no
//! in-doubt state either. Buffering in memory and applying only at commit is
//! therefore the correct design, not a shortcut, and a simpler one: a
//! transaction that never reaches `commit` leaves no trace in the log to
//! reason about, because it never wrote one. Should a voting protocol ever be
//! needed, a participant's durable "prepared" record is an additional WAL
//! opcode, not a rewrite of what is here; this module does not need to
//! anticipate it to avoid being broken by it.
//!
//! # Why atomicity does not need a shared commit timestamp
//!
//! Every write in this crate already carries its own [`adabt_core::ids::TxnId`]
//! — a fresh one per physical write, used for MVCC visibility. The obvious
//! worry is that applying a transaction's five writes with five different
//! stamps could let a reader observe three of them and not the other two.
//!
//! It cannot, and the reason is structural rather than a property of the
//! stamps: `Database`'s methods take `&mut self`, and this codebase has no
//! internal concurrency — no threads mutate one `Database` while another reads
//! it. [`Database::commit`](crate::database::Database::commit) runs to completion as one synchronous call; nothing
//! else can call [`Database::snapshot`](crate::database::Database::snapshot) until it returns, because doing so
//! would require a second live borrow of the same `&mut Database`, which the
//! borrow checker forbids. A snapshot opened before commit began has an `at`
//! fixed before any of the five stamps exist and sees none of them; one opened
//! after commit returns has an `at` past all five and sees all of them. There
//! is no window in which only some are visible, so there is nothing for a
//! shared timestamp to buy.
//!
//! (This stops being true the moment `Database` gains real internal
//! concurrency — a reader with a live `&self` borrow while a writer holds
//! `&mut self` elsewhere. Nothing here does that, and [`LogicalStore`](adabt_core::store::LogicalStore)'s own
//! documentation is explicit that its `&mut self` reads exist precisely to
//! keep it that way.)
//!
//! # What is enforced, and what is deferred to commit
//!
//! [`Transaction::insert`], [`update`](Transaction::update) and
//! [`delete`](Transaction::delete) check existence immediately, against the
//! transaction's own snapshot merged with its own prior writes — the same
//! read-your-own-writes view [`Transaction::get`] uses — so `insert` of an id
//! already visible to the transaction fails right away, exactly as the
//! ordinary non-transactional `insert` would.
//!
//! Schema validity and unique constraints are checked at commit, once,
//! against the state the whole write-set leaves, before anything is logged or
//! applied. Validating and applying each write as it passed would leave a
//! window in which a later failure aborts a transaction that has already
//! partly landed. After validation the write-set is logged as one group under
//! a single commit record, so it survives a crash entirely or not at all; see
//! the `commit` module of `database` for why that holds.
//!
//! # DDL is not transactional
//!
//! `create_collection`, `drop_collection`, `create_index` and schema changes
//! take effect immediately and are not part of any `Transaction` — there is no
//! `txn.create_collection`. This is a decision, not an oversight: DDL already
//! has its own carefully-reasoned crash story (`AdoptMigration`'s atomic flip,
//! the persisted catalog), built long before transactions existed and correct
//! on its own terms. Folding it into transactional semantics would mean
//! answering what a transaction that creates a collection *and* aborts should
//! do to a collection other, concurrently-committed transactions may already
//! be writing into — a real question, and not one that needs answering to
//! deliver multi-statement data transactions.
//!
//! It follows that DDL is outside the serializable guarantee below. A
//! transaction whose read or scanned collection has been dropped by the time
//! it commits fails with `NoSuchCollection`; one dropped and re-created in
//! between is judged only by the writes made to the new collection. A schema
//! change that rebuilds the collection writes every record again, so it
//! conflicts with a concurrent scan like any other write; one applied in
//! place rewrites nothing and does not.
//!
//! # Isolation: snapshot by default, serializable on request
//!
//! Two transactions with disjoint write-sets can, under plain snapshot
//! isolation, commit a result no serial execution would have produced (the
//! classic write-skew anomaly) — and that remains true when the policy says
//! [`adabt_core::policy::Consistency::Snapshot`].
//!
//! When the policy says [`adabt_core::policy::Consistency::Strict`], commit
//! also validates everything the transaction *observed*, optimistically and
//! with the same first-committer-wins rule the write set always had:
//!
//! - every id it read with [`Transaction::get`] — absence included, and
//!   including the existence checks `insert`, `update` and `delete` make —
//!   must not have been written since its snapshot;
//! - every collection it read with [`Transaction::scan`] must have received
//!   no committed write at all since its snapshot: no insert, no update, no
//!   delete, of any row.
//!
//! The second rule is what closes predicate phantoms. Whatever predicate a
//! caller evaluates over a scan, its answer can only change if the set of
//! rows changes, and any such change is a committed write to the collection.
//! Recording ids alone would miss the row a concurrent transaction inserts,
//! because the scan never saw it.
//!
//! Why that is serializable: a committed `Strict` transaction read nothing
//! that changed between its snapshot and its commit, so every read it made
//! returns the same answer at its commit point, where its writes land as one
//! group. It is therefore equivalent to running alone at that point, and
//! commit order is a serial order for all of them. (A read-only transaction
//! is equivalent to running at its snapshot, which is equally a point on that
//! order.) Nothing commits concurrently with a commit — `&mut self` again —
//! so "between snapshot and commit" is well defined.
//!
//! The price is granularity. A scan conflicts with *any* concurrent committed
//! write to its collection, including one to a row no predicate of the
//! caller's would ever have matched; a busy collection makes long scanning
//! transactions abort and retry. That is the honest cost of validating
//! without knowing the predicate — this API has no predicate to know — and
//! it is paid only under `Strict`: a transaction that only reads by id is
//! judged by those ids alone, as before.
//!
//! Reads are recorded under both policies so the guarantee stays a choice
//! made at the policy, not a fork in the transaction code path; under
//! `Snapshot` the record is simply never consulted.
//!
//! # What this does not do
//!
//! Nothing here reaches across shards. A `Transaction` is born from one
//! `Database` and can only write to it. A write-set spanning shards goes
//! through `ShardedDatabase::commit_coordinated` instead, which guarantees the
//! writes survive a crash together but takes no snapshot and validates no
//! reads, so it is not a cross-shard `Transaction`.

use adabt_core::error::{Error, Result};
use adabt_core::ids::{RecordId, TransactionId};
use adabt_core::record::Record;
use adabt_storage::version::Snapshot;
use std::collections::{BTreeSet, HashMap, HashSet};

/// One buffered write, keyed by `(collection, id)` in [`Transaction::writes`].
#[derive(Debug, Clone)]
pub(crate) enum Write {
    Put(Record),
    Delete,
}

/// A multi-statement transaction: snapshot isolation, or serializable when
/// the policy asks for `Consistency::Strict`.
///
/// See the module documentation for exactly what each guarantee means here
/// and why buffering in memory is the correct mechanism rather than a
/// shortcut.
pub struct Transaction {
    id: TransactionId,
    snapshot: Snapshot,
    writes: HashMap<(String, RecordId), Write>,
    /// Every id this transaction looked up through its snapshot with `get`,
    /// including lookups that found nothing. Recorded always, so the
    /// isolation decision can be made at commit rather than frozen at begin;
    /// validated only when the policy demands serializable, where an id
    /// written since the snapshot aborts the commit and closes write skew.
    /// Under plain snapshot isolation this set is written but never read,
    /// which is the cost of keeping one code path for both guarantees.
    reads: HashSet<(String, RecordId)>,
    /// Every collection this transaction scanned. Validated, under the same
    /// policy, against the collection's newest committed write rather than
    /// row by row, which is what catches a row inserted after the snapshot
    /// that the scan therefore never saw. Ordered so that, of several
    /// conflicting collections, the one reported is always the same.
    scanned: BTreeSet<String>,
}

impl Transaction {
    pub(crate) fn new(id: TransactionId, snapshot: Snapshot) -> Self {
        Self {
            id,
            snapshot,
            writes: HashMap::new(),
            reads: HashSet::new(),
            scanned: BTreeSet::new(),
        }
    }

    pub fn id(&self) -> TransactionId {
        self.id
    }

    /// The transaction's read view: everything committed at or before its
    /// snapshot was taken. Exposed so a caller can tell two transactions apart
    /// by when they began, not just by identity.
    pub fn snapshot(&self) -> &Snapshot {
        &self.snapshot
    }

    pub fn is_empty(&self) -> bool {
        self.writes.is_empty()
    }

    pub fn write_count(&self) -> usize {
        self.writes.len()
    }

    pub(crate) fn writes(&self) -> &HashMap<(String, RecordId), Write> {
        &self.writes
    }

    /// Every id this transaction looked up through its snapshot.
    pub(crate) fn reads(&self) -> &HashSet<(String, RecordId)> {
        &self.reads
    }

    /// Every collection this transaction scanned through its snapshot.
    pub(crate) fn scanned(&self) -> &BTreeSet<String> {
        &self.scanned
    }

    /// Read as this transaction sees it: its own buffered writes first, then
    /// its snapshot. A record this transaction deleted reads as absent even
    /// though the snapshot still holds it; one it inserted reads back even
    /// though the snapshot predates it.
    pub fn get(
        &mut self,
        db: &mut crate::Database,
        collection: &str,
        id: RecordId,
    ) -> Result<Option<Record>> {
        if let Some(w) = self.writes.get(&(collection.to_string(), id)) {
            return Ok(match w {
                Write::Put(r) => Some(r.clone()),
                Write::Delete => None,
            });
        }
        // A row the transaction observed through the snapshot is part of its
        // read set — absence included, since "no row here yet" is exactly
        // what write skew exploits.
        self.reads.insert((collection.to_string(), id));
        db.get_at(collection, id, &self.snapshot)
    }

    /// Scan as this transaction sees it: the snapshot, with this transaction's
    /// own writes overlaid. Ascending by id, matching every other scan in this
    /// project.
    pub fn scan(
        &mut self,
        db: &mut crate::Database,
        collection: &str,
    ) -> Result<Vec<(RecordId, Record)>> {
        let mut rows: std::collections::BTreeMap<RecordId, Record> = db
            .scan_at(collection, &self.snapshot)?
            .into_iter()
            .collect();
        for ((c, id), w) in &self.writes {
            if c != collection {
                continue;
            }
            match w {
                Write::Put(r) => {
                    rows.insert(*id, r.clone());
                }
                Write::Delete => {
                    rows.remove(id);
                }
            }
        }
        // The scan is recorded as a whole, not as the ids it returned: a
        // caller filtering these rows is asking a question about the
        // collection, and a row inserted after the snapshot changes the
        // answer without being one of these ids. Collection-level validation
        // also covers every returned id, so recording them as well would buy
        // nothing.
        self.scanned.insert(collection.to_string());
        Ok(rows.into_iter().collect())
    }

    /// Buffer an insert. Fails immediately, exactly as the ordinary
    /// non-transactional `insert` would, if this transaction already sees a
    /// record at `id` — whether from its snapshot or from an earlier write of
    /// its own.
    pub fn insert(
        &mut self,
        db: &mut crate::Database,
        collection: &str,
        id: RecordId,
        rec: Record,
    ) -> Result<()> {
        if self.get(db, collection, id)?.is_some() {
            return Err(Error::RecordExists(id));
        }
        self.writes
            .insert((collection.to_string(), id), Write::Put(rec));
        Ok(())
    }

    /// Buffer an upsert. Returns whether this transaction already saw a record
    /// at `id`, matching the ordinary `update`'s return.
    pub fn update(
        &mut self,
        db: &mut crate::Database,
        collection: &str,
        id: RecordId,
        rec: Record,
    ) -> Result<bool> {
        let existed = self.get(db, collection, id)?.is_some();
        self.writes
            .insert((collection.to_string(), id), Write::Put(rec));
        Ok(existed)
    }

    /// Buffer a delete. Returns whether this transaction already saw a record
    /// at `id`.
    pub fn delete(
        &mut self,
        db: &mut crate::Database,
        collection: &str,
        id: RecordId,
    ) -> Result<bool> {
        let existed = self.get(db, collection, id)?.is_some();
        self.writes
            .insert((collection.to_string(), id), Write::Delete);
        Ok(existed)
    }
}
