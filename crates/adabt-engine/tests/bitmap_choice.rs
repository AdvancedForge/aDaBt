//! The bitmap-over-hash decision, made real by the cardinality signal.
//!
//! `adabt-bench index-scale` measured the two structures tying on lookup
//! latency at every scale tried while a low-cardinality bitmap held ~6% of
//! the memory — and the planner's old comment said a cardinality signal
//! "would reopen the question." The signal exists now (per-field key counts,
//! read O(1) from each index), so the question is reopened and settled: a
//! field proven small plans and executes through its bitmap; anything else
//! keeps the shipped hash-first order that cannot blow up. These tests pin
//! both halves end to end — through `Database::plan`, which is where real
//! cardinality from real indexes meets the rule.

use adabt_core::ids::RecordId;
use adabt_core::policy::Policy;
use adabt_core::record::Record;
use adabt_core::schema::{Schema, SchemaMode};
use adabt_core::store::LogicalStore;
use adabt_engine::Database;
use adabt_index::IndexKind;
use std::path::{Path, PathBuf};

struct Tmp(PathBuf);
impl Tmp {
    fn new(tag: &str) -> Self {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "adabt-bitmap-{tag}-{}-{:?}",
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

/// A collection with BOTH a hash and a bitmap index on `bucket`. Which one
/// serves an equality probe is the whole question.
fn dual_indexed(dir: &Path, rows: u64, distinct: u64) -> Database {
    let mut db = Database::open(dir, Policy::manual(4)).unwrap();
    let schema = Schema::new(
        SchemaMode::Dynamic,
        vec![
            adabt_core::schema::FieldDef::new("id", adabt_core::schema::FieldType::U64),
            adabt_core::schema::FieldDef::new("bucket", adabt_core::schema::FieldType::I64),
        ],
    )
    .unwrap();
    db.create_collection("events", schema).unwrap();
    // Hash first — creation order is the shipped tie-break, so this setup
    // makes any bitmap win a genuine reversal of the old behaviour.
    db.create_index("events", "bucket", IndexKind::Hash)
        .unwrap();
    db.create_index("events", "bucket", IndexKind::Bitmap)
        .unwrap();
    for i in 0..rows {
        db.insert(
            "events",
            RecordId(i),
            Record::new()
                .with("id", i)
                .with("bucket", (i % distinct) as i64),
        )
        .unwrap();
    }
    db
}

#[test]
fn a_low_cardinality_field_answers_through_its_bitmap() {
    let t = Tmp::new("low");
    let mut db = dual_indexed(t.path(), 2_000, 4);

    // End to end through the planner, with real key counts from real indexes.
    let logical = adabt_ir::plan::LogicalPlan::new(
        adabt_ir::plan::LogicalOp::scan("events").filter(adabt_ir::Expr::eq("bucket", 2i64)),
    );
    let explain = db.plan(&logical).explain();
    assert!(
        explain.contains("bitmap"),
        "low-cardinality equality should serve through the bitmap:\n{explain}"
    );

    // And execution answers exactly what the hash would have.
    let rows = db.query(&logical).unwrap();
    assert_eq!(rows.len(), 500);
    assert!(rows
        .iter()
        .all(|(_, r)| r.get("bucket") == Some(&adabt_core::value::Value::I64(2))));
}

#[test]
fn a_high_cardinality_field_keeps_hash_first_despite_a_bitmap_existing() {
    let t = Tmp::new("high");
    // 2,000 rows over 1,000 distinct values: far past the low-cardinality
    // bound, where a bitmap's footprint is no longer a bargain.
    let mut db = dual_indexed(t.path(), 2_000, 1_000);

    let logical = adabt_ir::plan::LogicalPlan::new(
        adabt_ir::plan::LogicalOp::scan("events").filter(adabt_ir::Expr::eq("bucket", 7i64)),
    );
    let explain = db.plan(&logical).explain();
    assert!(
        explain.contains("hash"),
        "high-cardinality equality should keep the hash-first order:\n{explain}"
    );
    assert_eq!(db.query(&logical).unwrap().len(), 2);
}

#[test]
fn answers_are_identical_whichever_structure_serves() {
    // The guarantee underneath the choice: both structures agree, row for
    // row, so switching between them by policy can never change an answer.
    let t = Tmp::new("agree");
    let mut db = dual_indexed(t.path(), 500, 5);
    let via_bitmap = {
        let logical = adabt_ir::plan::LogicalPlan::new(
            adabt_ir::plan::LogicalOp::scan("events").filter(adabt_ir::Expr::eq("bucket", 3i64)),
        );
        assert!(db.plan(&logical).explain().contains("bitmap"));
        db.query(&logical).unwrap()
    };
    let via_hash = {
        // Drop the bitmap; the same query falls back to the hash.
        assert!(db.drop_index("events", "bucket", IndexKind::Bitmap));
        let logical = adabt_ir::plan::LogicalPlan::new(
            adabt_ir::plan::LogicalOp::scan("events").filter(adabt_ir::Expr::eq("bucket", 3i64)),
        );
        assert!(db.plan(&logical).explain().contains("hash"));
        db.query(&logical).unwrap()
    };
    assert_eq!(via_bitmap, via_hash);
}
