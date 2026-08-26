//! Multi-statement transactions: snapshot isolation, atomicity, and the
//! specific claim `crate::transaction` makes about why no shared commit
//! timestamp is needed for either.

use adabt_core::error::Error;
use adabt_core::ids::RecordId;
use adabt_core::policy::Policy;
use adabt_core::record::Record;
use adabt_core::schema::{FieldDef, FieldType, Schema, SchemaMode};
use adabt_core::store::LogicalStore;
use adabt_engine::Database;
use std::path::{Path, PathBuf};

struct Tmp(PathBuf);
impl Tmp {
    fn new(tag: &str) -> Self {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "adabt-txn-{tag}-{}-{:?}",
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

fn schema() -> Schema {
    Schema::new(
        SchemaMode::Strict,
        vec![
            FieldDef::new("id", FieldType::U64).required(),
            FieldDef::new("name", FieldType::Str { max_len: Some(32) }),
            FieldDef::new("balance", FieldType::I64),
        ],
    )
    .unwrap()
}

fn rec(i: u64, name: &str, balance: i64) -> Record {
    Record::new()
        .with("id", i)
        .with("name", name)
        .with("balance", balance)
}

fn open(dir: &Path) -> Database {
    let mut db = Database::open(dir, Policy::manual(0)).unwrap();
    db.create_collection("accounts", schema()).unwrap();
    db
}

#[test]
fn a_commit_makes_every_write_visible_at_once() {
    let t = Tmp::new("atomic-visibility");
    let mut db = open(t.path());
    let mut txn = db.begin();
    txn.insert(&mut db, "accounts", RecordId(1), rec(1, "a", 100))
        .unwrap();
    txn.insert(&mut db, "accounts", RecordId(2), rec(2, "b", 200))
        .unwrap();
    db.commit(txn).unwrap();

    assert_eq!(
        db.get("accounts", RecordId(1)).unwrap(),
        Some(rec(1, "a", 100))
    );
    assert_eq!(
        db.get("accounts", RecordId(2)).unwrap(),
        Some(rec(2, "b", 200))
    );
}

#[test]
fn a_reader_started_before_the_commit_sees_neither_write() {
    // The core snapshot-isolation claim: a transaction's writes are invisible
    // to anyone whose view predates the commit, in full — not "some of them",
    // which is exactly the partial-visibility failure the module's reasoning
    // about a shared commit timestamp argues cannot happen here.
    let t = Tmp::new("isolation");
    let mut db = open(t.path());
    let mut reader = db.begin();

    let mut writer = db.begin();
    writer
        .insert(&mut db, "accounts", RecordId(1), rec(1, "a", 100))
        .unwrap();
    writer
        .insert(&mut db, "accounts", RecordId(2), rec(2, "b", 200))
        .unwrap();
    db.commit(writer).unwrap();

    assert_eq!(reader.get(&mut db, "accounts", RecordId(1)).unwrap(), None);
    assert_eq!(reader.get(&mut db, "accounts", RecordId(2)).unwrap(), None);
    assert!(reader.scan(&mut db, "accounts").unwrap().is_empty());
}

#[test]
fn a_reader_begun_after_the_commit_sees_both_writes() {
    let t = Tmp::new("isolation-after");
    let mut db = open(t.path());

    let mut writer = db.begin();
    writer
        .insert(&mut db, "accounts", RecordId(1), rec(1, "a", 100))
        .unwrap();
    writer
        .insert(&mut db, "accounts", RecordId(2), rec(2, "b", 200))
        .unwrap();
    db.commit(writer).unwrap();

    let mut reader = db.begin();
    assert_eq!(
        reader.get(&mut db, "accounts", RecordId(1)).unwrap(),
        Some(rec(1, "a", 100))
    );
    assert_eq!(
        reader.get(&mut db, "accounts", RecordId(2)).unwrap(),
        Some(rec(2, "b", 200))
    );
    assert_eq!(reader.scan(&mut db, "accounts").unwrap().len(), 2);
}

#[test]
fn a_transaction_reads_its_own_writes() {
    let t = Tmp::new("ryow");
    let mut db = open(t.path());
    let mut txn = db.begin();
    assert_eq!(txn.get(&mut db, "accounts", RecordId(1)).unwrap(), None);
    txn.insert(&mut db, "accounts", RecordId(1), rec(1, "a", 100))
        .unwrap();
    assert_eq!(
        txn.get(&mut db, "accounts", RecordId(1)).unwrap(),
        Some(rec(1, "a", 100))
    );
    txn.update(&mut db, "accounts", RecordId(1), rec(1, "a", 150))
        .unwrap();
    assert_eq!(
        txn.get(&mut db, "accounts", RecordId(1)).unwrap().unwrap(),
        rec(1, "a", 150)
    );
    txn.delete(&mut db, "accounts", RecordId(1)).unwrap();
    assert_eq!(txn.get(&mut db, "accounts", RecordId(1)).unwrap(), None);

    // Uncommitted the whole time: the database itself never saw any of it.
    assert_eq!(db.get("accounts", RecordId(1)).unwrap(), None);
}

#[test]
fn read_your_own_writes_covers_scan_too() {
    let t = Tmp::new("ryow-scan");
    let mut db = open(t.path());
    db.insert("accounts", RecordId(0), rec(0, "zero", 0))
        .unwrap();

    let mut txn = db.begin();
    txn.insert(&mut db, "accounts", RecordId(1), rec(1, "a", 100))
        .unwrap();
    txn.delete(&mut db, "accounts", RecordId(0)).unwrap();

    let rows = txn.scan(&mut db, "accounts").unwrap();
    let ids: Vec<u64> = rows.iter().map(|(id, _)| id.0).collect();
    assert_eq!(
        ids,
        vec![1],
        "the scan did not reflect the buffered insert and delete"
    );

    // The database's own view is unaffected until commit.
    assert_eq!(db.scan("accounts").unwrap().len(), 1);
}

#[test]
fn insert_of_a_record_the_transaction_already_sees_fails_immediately() {
    let t = Tmp::new("insert-conflict-self");
    let mut db = open(t.path());
    db.insert("accounts", RecordId(1), rec(1, "a", 100))
        .unwrap();
    let mut txn = db.begin();
    let err = txn
        .insert(&mut db, "accounts", RecordId(1), rec(1, "dup", 0))
        .unwrap_err();
    assert!(matches!(err, Error::RecordExists(_)));
    assert!(txn.is_empty(), "the failed insert was buffered anyway");
}

#[test]
fn two_transactions_from_the_same_snapshot_the_second_committer_loses() {
    // First-committer-wins: both begin before either writes anything, so both
    // see the same snapshot; whichever calls `commit` first gets it, and the
    // other is told its view is stale rather than silently overwriting.
    let t = Tmp::new("conflict");
    let mut db = open(t.path());
    db.insert("accounts", RecordId(1), rec(1, "a", 100))
        .unwrap();

    let mut txn_a = db.begin();
    let mut txn_b = db.begin();
    txn_a
        .update(&mut db, "accounts", RecordId(1), rec(1, "a", 150))
        .unwrap();
    txn_b
        .update(&mut db, "accounts", RecordId(1), rec(1, "a", 999))
        .unwrap();

    db.commit(txn_a).unwrap();
    let err = db.commit(txn_b).unwrap_err();
    assert!(matches!(err, Error::TransactionConflict { .. }), "{err}");

    // The winner's write stands; the loser's is nowhere.
    assert_eq!(
        db.get("accounts", RecordId(1))
            .unwrap()
            .unwrap()
            .get("balance"),
        Some(&adabt_core::value::Value::I64(150))
    );
}

#[test]
fn a_conflicting_transaction_applies_none_of_its_other_writes_either() {
    // The conflict check runs over the whole write-set before anything is
    // applied, so a transaction that touches several keys and conflicts on one
    // of them must not land the others.
    let t = Tmp::new("conflict-partial");
    let mut db = open(t.path());
    db.insert("accounts", RecordId(1), rec(1, "a", 100))
        .unwrap();
    db.insert("accounts", RecordId(2), rec(2, "b", 200))
        .unwrap();

    // Both begin from the same snapshot, before either writes anything.
    let mut early = db.begin();
    let mut late = db.begin();

    early
        .update(&mut db, "accounts", RecordId(1), rec(1, "a", 500))
        .unwrap();
    db.commit(early).unwrap();

    // `late`'s snapshot predates that commit, so touching record 1 conflicts —
    // but it also touches record 2, which nobody else changed.
    late.update(&mut db, "accounts", RecordId(1), rec(1, "a", 999))
        .unwrap();
    late.update(&mut db, "accounts", RecordId(2), rec(2, "b", 999))
        .unwrap();
    assert!(matches!(
        db.commit(late).unwrap_err(),
        Error::TransactionConflict { .. }
    ));

    // Record 1 keeps the winner's value; record 2 was never touched by the
    // loser, even though it conflicted on nothing of its own.
    assert_eq!(
        db.get("accounts", RecordId(1))
            .unwrap()
            .unwrap()
            .get("balance"),
        Some(&adabt_core::value::Value::I64(500))
    );
    assert_eq!(
        db.get("accounts", RecordId(2))
            .unwrap()
            .unwrap()
            .get("balance"),
        Some(&adabt_core::value::Value::I64(200)),
        "the non-conflicting write in a rejected transaction was applied anyway"
    );
}

#[test]
fn disjoint_transactions_never_conflict() {
    // Different keys, same snapshot: both are first-committers of what they
    // actually touched, so both must succeed.
    let t = Tmp::new("disjoint");
    let mut db = open(t.path());
    db.insert("accounts", RecordId(1), rec(1, "a", 100))
        .unwrap();
    db.insert("accounts", RecordId(2), rec(2, "b", 200))
        .unwrap();

    let mut txn_a = db.begin();
    let mut txn_b = db.begin();
    txn_a
        .update(&mut db, "accounts", RecordId(1), rec(1, "a", 111))
        .unwrap();
    txn_b
        .update(&mut db, "accounts", RecordId(2), rec(2, "b", 222))
        .unwrap();

    db.commit(txn_a).unwrap();
    db.commit(txn_b).unwrap();
    assert_eq!(
        db.get("accounts", RecordId(1))
            .unwrap()
            .unwrap()
            .get("balance"),
        Some(&adabt_core::value::Value::I64(111))
    );
    assert_eq!(
        db.get("accounts", RecordId(2))
            .unwrap()
            .unwrap()
            .get("balance"),
        Some(&adabt_core::value::Value::I64(222))
    );
}

#[test]
fn a_schema_violation_anywhere_in_the_batch_commits_nothing() {
    let t = Tmp::new("schema-atomic");
    let mut db = open(t.path());
    let mut txn = db.begin();
    txn.insert(&mut db, "accounts", RecordId(1), rec(1, "good", 1))
        .unwrap();
    // A field the strict schema does not declare.
    txn.insert(
        &mut db,
        "accounts",
        RecordId(2),
        Record::new().with("id", 2u64).with("nope", "x"),
    )
    .unwrap();

    assert!(db.commit(txn).is_err());
    assert_eq!(
        db.get("accounts", RecordId(1)).unwrap(),
        None,
        "a valid write in a failed batch was applied"
    );
    assert_eq!(db.count("accounts").unwrap(), 0);
}

#[test]
fn a_unique_constraint_violation_anywhere_in_the_batch_commits_nothing() {
    let t = Tmp::new("unique-atomic");
    let mut db = open(t.path());
    db.add_unique_constraint("accounts", "name").unwrap();
    db.insert("accounts", RecordId(0), rec(0, "taken", 0))
        .unwrap();

    let mut txn = db.begin();
    txn.insert(&mut db, "accounts", RecordId(1), rec(1, "fresh", 1))
        .unwrap();
    txn.insert(&mut db, "accounts", RecordId(2), rec(2, "taken", 2))
        .unwrap();

    assert!(db.commit(txn).is_err());
    assert_eq!(db.get("accounts", RecordId(1)).unwrap(), None);
    assert_eq!(db.count("accounts").unwrap(), 1);
}

#[test]
fn a_committed_transaction_reindexes_correctly() {
    let t = Tmp::new("reindex");
    let mut db = open(t.path());
    db.create_index("accounts", "name", adabt_index::IndexKind::Hash)
        .unwrap();

    let mut txn = db.begin();
    txn.insert(&mut db, "accounts", RecordId(1), rec(1, "ada", 100))
        .unwrap();
    txn.insert(&mut db, "accounts", RecordId(2), rec(2, "grace", 200))
        .unwrap();
    db.commit(txn).unwrap();

    let plan = adabt_ir::plan::LogicalPlan::new(
        adabt_ir::plan::LogicalOp::scan("accounts").filter(adabt_ir::Expr::eq("name", "ada")),
    );
    let rows = db.query(&plan).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, RecordId(1));
}

#[test]
fn committing_an_empty_transaction_is_a_well_defined_no_op() {
    let t = Tmp::new("empty-commit");
    let mut db = open(t.path());
    let txn = db.begin();
    assert!(txn.is_empty());
    db.commit(txn).unwrap();
    assert_eq!(db.count("accounts").unwrap(), 0);
}

#[test]
fn abort_discards_everything_the_transaction_buffered() {
    let t = Tmp::new("abort");
    let mut db = open(t.path());
    let mut txn = db.begin();
    txn.insert(&mut db, "accounts", RecordId(1), rec(1, "a", 100))
        .unwrap();
    assert_eq!(txn.write_count(), 1);
    db.abort(txn);
    assert_eq!(db.count("accounts").unwrap(), 0);
    assert_eq!(db.get("accounts", RecordId(1)).unwrap(), None);
}

#[test]
fn dropping_a_transaction_without_committing_is_the_same_as_aborting() {
    let t = Tmp::new("drop");
    let mut db = open(t.path());
    {
        let mut txn = db.begin();
        txn.insert(&mut db, "accounts", RecordId(1), rec(1, "a", 100))
            .unwrap();
        // txn dropped here, never committed or explicitly aborted.
    }
    assert_eq!(db.count("accounts").unwrap(), 0);
}

#[test]
fn an_uncommitted_transaction_touches_no_durable_state_at_all() {
    // The module's central claim, checked the most direct way available: kill
    // the process (simulated by simply never calling commit and reopening) and
    // confirm the pre-transaction state is exactly what comes back — not
    // approximately, exactly, because nothing was ever written for a crash to
    // half-apply.
    let t = Tmp::new("crash-mid-transaction");
    {
        let mut db = open(t.path());
        db.insert("accounts", RecordId(0), rec(0, "before", 1))
            .unwrap();
        db.checkpoint().unwrap();
        let mut txn = db.begin();
        txn.insert(
            &mut db,
            "accounts",
            RecordId(1),
            rec(1, "never-committed", 999),
        )
        .unwrap();
        txn.update(&mut db, "accounts", RecordId(0), rec(0, "changed", 2))
            .unwrap();
        // No commit. Process "dies" here.
    }
    let mut db = Database::open(t.path(), Policy::manual(0)).unwrap();
    assert_eq!(db.count("accounts").unwrap(), 1);
    assert_eq!(
        db.get("accounts", RecordId(0)).unwrap(),
        Some(rec(0, "before", 1)),
        "an uncommitted transaction's write survived a restart"
    );
    assert_eq!(db.get("accounts", RecordId(1)).unwrap(), None);
}

#[test]
fn a_committed_transaction_survives_a_restart() {
    let t = Tmp::new("commit-restart");
    {
        let mut db = open(t.path());
        let mut txn = db.begin();
        txn.insert(&mut db, "accounts", RecordId(1), rec(1, "a", 100))
            .unwrap();
        txn.insert(&mut db, "accounts", RecordId(2), rec(2, "b", 200))
            .unwrap();
        db.commit(txn).unwrap();
        db.checkpoint().unwrap();
    }
    let mut db = Database::open(t.path(), Policy::manual(0)).unwrap();
    assert_eq!(db.count("accounts").unwrap(), 2);
    assert_eq!(
        db.get("accounts", RecordId(1)).unwrap(),
        Some(rec(1, "a", 100))
    );
    assert_eq!(
        db.get("accounts", RecordId(2)).unwrap(),
        Some(rec(2, "b", 200))
    );
}

#[test]
fn a_delete_inside_a_transaction_is_visible_after_commit() {
    let t = Tmp::new("delete-commit");
    let mut db = open(t.path());
    db.insert("accounts", RecordId(1), rec(1, "a", 100))
        .unwrap();
    let mut txn = db.begin();
    assert!(txn.delete(&mut db, "accounts", RecordId(1)).unwrap());
    db.commit(txn).unwrap();
    assert_eq!(db.get("accounts", RecordId(1)).unwrap(), None);
    assert_eq!(db.count("accounts").unwrap(), 0);
}

#[test]
fn a_delete_conflicting_with_a_concurrent_update_is_refused() {
    let t = Tmp::new("delete-conflict");
    let mut db = open(t.path());
    db.insert("accounts", RecordId(1), rec(1, "a", 100))
        .unwrap();

    let mut updater = db.begin();
    let mut deleter = db.begin();
    updater
        .update(&mut db, "accounts", RecordId(1), rec(1, "a", 150))
        .unwrap();
    deleter.delete(&mut db, "accounts", RecordId(1)).unwrap();

    db.commit(updater).unwrap();
    let err = db.commit(deleter).unwrap_err();
    assert!(matches!(err, Error::TransactionConflict { .. }));
    // The update stands; the record was not deleted out from under it.
    assert!(db.get("accounts", RecordId(1)).unwrap().is_some());
}

#[test]
fn many_sequential_transactions_leave_no_trace_of_the_aborted_ones() {
    let t = Tmp::new("sequential");
    let mut db = open(t.path());
    let mut committed = 0;
    for i in 0..50u64 {
        let mut txn = db.begin();
        txn.insert(&mut db, "accounts", RecordId(i), rec(i, "n", i as i64))
            .unwrap();
        if i % 3 == 0 {
            db.abort(txn);
        } else {
            db.commit(txn).unwrap();
            committed += 1;
        }
    }
    assert_eq!(db.count("accounts").unwrap(), committed);
    for i in 0..50u64 {
        let present = db.get("accounts", RecordId(i)).unwrap().is_some();
        assert_eq!(present, i % 3 != 0, "record {i}");
    }
}
