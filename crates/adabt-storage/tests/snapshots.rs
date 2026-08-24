//! Snapshot isolation: the prerequisite for comparing two representations
//! against the same state.

use adabt_core::ids::RecordId;
use adabt_core::policy::Durability;
use adabt_core::record::Record;
use adabt_core::schema::{FieldDef, FieldType, Schema, SchemaMode};
use adabt_core::store::LogicalStore;
use adabt_core::value::Value;
use adabt_storage::heap::HeapStore;
use std::path::{Path, PathBuf};

struct Tmp(PathBuf);
impl Tmp {
    fn new(tag: &str) -> Self {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "adabt-snap-{tag}-{}-{:?}",
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
        SchemaMode::Fixed,
        vec![
            FieldDef::new("id", FieldType::U64).required(),
            FieldDef::new("v", FieldType::I64).required(),
        ],
    )
    .unwrap()
}

fn rec(id: u64, v: i64) -> Record {
    Record::new().with("id", id).with("v", v)
}

fn store(dir: &Path, n: u64) -> HeapStore {
    let mut h = HeapStore::open(dir, Durability::Relaxed, 256).unwrap();
    h.create_collection("c", schema()).unwrap();
    for i in 0..n {
        h.insert("c", RecordId(i), rec(i, i as i64)).unwrap();
    }
    h
}

#[test]
fn a_snapshot_does_not_see_writes_made_after_it() {
    let t = Tmp::new("isolation");
    let mut h = store(t.path(), 100);
    let snap = h.snapshot();

    for i in 0..100u64 {
        h.update("c", RecordId(i), rec(i, 9_999)).unwrap();
    }

    for i in 0..100u64 {
        assert_eq!(
            h.get_at("c", RecordId(i), &snap).unwrap(),
            Some(rec(i, i as i64)),
            "record {i} changed under an open snapshot"
        );
        // While a read at "now" sees the update.
        assert_eq!(
            h.get("c", RecordId(i)).unwrap().unwrap().get("v"),
            Some(&Value::I64(9_999))
        );
    }
}

#[test]
fn a_snapshot_does_not_see_deletes_made_after_it() {
    let t = Tmp::new("delete");
    let mut h = store(t.path(), 50);
    let snap = h.snapshot();

    for i in 0..50u64 {
        h.delete("c", RecordId(i)).unwrap();
    }
    assert_eq!(h.count("c").unwrap(), 0);

    assert_eq!(h.scan_at("c", &snap).unwrap().len(), 50);
    assert_eq!(h.get_at("c", RecordId(7), &snap).unwrap(), Some(rec(7, 7)));
}

#[test]
fn a_snapshot_does_not_see_inserts_made_after_it() {
    let t = Tmp::new("insert");
    let mut h = store(t.path(), 10);
    let snap = h.snapshot();
    for i in 10..30u64 {
        h.insert("c", RecordId(i), rec(i, i as i64)).unwrap();
    }
    assert_eq!(h.scan_at("c", &snap).unwrap().len(), 10);
    assert_eq!(h.count("c").unwrap(), 30);
}

#[test]
fn a_scan_under_a_snapshot_is_stable_while_the_data_churns() {
    // The property shadow execution needs: two reads of the same snapshot
    // return the same thing however much moves in between.
    let t = Tmp::new("stable");
    let mut h = store(t.path(), 200);
    let snap = h.snapshot();
    let first = h.scan_at("c", &snap).unwrap();

    for round in 0..5 {
        for i in 0..200u64 {
            if i % 3 == 0 {
                h.delete("c", RecordId(i)).unwrap();
            } else {
                h.update("c", RecordId(i), rec(i, round * 1000 + i as i64))
                    .unwrap();
            }
        }
        for i in 0..200u64 {
            if i % 3 == 0 {
                let _ = h.insert("c", RecordId(i), rec(i, -1));
            }
        }
    }

    assert_eq!(
        h.scan_at("c", &snap).unwrap(),
        first,
        "the snapshot moved under a churning workload"
    );
}

