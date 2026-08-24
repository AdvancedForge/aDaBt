//! Bulk loading, and the fsync it saves.
//!
//! Loading a dataset was a first-hour activity with no first-hour answer: under
//! `Strict` durability, `insert` in a loop is one fsync per row, because that is
//! the only guarantee `insert` makes. `insert_batch` makes a different one —
//! everything in the batch, or nothing — which is what allows fsyncing once for
//! the whole batch instead of once per row.

use adabt_core::ids::RecordId;
use adabt_core::policy::Durability;
use adabt_core::record::Record;
use adabt_core::schema::Schema;
use adabt_core::store::LogicalStore;
use adabt_storage::heap::HeapStore;
use std::path::{Path, PathBuf};

struct Tmp(PathBuf);
impl Tmp {
    fn new(tag: &str) -> Self {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "adabt-batch-{tag}-{}-{:?}",
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

fn rec(i: u64) -> Record {
    Record::new().with("i", i).with("pad", "x".repeat(40))
}

fn batch(n: u64) -> Vec<(RecordId, Record)> {
    (0..n).map(|i| (RecordId(i), rec(i))).collect()
}

#[test]
fn a_batch_writes_everything_it_was_given() {
    let t = Tmp::new("basic");
    let mut h = HeapStore::open(t.path(), Durability::Strict, 64).unwrap();
    h.create_collection("c", Schema::dynamic()).unwrap();
    let n = h.insert_batch("c", batch(500)).unwrap();
    assert_eq!(n, 500);
    assert_eq!(h.count("c").unwrap(), 500);
    for i in 0..500u64 {
        assert_eq!(h.get("c", RecordId(i)).unwrap(), Some(rec(i)), "{i}");
    }
}

#[test]
fn a_batch_survives_a_restart() {
    let t = Tmp::new("restart");
    {
        let mut h = HeapStore::open(t.path(), Durability::Strict, 64).unwrap();
        h.create_collection("c", Schema::dynamic()).unwrap();
        h.insert_batch("c", batch(800)).unwrap();
    }
    let mut h = HeapStore::open(t.path(), Durability::Strict, 64).unwrap();
    assert_eq!(h.count("c").unwrap(), 800);
    for i in 0..800u64 {
        assert_eq!(h.get("c", RecordId(i)).unwrap(), Some(rec(i)), "{i}");
    }
}

#[test]
fn one_conflicting_id_fails_the_whole_batch() {
    // All-or-nothing: the caller must be able to trust that a failed batch
    // inserted nothing, or a retry would double-insert everything before the
    // conflict.
    let t = Tmp::new("conflict");
    let mut h = HeapStore::open(t.path(), Durability::Strict, 64).unwrap();
    h.create_collection("c", Schema::dynamic()).unwrap();
    h.insert("c", RecordId(50), rec(999)).unwrap();

    let err = h.insert_batch("c", batch(100)).unwrap_err();
    assert!(matches!(err, adabt_core::error::Error::RecordExists(_)));
    // Nothing from the batch landed, including the ids before the conflict.
    assert_eq!(h.count("c").unwrap(), 1);
    assert_eq!(h.get("c", RecordId(50)).unwrap(), Some(rec(999)));
    assert_eq!(h.get("c", RecordId(0)).unwrap(), None);
    assert_eq!(h.get("c", RecordId(99)).unwrap(), None);
}

#[test]
fn a_duplicate_id_within_one_batch_fails_it_entirely() {
    let t = Tmp::new("dup-in-batch");
    let mut h = HeapStore::open(t.path(), Durability::Strict, 64).unwrap();
    h.create_collection("c", Schema::dynamic()).unwrap();
    let mut b = batch(20);
    b.push((RecordId(5), rec(5)));
    let err = h.insert_batch("c", b).unwrap_err();
    assert!(matches!(err, adabt_core::error::Error::RecordExists(_)));
    assert_eq!(h.count("c").unwrap(), 0);
}

#[test]
fn an_invalid_record_fails_the_batch_before_anything_is_written() {
    use adabt_core::schema::{FieldDef, FieldType, SchemaMode};
    let t = Tmp::new("invalid");
    let mut h = HeapStore::open(t.path(), Durability::Strict, 64).unwrap();
    h.create_collection(
        "c",
        Schema::new(
            SchemaMode::Strict,
            vec![FieldDef::new("n", FieldType::I64).required()],
        )
        .unwrap(),
    )
    .unwrap();

    let mut b: Vec<(RecordId, Record)> = (0..10u64)
        .map(|i| (RecordId(i), Record::new().with("n", i as i64)))
        .collect();
    // One record with a field the schema does not declare.
    b.push((RecordId(99), Record::new().with("nope", "x")));

    assert!(h.insert_batch("c", b).is_err());
    assert_eq!(h.count("c").unwrap(), 0);
}

#[test]
fn a_crash_after_the_single_fsync_recovers_the_whole_batch() {
    // The property that makes one fsync for the whole batch safe: everything is
    // in the log before the sync, so a crash anywhere after it recovers all of
    // it, and a crash before it recovers none of it — never a partial batch.
    let t = Tmp::new("crash-after");
    {
        let mut h = HeapStore::open(t.path(), Durability::Strict, 64).unwrap();
        h.create_collection("c", Schema::dynamic()).unwrap();
        h.insert_batch("c", batch(300)).unwrap();
        // No checkpoint: durability comes entirely from the log's own fsync.
    }
    let mut h = HeapStore::open(t.path(), Durability::Strict, 64).unwrap();
    assert_eq!(h.count("c").unwrap(), 300);
}

#[test]
fn an_empty_batch_is_a_well_defined_no_op() {
    let t = Tmp::new("empty");
    let mut h = HeapStore::open(t.path(), Durability::Strict, 64).unwrap();
    h.create_collection("c", Schema::dynamic()).unwrap();
    assert_eq!(h.insert_batch("c", Vec::new()).unwrap(), 0);
    assert_eq!(h.count("c").unwrap(), 0);
}

#[test]
fn batched_inserts_answer_queries_identically_to_one_at_a_time() {
    // The differential idea applied narrowly: two stores built two different
    // ways from the same logical data must agree on everything.
    let a = Tmp::new("differential-a");
    let b = Tmp::new("differential-b");
    let mut individually = HeapStore::open(a.path(), Durability::Relaxed, 64).unwrap();
    let mut batched = HeapStore::open(b.path(), Durability::Relaxed, 64).unwrap();
    individually
        .create_collection("c", Schema::dynamic())
        .unwrap();
    batched.create_collection("c", Schema::dynamic()).unwrap();

    for (id, r) in batch(400) {
        individually.insert("c", id, r).unwrap();
    }
    batched.insert_batch("c", batch(400)).unwrap();

    assert_eq!(individually.scan("c").unwrap(), batched.scan("c").unwrap());
}

#[test]
fn a_batch_under_strict_durability_costs_one_sync_not_one_per_row() {
    let t = Tmp::new("sync-count");
    let mut h = HeapStore::open(t.path(), Durability::Strict, 64).unwrap();
    h.create_collection("c", Schema::dynamic()).unwrap();
    let before = h.sync_count();
    h.insert_batch("c", batch(1_000)).unwrap();
    let after = h.sync_count();
    assert_eq!(
        after - before,
        1,
        "a batch of 1000 rows caused {} syncs under Strict",
        after - before
    );
}

#[test]
fn a_thousand_row_batch_is_far_fewer_syscalls_than_a_thousand_single_inserts() {
    // Not a timing assertion — those are flaky under shared CI load — but the
    // sync count is exactly the resource the batch API exists to economise on,
    // and it is deterministic.
    let a = Tmp::new("cost-individual");
    let b = Tmp::new("cost-batch");
    let mut individually = HeapStore::open(a.path(), Durability::Strict, 64).unwrap();
    let mut batched = HeapStore::open(b.path(), Durability::Strict, 64).unwrap();
    individually
        .create_collection("c", Schema::dynamic())
        .unwrap();
    batched.create_collection("c", Schema::dynamic()).unwrap();
    // Baselines taken after collection creation, which itself syncs once, so
    // what is compared below is the cost of the inserts alone.
    let base_individually = individually.sync_count();
    let base_batched = batched.sync_count();

    for (id, r) in batch(1_000) {
        individually.insert("c", id, r).unwrap();
    }
    batched.insert_batch("c", batch(1_000)).unwrap();

    assert_eq!(individually.sync_count() - base_individually, 1_000);
    assert_eq!(batched.sync_count() - base_batched, 1);
}
