//! Serializable transactions, selected by policy.
//!
//! `Consistency::Strict` was declared in the policy surface since the
//! guarantees existed and enforced nowhere — a promise without a mechanism.
//! It now means something: at commit, the read set gets the same
//! first-committer-wins validation the write set always had, which closes
//! the write-skew anomaly that plain snapshot isolation permits, and every
//! collection it scanned is checked for any committed write at all, which
//! closes predicate phantoms. The same
//! workload under `Consistency::Snapshot` commits both transactions — this
//! file runs it both ways, because a guarantee is only honest when the test
//! shows what life is like *without* it.

use adabt_core::ids::RecordId;
use adabt_core::policy::{Consistency, Durability, Guarantees, Policy};
use adabt_core::record::Record;
use adabt_core::schema::{Schema, SchemaMode};
use adabt_core::store::LogicalStore;
use adabt_engine::Database;
use std::path::{Path, PathBuf};

struct Tmp(PathBuf);
impl Tmp {
    fn new(tag: &str) -> Self {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "adabt-ser-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&p);
        Tmp(p)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}
impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn policy(consistency: Consistency) -> Policy {
    Policy {
        guarantees: Guarantees {
            durability: Durability::Strict,
            consistency,
        },
        ..Policy::conventional()
    }
}

/// Two doctors, both on call. The classic write-skew setup: each transaction
/// reads *both* rows, then takes itself off call by writing only its own —
/// disjoint write-sets, overlapping read-sets. Committed together under
/// snapshot isolation, nobody is on call; no serial execution allows that.
fn on_call_db(dir: &Path, consistency: Consistency) -> Database {
    let mut db = Database::open(dir, policy(consistency)).unwrap();
    let schema = Schema::new(
        SchemaMode::Dynamic,
        vec![
            adabt_core::schema::FieldDef::new("who", adabt_core::schema::FieldType::Char(8)),
            adabt_core::schema::FieldDef::new("on_call", adabt_core::schema::FieldType::Bool),
        ],
    )
    .unwrap();
    db.create_collection("doctors", schema).unwrap();
    for who in ["alice", "bob"] {
        let id = RecordId(if who == "alice" { 1 } else { 2 });
        db.insert(
            "doctors",
            id,
            Record::new().with("who", who).with("on_call", true),
        )
        .unwrap();
    }
    db.checkpoint().unwrap();
    db
}

fn off_call(rec: &Record) -> Record {
    rec.clone().with("on_call", false)
}

#[test]
fn write_skew_commits_under_snapshot_isolation_and_is_refused_under_strict() {
    // Snapshot isolation: exactly the anomaly the module docs promise is
    // still possible. Both transactions commit; both doctors are off call.
    {
        let t = Tmp::new("si");
        let mut db = on_call_db(t.path(), Consistency::Snapshot);
        let mut t1 = db.begin();
        let mut t2 = db.begin();

        let alice = t1.get(&mut db, "doctors", RecordId(1)).unwrap().unwrap();
        let bob = t2.get(&mut db, "doctors", RecordId(2)).unwrap().unwrap();
        // Each saw the other as still on call...
        assert_eq!(
            t1.get(&mut db, "doctors", RecordId(2))
                .unwrap()
                .unwrap()
                .get("on_call"),
            Some(&adabt_core::value::Value::Bool(true))
        );
        assert_eq!(
            t2.get(&mut db, "doctors", RecordId(1))
                .unwrap()
                .unwrap()
                .get("on_call"),
            Some(&adabt_core::value::Value::Bool(true))
        );

        t1.update(&mut db, "doctors", RecordId(1), off_call(&alice))
            .unwrap();
        t2.update(&mut db, "doctors", RecordId(2), off_call(&bob))
            .unwrap();

        db.commit(t1).unwrap();
        db.commit(t2).expect("snapshot isolation permits the skew");

        let on_call_count = db
            .scan("doctors")
            .unwrap()
            .iter()
            .filter(|(_, r)| r.get("on_call") == Some(&adabt_core::value::Value::Bool(true)))
            .count();
        assert_eq!(on_call_count, 0, "the anomaly did not reproduce");
    }

    // Strict: same interleaving, second commit refused. Alice's write made
    // Bob's *read* of Alice stale, so Bob's commit conflicts even though
    // their write-sets never touched.
    {
        let t = Tmp::new("strict");
        let mut db = on_call_db(t.path(), Consistency::Strict);
        let mut t1 = db.begin();
        let mut t2 = db.begin();

        let alice = t1.get(&mut db, "doctors", RecordId(1)).unwrap().unwrap();
        let bob = t2.get(&mut db, "doctors", RecordId(2)).unwrap().unwrap();
        // The observations that make it skew: each sees the other on call.
        assert_eq!(
            t2.get(&mut db, "doctors", RecordId(1))
                .unwrap()
                .unwrap()
                .get("on_call"),
            Some(&adabt_core::value::Value::Bool(true))
        );
        let _ = bob;

        t1.update(&mut db, "doctors", RecordId(1), off_call(&alice))
            .unwrap();
        t2.update(&mut db, "doctors", RecordId(2), off_call(&bob))
            .unwrap();

        db.commit(t1).unwrap();
        let err = db.commit(t2).expect_err("strict must refuse the skew");
        assert!(
            matches!(err, adabt_core::error::Error::TransactionConflict { .. }),
            "{err}"
        );

        // And the refusal leaves a sane database: one doctor on call, and
        // the aborted transaction's write is nowhere (its commit never ran).
        let rows = db.scan("doctors").unwrap();
        let on_call: Vec<_> = rows
            .iter()
            .filter(|(_, r)| r.get("on_call") == Some(&adabt_core::value::Value::Bool(true)))
            .collect();
        assert_eq!(on_call.len(), 1);
    }
}

#[test]
fn strict_costs_innocent_workloads_nothing() {
    let t = Tmp::new("innocent");
    let mut db = on_call_db(t.path(), Consistency::Strict);

    // A read-only transaction over untouched rows commits cleanly.
    let mut reader = db.begin();
    let _ = reader.scan(&mut db, "doctors").unwrap();
    db.commit(reader).unwrap();

    // A writer whose keys were never observed by another live transaction
    // commits cleanly too.
    let mut w = db.begin();
    w.insert(
        &mut db,
        "doctors",
        RecordId(3),
        Record::new().with("who", "carol").with("on_call", true),
    )
    .unwrap();
    db.commit(w).unwrap();
    assert_eq!(db.count("doctors").unwrap(), 3);
}

#[test]
fn a_transactions_own_writes_need_no_read_validation() {
    // Read-your-own-writes through `get` must not put the written row into
    // conflict with itself: the buffered write shadows the snapshot read,
    // so committing what you wrote is not "a stale observation".
    let t = Tmp::new("own-writes");
    let mut db = on_call_db(t.path(), Consistency::Strict);
    let mut txn = db.begin();
    let alice = txn.get(&mut db, "doctors", RecordId(1)).unwrap().unwrap();
    txn.update(&mut db, "doctors", RecordId(1), off_call(&alice))
        .unwrap();
    assert_eq!(
        txn.get(&mut db, "doctors", RecordId(1)).unwrap().unwrap(),
        off_call(&alice)
    );
    db.commit(txn).unwrap();
}

fn i64(v: i64) -> adabt_core::value::Value {
    adabt_core::value::Value::I64(v)
}

fn doctor(who: &str, age: i64) -> Record {
    Record::new()
        .with("who", who)
        .with("on_call", true)
        .with("age", age)
}

/// The predicate both transactions in the phantom tests evaluate over a scan.
fn anyone_aged(txn: &mut adabt_engine::Transaction, db: &mut Database, age: i64) -> bool {
    txn.scan(db, "doctors")
        .unwrap()
        .iter()
        .any(|(_, r)| r.get("age") == Some(&i64(age)))
}

fn is_conflict(err: &adabt_core::error::Error) -> bool {
    matches!(err, adabt_core::error::Error::TransactionConflict { .. })
}

#[test]
fn write_skew_through_a_phantom_commits_under_snapshot_and_is_refused_under_strict() {
    // The rule: at most one doctor aged 30. Each transaction scans, finds
    // nobody aged 30, and inserts one under a fresh id. Their write-sets are
    // disjoint and neither saw the other's row — it did not exist yet — so no
    // per-id check can relate them. Only validating the scan itself can.
    for consistency in [Consistency::Snapshot, Consistency::Strict] {
        let t = Tmp::new(&format!("phantom-skew-{consistency:?}"));
        let mut db = on_call_db(t.path(), consistency);
        let mut t1 = db.begin();
        let mut t2 = db.begin();

        assert!(!anyone_aged(&mut t1, &mut db, 30));
        assert!(!anyone_aged(&mut t2, &mut db, 30));
        t1.insert(&mut db, "doctors", RecordId(10), doctor("eve", 30))
            .unwrap();
        t2.insert(&mut db, "doctors", RecordId(11), doctor("mallory", 30))
            .unwrap();

        db.commit(t1).unwrap();
        let second = db.commit(t2);

        let aged_30 = db
            .scan("doctors")
            .unwrap()
            .iter()
            .filter(|(_, r)| r.get("age") == Some(&i64(30)))
            .count();
        match consistency {
            Consistency::Snapshot => {
                second.expect("snapshot isolation permits the phantom");
                assert_eq!(aged_30, 2, "the anomaly did not reproduce");
            }
            _ => {
                let err = second.expect_err("strict must refuse the phantom");
                assert!(is_conflict(&err), "{err}");
                assert_eq!(aged_30, 1, "the refused transaction left a trace");
            }
        }
    }
}

/// A concurrent autocommit write against the scanned collection, as the
/// conflict tests below exercise it.
type ConcurrentWrite = fn(&mut Database);

/// Every kind of committed write a scan must be invalidated by. Rows 1 and 2
/// exist when the scanning transaction begins; row 50 does not.
const CONCURRENT_WRITES: [(&str, ConcurrentWrite); 3] = [
    ("insert", |db| {
        db.insert("doctors", RecordId(50), doctor("eve", 30))
            .unwrap();
    }),
    ("update", |db| {
        db.update("doctors", RecordId(1), doctor("alice", 30))
            .unwrap();
    }),
    ("delete", |db| {
        assert!(db.delete("doctors", RecordId(2)).unwrap());
    }),
];

#[test]
fn a_scan_conflicts_with_any_concurrent_write_to_its_collection_under_strict() {
    // The insert is the phantom proper: a row the scan never saw. The update
    // and the delete hit rows the scan did return, which the collection-level
    // check must cover now that a scan no longer records the ids it saw.
    for (what, concurrent_write) in CONCURRENT_WRITES {
        for consistency in [Consistency::Snapshot, Consistency::Strict] {
            let t = Tmp::new(&format!("scan-vs-{what}-{consistency:?}"));
            let mut db = on_call_db(t.path(), consistency);
            let mut txn = db.begin();
            let _ = txn.scan(&mut db, "doctors").unwrap();
            concurrent_write(&mut db);
            // A write on an id nobody else touches, so the write-set alone
            // can never conflict: anything refused here is the scan.
            txn.insert(&mut db, "doctors", RecordId(60), doctor("zed", 40))
                .unwrap();

            let result = db.commit(txn);
            match consistency {
                Consistency::Snapshot => {
                    result.unwrap_or_else(|e| panic!("{what}: snapshot refused: {e}"))
                }
                _ => {
                    let err = result.expect_err(what);
                    assert!(is_conflict(&err), "{what}: {err}");
                }
            }
        }
    }
}

#[test]
fn the_scan_conflict_names_the_collection_and_the_newest_write() {
    let t = Tmp::new("scan-conflict-names");
    let mut db = on_call_db(t.path(), Consistency::Strict);
    let mut txn = db.begin();
    let _ = txn.scan(&mut db, "doctors").unwrap();
    db.insert("doctors", RecordId(50), doctor("eve", 30))
        .unwrap();
    db.insert("doctors", RecordId(51), doctor("frank", 31))
        .unwrap();
    txn.insert(&mut db, "doctors", RecordId(60), doctor("zed", 40))
        .unwrap();

    match db.commit(txn) {
        Err(adabt_core::error::Error::TransactionConflict { collection, id }) => {
            assert_eq!(collection, "doctors");
            assert_eq!(id, RecordId(51));
        }
        other => panic!("expected a conflict, got {other:?}"),
    }
}

#[test]
fn a_scan_does_not_conflict_with_writes_to_another_collection() {
    let t = Tmp::new("scan-other-collection");
    let mut db = on_call_db(t.path(), Consistency::Strict);
    db.create_collection("shifts", Schema::new(SchemaMode::Dynamic, vec![]).unwrap())
        .unwrap();
    let mut txn = db.begin();
    let _ = txn.scan(&mut db, "doctors").unwrap();
    db.insert("shifts", RecordId(1), Record::new().with("who", "alice"))
        .unwrap();
    txn.insert(&mut db, "doctors", RecordId(60), doctor("zed", 40))
        .unwrap();
    db.commit(txn).unwrap();
}

#[test]
fn a_scan_does_not_conflict_with_earlier_commits_or_its_own_writes() {
    // The bound is the snapshot, not the transaction's first read, and a
    // transaction's own buffered writes are not "committed by someone else".
    let t = Tmp::new("scan-own-writes");
    let mut db = on_call_db(t.path(), Consistency::Strict);
    db.insert("doctors", RecordId(50), doctor("eve", 30))
        .unwrap();
    let mut txn = db.begin();
    txn.update(&mut db, "doctors", RecordId(1), doctor("alice", 41))
        .unwrap();
    txn.delete(&mut db, "doctors", RecordId(2)).unwrap();
    let rows = txn.scan(&mut db, "doctors").unwrap();
    assert_eq!(rows.len(), 2);
    txn.insert(&mut db, "doctors", RecordId(60), doctor("zed", 40))
        .unwrap();
    db.commit(txn).unwrap();
    assert_eq!(db.count("doctors").unwrap(), 3);
}

#[test]
fn point_reads_do_not_conflict_with_writes_to_other_rows_of_the_collection() {
    // Collection-level validation applies to scans only. A transaction that
    // reads by id depends on those ids alone, so a concurrent insert, update
    // or delete elsewhere in the collection must not abort it.
    for (what, concurrent_write) in CONCURRENT_WRITES {
        let t = Tmp::new(&format!("point-vs-{what}"));
        let mut db = on_call_db(t.path(), Consistency::Strict);
        db.insert("doctors", RecordId(3), doctor("carol", 50))
            .unwrap();
        let mut txn = db.begin();
        let carol = txn.get(&mut db, "doctors", RecordId(3)).unwrap().unwrap();
        concurrent_write(&mut db);
        txn.update(&mut db, "doctors", RecordId(3), off_call(&carol))
            .unwrap();
        db.commit(txn)
            .unwrap_or_else(|e| panic!("{what}: a point read conflicted: {e}"));
    }
}

#[test]
fn a_schema_rebuild_carries_the_collections_write_history() {
    // Deleting every row and then rebuilding the collection under a new
    // schema leaves a collection none of whose records was written after the
    // snapshot — there are none. The deletes happened, though, and the scan
    // that saw those rows must still be refused.
    let t = Tmp::new("scan-vs-rebuild");
    let mut db = on_call_db(t.path(), Consistency::Strict);
    let mut txn = db.begin();
    assert_eq!(txn.scan(&mut db, "doctors").unwrap().len(), 2);
    for id in [1, 2] {
        assert!(db.delete("doctors", RecordId(id)).unwrap());
    }
    let wider = Schema::new(
        SchemaMode::Dynamic,
        vec![
            adabt_core::schema::FieldDef::new("who", adabt_core::schema::FieldType::Char(8)),
            adabt_core::schema::FieldDef::new("on_call", adabt_core::schema::FieldType::Bool),
            adabt_core::schema::FieldDef::new("age", adabt_core::schema::FieldType::I64),
        ],
    )
    .unwrap();
    assert_eq!(db.alter_schema("doctors", wider).unwrap(), 0);
    txn.insert(&mut db, "doctors", RecordId(60), doctor("zed", 40))
        .unwrap();
    let err = db.commit(txn).expect_err("the deletes made the scan stale");
    assert!(is_conflict(&err), "{err}");
}
