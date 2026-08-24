//! M25's index/layout additions.
//!
//! **Bitmap indexes.** A third `IndexKind`, alongside `Hash` and `BTree`:
//! same `Index` trait, same equality semantics, a bitmap instead of a
//! per-key `Vec<RecordId>` — worth reaching for on a low-cardinality field
//! with many rows per value, where `Hash`'s per-entry overhead adds up and a
//! bitmap's does not. `crates/adabt-index/src/lib.rs`'s own unit tests are
//! the evidence for the structure itself (word-boundary correctness,
//! removal, memory profile); these are the evidence that it is wired
//! correctly into the rest of the engine — chosen by the planner, persisted
//! and restorable, and, like every other index kind, provably unable to
//! change what a query answers.
//!
//! **Per-column dictionary encoding** is not new work here: `ColumnStore`
//! already does it for low-cardinality text columns (`column.rs`), and
//! already has its own passing test
//! (`dictionary_encoding_collapses_a_low_cardinality_column`). Nothing in
//! this file duplicates that; it is named in the M25 notes as already
//! satisfied, not rebuilt.

use adabt_core::ids::RecordId;
use adabt_core::policy::{Mode, Override, Policy};
use adabt_core::record::Record;
use adabt_core::schema::{FieldDef, FieldType, Schema, SchemaMode};
use adabt_core::store::LogicalStore;
use adabt_engine::Database;
use adabt_index::IndexKind;
use adabt_ir::plan::{LogicalOp, LogicalPlan};
use adabt_ir::Expr;
use std::path::{Path, PathBuf};

struct Tmp(PathBuf);
impl Tmp {
    fn new(tag: &str) -> Self {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "adabt-index-layout-{tag}-{}-{:?}",
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
            FieldDef::new("status", FieldType::Str { max_len: Some(16) }),
        ],
    )
    .unwrap()
}

const STATUSES: [&str; 3] = ["active", "closed", "pending"];

fn seeded(dir: &Path, n: u64) -> Database {
    let mut db = Database::open(dir, Policy::manual(0)).unwrap();
    db.create_collection("orders", schema()).unwrap();
    for i in 0..n {
        db.insert(
            "orders",
            RecordId(i),
            Record::new()
                .with("id", i)
                .with("status", STATUSES[(i % 3) as usize]),
        )
        .unwrap();
    }
    db
}

fn manual_with(overrides: Vec<Override>) -> Policy {
    Policy {
        mode: Mode::Manual {
            level: 4,
            overrides,
        },
        ..Policy::conventional()
    }
}

fn eq_plan() -> LogicalPlan {
    LogicalPlan::new(LogicalOp::scan("orders").filter(Expr::eq("status", "active")))
}

#[test]
fn a_bitmap_index_answers_equality_correctly() {
    let t = Tmp::new("answers");
    let mut db = seeded(t.path(), 300);
    let before = db.query(&eq_plan()).unwrap();

    db.create_index("orders", "status", IndexKind::Bitmap)
        .unwrap();
    let after = db.query(&eq_plan()).unwrap();

    assert_eq!(before.len(), 100, "one third of 300 rows");
    assert_eq!(before, after, "an index must never change the answer");
}

#[test]
fn the_planner_uses_the_bitmap_index_when_it_is_the_only_one() {
    let t = Tmp::new("planner");
    let mut db = seeded(t.path(), 300);
    db.create_index("orders", "status", IndexKind::Bitmap)
        .unwrap();

    assert!(
        !db.plan(&eq_plan()).is_full_scan(),
        "{}",
        db.explain(&eq_plan())
    );
    let stats_before = db.last_exec_stats();
    db.query(&eq_plan()).unwrap();
    let stats_after = db.last_exec_stats();
    let _ = stats_before;
    assert!(
        stats_after.rows_scanned < 300,
        "an equality query with a usable index should not scan every row: {}",
        stats_after.rows_scanned
    );
}

#[test]
fn a_bitmap_index_survives_a_restart_uncached() {
    // Same shape as `derived_cache.rs`'s
    // `an_index_added_after_the_last_checkpoint_is_still_there_after_a_restart`
    // — the definition is in the log even though the contents never reached
    // a checkpoint, so recovery has to rebuild it, and the rebuild has to
    // reconstruct the same `Bitmap` kind, not silently fall back to another.
    let t = Tmp::new("restart");
    let mut db = seeded(t.path(), 300);
    db.create_index("orders", "status", IndexKind::Bitmap)
        .unwrap();
    let expected = db.query(&eq_plan()).unwrap();
    drop(db);

    let mut db = Database::open(t.path(), Policy::manual(0)).unwrap();
    let specs = db.index_specs();
    assert_eq!(specs.len(), 1);
    assert_eq!(specs[0].kind, IndexKind::Bitmap);
    assert_eq!(db.query(&eq_plan()).unwrap(), expected);
}

