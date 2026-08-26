//! Serializable transactions, selected by policy.
//!
//! `Consistency::Strict` was declared in the policy surface since the
//! guarantees existed and enforced nowhere — a promise without a mechanism.
//! It now means something: at commit, the read set gets the same
//! first-committer-wins validation the write set always had, which closes
//! the write-skew anomaly that plain snapshot isolation permits. The same
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