#[test]
fn two_snapshots_taken_at_different_times_see_different_states() {
    let t = Tmp::new("two");
    let mut h = store(t.path(), 20);
    let early = h.snapshot();
    for i in 0..20u64 {
        h.update("c", RecordId(i), rec(i, 111)).unwrap();
    }
    let late = h.snapshot();
    for i in 0..20u64 {
        h.update("c", RecordId(i), rec(i, 222)).unwrap();
    }

    let value = |snap: &adabt_storage::version::Snapshot, h: &mut HeapStore| {
        h.get_at("c", RecordId(0), snap)
            .unwrap()
            .unwrap()
            .get("v")
            .cloned()
    };
    assert_eq!(value(&early, &mut h), Some(Value::I64(0)));
    assert_eq!(value(&late, &mut h), Some(Value::I64(111)));
    assert_eq!(
        h.get("c", RecordId(0)).unwrap().unwrap().get("v"),
        Some(&Value::I64(222))
    );
}

#[test]
fn reclamation_is_blocked_while_a_snapshot_is_open_and_proceeds_after() {
    let t = Tmp::new("reclaim");
    let mut h = store(t.path(), 100);
    let snap = h.snapshot();
    for i in 0..100u64 {
        h.update("c", RecordId(i), rec(i, 1)).unwrap();
    }
    let retained_with_snapshot = h.retained_versions();
    assert!(
        retained_with_snapshot > 0,
        "no versions were retained at all"
    );

    // Nothing the open snapshot might need may be freed.
    h.reclaim().unwrap();
    assert_eq!(
        h.scan_at("c", &snap).unwrap().len(),
        100,
        "reclamation ran while a snapshot needed the versions"
    );

    drop(snap);
    let freed = h.reclaim().unwrap();
    assert!(freed > 0, "closing the snapshot did not release anything");
    assert!(h.retained_versions() < retained_with_snapshot);
    // And the live data is untouched.
    assert_eq!(h.count("c").unwrap(), 100);
    assert_eq!(
        h.get("c", RecordId(50)).unwrap().unwrap().get("v"),
        Some(&Value::I64(1))
    );
}

#[test]
fn reclaimed_space_is_reused_rather_than_leaked() {
    let t = Tmp::new("space");
    let mut h = store(t.path(), 500);
    let before = h.page_count();
    for _ in 0..8 {
        for i in 0..500u64 {
            h.update("c", RecordId(i), rec(i, 7)).unwrap();
        }
        h.reclaim().unwrap();
    }
    assert!(
        h.page_count() < before * 4,
        "versioned updates leaked pages: {} grew to {}",
        before,
        h.page_count()
    );
    assert_eq!(h.count("c").unwrap(), 500);
}

#[test]
fn versions_do_not_survive_a_restart() {
    // No snapshot spans a restart, so recovery rebuilds single-version chains
    // and the retained history is correctly gone.
    let t = Tmp::new("restart");
    {
        let mut h = store(t.path(), 50);
        let _snap = h.snapshot();
        for i in 0..50u64 {
            h.update("c", RecordId(i), rec(i, 5)).unwrap();
        }
        assert!(h.retained_versions() > 0);
    }
    let mut h = HeapStore::open(t.path(), Durability::Relaxed, 256).unwrap();
    assert_eq!(h.retained_versions(), 0);
    assert_eq!(h.count("c").unwrap(), 50);
    assert_eq!(
        h.get("c", RecordId(1)).unwrap().unwrap().get("v"),
        Some(&Value::I64(5))
    );
}

#[test]
fn a_record_inserted_and_deleted_under_a_snapshot_is_invisible_to_it() {
    let t = Tmp::new("churn");
    let mut h = store(t.path(), 5);
    let snap = h.snapshot();
    h.insert("c", RecordId(99), rec(99, 1)).unwrap();
    h.delete("c", RecordId(99)).unwrap();
    assert_eq!(h.get_at("c", RecordId(99), &snap).unwrap(), None);
    assert_eq!(h.scan_at("c", &snap).unwrap().len(), 5);
}

#[test]
fn reinserting_a_deleted_id_is_visible_at_the_right_snapshots() {
    let t = Tmp::new("reinsert");
    let mut h = store(t.path(), 3);
    let before_delete = h.snapshot();
    h.delete("c", RecordId(1)).unwrap();
    let after_delete = h.snapshot();
    h.insert("c", RecordId(1), rec(1, 42)).unwrap();
    let after_reinsert = h.snapshot();

    assert_eq!(
        h.get_at("c", RecordId(1), &before_delete).unwrap(),
        Some(rec(1, 1))
    );
    assert_eq!(h.get_at("c", RecordId(1), &after_delete).unwrap(), None);
    assert_eq!(
        h.get_at("c", RecordId(1), &after_reinsert).unwrap(),
        Some(rec(1, 42))
    );
}