#[test]
fn dropping_a_bitmap_index_does_not_change_the_answer() {
    let t = Tmp::new("drop");
    let mut db = seeded(t.path(), 300);
    let before = db.query(&eq_plan()).unwrap();

    db.create_index("orders", "status", IndexKind::Bitmap)
        .unwrap();
    assert_eq!(db.query(&eq_plan()).unwrap(), before);

    db.drop_index("orders", "status", IndexKind::Bitmap);
    assert_eq!(db.query(&eq_plan()).unwrap(), before);
}

#[test]
fn creating_the_same_bitmap_index_twice_is_a_no_op() {
    let t = Tmp::new("idempotent");
    let mut db = seeded(t.path(), 50);
    db.create_index("orders", "status", IndexKind::Bitmap)
        .unwrap();
    db.create_index("orders", "status", IndexKind::Bitmap)
        .unwrap();
    assert_eq!(db.index_specs().len(), 1);
}

#[test]
fn a_bitmap_index_stays_correct_under_updates_and_deletes() {
    let t = Tmp::new("maintenance");
    let mut db = seeded(t.path(), 200);
    db.create_index("orders", "status", IndexKind::Bitmap)
        .unwrap();

    let movers: Vec<RecordId> = db
        .query(&eq_plan())
        .unwrap()
        .iter()
        .map(|(i, _)| *i)
        .collect();
    for id in &movers {
        let mut rec = db.get("orders", *id).unwrap().unwrap();
        rec.set("status", "closed");
        db.update("orders", *id, rec).unwrap();
    }
    assert!(
        db.query(&eq_plan()).unwrap().is_empty(),
        "stale bitmap entries survived an update"
    );

    let closed = LogicalPlan::new(LogicalOp::scan("orders").filter(Expr::eq("status", "closed")));
    let before = db.query(&closed).unwrap().len();
    for id in movers.iter().take(10) {
        db.delete("orders", *id).unwrap();
    }
    assert_eq!(db.query(&closed).unwrap().len(), before - 10);
}

#[test]
fn a_dictionary_encoded_column_store_answers_exactly_as_the_heap_does() {
    // Per-column dictionary encoding is one of M25's six named features and
    // it already existed (`column.rs`'s `Column::Dict`), so this is
    // confirmation, not new coverage. But it has to actually confirm
    // something: an earlier version of this test computed a result, called
    // `optimize()`, recomputed it, and asserted the two were equal — which
    // passes whether or not the column store engages at all, and would pass
    // against a completely broken dictionary. The audit that caught that was
    // right; this version forces the column store on explicitly and compares
    // against a database that does not have it, so the two answers can only
    // agree if dictionary-encoded reads are genuinely faithful.
    let plan = || {
        LogicalPlan::new(
            LogicalOp::scan("orders")
                .aggregate(vec!["status".into()], vec![adabt_ir::plan::Agg::count("n")]),
        )
    };

    let heap_only = Tmp::new("dict-heap");
    let mut plain = seeded(heap_only.path(), 2_500);
    let via_heap = plain.query(&plan()).unwrap();

    let columnar = Tmp::new("dict-columnar");
    let mut with_columns = Database::open(
        columnar.path(),
        manual_with(vec![Override::toggle("column_store", true)]),
    )
    .unwrap();
    with_columns.create_collection("orders", schema()).unwrap();
    for i in 0..2_500u64 {
        with_columns
            .insert(
                "orders",
                RecordId(i),
                Record::new()
                    .with("id", i)
                    .with("status", STATUSES[(i % 3) as usize]),
            )
            .unwrap();
    }
    with_columns.optimize().unwrap();
    assert!(
        with_columns.column_store_collections() > 0,
        "the column store did not engage, so this test would prove nothing"
    );

    let via_columns = with_columns.query(&plan()).unwrap();
    assert_eq!(
        via_heap, via_columns,
        "a dictionary-encoded column answered differently from the heap"
    );
    // And the status column is exactly the low-cardinality shape dictionary
    // encoding exists for: 2,500 rows over 3 distinct strings.
    assert_eq!(via_heap.len(), 3);
}
