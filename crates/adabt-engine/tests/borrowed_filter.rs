//! The borrowed-view filter, end to end.
//!
//! A single-field predicate sitting directly over a heap scan no longer
//! decodes every row to decide it: the executor peeks one field per id —
//! an address calculation when the collection has a direct array — and
//! fetches in full only the rows that survive. These tests pin the two
//! properties that make the substitution safe: the answers are exactly what
//! a full decode would give (including absent fields, nulls, and deleted
//! rows), and the fast path actually engages where it should.

use adabt_core::ids::RecordId;
use adabt_core::policy::Policy;
use adabt_core::record::Record;
use adabt_core::schema::{FieldDef, FieldType, Schema, SchemaMode};
use adabt_core::store::LogicalStore;
use adabt_engine::Database;
use adabt_ir::plan::{LogicalOp, LogicalPlan};
use adabt_ir::{CmpOp, Expr};
use std::path::{Path, PathBuf};

struct Tmp(PathBuf);
impl Tmp {
    fn new(tag: &str) -> Self {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "adabt-peek-{tag}-{}-{:?}",
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

/// A fixed-schema collection large enough that level 10 builds a direct
/// array for it, with values chosen so a single-field equality keeps a
/// known minority of rows.
fn fixed_collection(dir: &Path) -> Database {
    let mut db = Database::open(dir, Policy::conventional()).unwrap();
    let schema = Schema::new(
        SchemaMode::Fixed,
        vec![
            FieldDef::new("id", FieldType::U64).required(),
            FieldDef::new("bucket", FieldType::I64),
        ],
    )
    .unwrap();
    db.create_collection("events", schema).unwrap();
    for i in 0..2_000u64 {
        db.insert(
            "events",
            RecordId(i),
            Record::new().with("id", i).with("bucket", (i % 7) as i64),
        )
        .unwrap();
    }
    db.set_level(10).unwrap();
    assert!(db.has_direct_array("events"), "no direct array to peek");
    db
}

#[test]
fn a_single_field_filter_answers_exactly_what_a_full_decode_would() {
    let t = Tmp::new("equiv");
    let mut db = fixed_collection(t.path());

    let fused = db
        .query(&LogicalPlan::new(
            LogicalOp::scan("events").filter(Expr::eq("bucket", 3i64)),
        ))
        .unwrap();

    // The same query against the same data through the generic path:
    // a two-field predicate defeats the fusion and forces whole decodes.
    let generic = db
        .query(&LogicalPlan::new(LogicalOp::scan("events").filter(
            Expr::And(vec![
                Expr::eq("bucket", 3i64),
                Expr::cmp("id", CmpOp::Lt, 10_000i64),
            ]),
        )))
        .unwrap();

    assert_eq!(
        fused.len(),
        (0..2_000u64).filter(|i| i % 7 == 3).count(),
        "wrong rows kept"
    );
    assert_eq!(fused, generic, "the fused filter changed the answer");
    assert!(fused
        .iter()
        .all(|(_, r)| r.get("bucket") == Some(&adabt_core::value::Value::I64(3))));
}

#[test]
fn is_null_over_a_dynamic_collection_treats_absent_and_null_alike() {
    let t = Tmp::new("isnull");
    let mut db = Database::open(t.path(), Policy::conventional()).unwrap();
    db.create_collection("docs", Schema::dynamic()).unwrap();
    db.insert("docs", RecordId(0), Record::new().with("tag", Value::Null))
        .unwrap();
    db.insert("docs", RecordId(1), Record::new().with("other", 1i64))
        .unwrap();
    db.insert("docs", RecordId(2), Record::new().with("tag", "set"))
        .unwrap();
    db.insert("docs", RecordId(3), Record::new().with("tag", 9i64))
        .unwrap();
    db.delete("docs", RecordId(3)).unwrap();

    let rows = db
        .query(&LogicalPlan::new(
            LogicalOp::scan("docs").filter(Expr::IsNull(Box::new(Expr::field("tag")))),
        ))
        .unwrap();
    let ids: Vec<u64> = rows.iter().map(|(id, _)| id.0).collect();
    assert_eq!(ids, vec![0, 1], "null and absent match; dead rows do not");

    let not_null = db
        .query(&LogicalPlan::new(
            LogicalOp::scan("docs").filter(Expr::IsNotNull(Box::new(Expr::field("tag")))),
        ))
        .unwrap();
    let ids: Vec<u64> = not_null.iter().map(|(id, _)| id.0).collect();
    assert_eq!(ids, vec![2]);
}

use adabt_core::value::Value;
