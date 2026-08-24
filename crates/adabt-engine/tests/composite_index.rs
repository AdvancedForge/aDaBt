//! Composite indexes: one index over several fields, serving a predicate
//! that pins every one of them.
//!
//! The invariant that matters most is the same one every index kind in this
//! project has to keep: **creating or dropping one must never change an
//! answer.** A composite index is the first index here whose *selection*
//! depends on the shape of the predicate rather than on a single field
//! name, so getting the matching rule wrong would silently return the wrong
//! rows rather than merely being slower.

use adabt_core::ids::RecordId;
use adabt_core::policy::Policy;
use adabt_core::record::Record;
use adabt_core::schema::{FieldDef, FieldType, Schema, SchemaMode};
use adabt_core::store::LogicalStore;
use adabt_engine::Database;
use adabt_exec::physical::PhysicalOp;
use adabt_ir::plan::{LogicalOp, LogicalPlan};
use adabt_ir::{CmpOp, Expr};
use std::path::{Path, PathBuf};

struct Tmp(PathBuf);
impl Tmp {
    fn new(tag: &str) -> Self {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "adabt-composite-{tag}-{}-{:?}",
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
            FieldDef::new("country", FieldType::Str { max_len: Some(8) }),
            FieldDef::new("age", FieldType::I64),
        ],
    )
    .unwrap()
}

const COUNTRIES: [&str; 4] = ["NO", "SE", "DK", "FI"];

fn seeded(dir: &Path, n: u64) -> Database {
    let mut db = Database::open(dir, Policy::manual(0)).unwrap();
    db.create_collection("users", schema()).unwrap();
    for i in 0..n {
        db.insert(
            "users",
            RecordId(i),
            Record::new()
                .with("id", i)
                .with("country", COUNTRIES[(i % 4) as usize])
                .with("age", (i % 10) as i64),
        )
        .unwrap();
    }
    db
}

fn both_pinned() -> LogicalPlan {
    LogicalPlan::new(LogicalOp::scan("users").filter(Expr::And(vec![
        Expr::eq("country", "NO"),
        Expr::eq("age", 4i64),
    ])))
}

fn fields() -> Vec<String> {
    vec!["country".to_string(), "age".to_string()]
}

#[test]
fn a_composite_index_never_changes_the_answer() {
    let t = Tmp::new("invariance");
    let mut db = seeded(t.path(), 400);
    let before = db.query(&both_pinned()).unwrap();
    assert!(!before.is_empty(), "the fixture must actually match rows");

    db.create_composite_index("users", &fields()).unwrap();
    let after = db.query(&both_pinned()).unwrap();
    assert_eq!(before, after, "a composite index changed the answer");
}

#[test]
fn the_planner_chooses_it_when_every_field_is_pinned() {
    let t = Tmp::new("chosen");
    let mut db = seeded(t.path(), 400);
    db.create_composite_index("users", &fields()).unwrap();

    let plan = db.plan(&both_pinned());
    assert_eq!(
        plan.root.access_path().name(),
        "CompositeLookup",
        "{}",
        db.explain(&both_pinned())
    );
    // And it actually narrows: a full scan would touch every row.
    db.query(&both_pinned()).unwrap();
    assert!(
        db.last_exec_stats().rows_scanned < 400,
        "composite lookup scanned {} of 400 rows",
        db.last_exec_stats().rows_scanned
    );
}

#[test]
fn a_predicate_pinning_only_part_of_the_key_does_not_use_it() {
    // The correctness rule: a hash-backed composite index over (country,
    // age) cannot answer `country = 'NO'` alone. Using it would need a
    // prefix scan the structure cannot do, and picking it anyway would
    // return only the rows whose age happened to match nothing.
    let t = Tmp::new("partial");
    let mut db = seeded(t.path(), 400);
    let one_field = LogicalPlan::new(LogicalOp::scan("users").filter(Expr::eq("country", "NO")));
    let before = db.query(&one_field).unwrap();

    db.create_composite_index("users", &fields()).unwrap();
    let plan = db.plan(&one_field);
    assert_ne!(
        plan.root.access_path().name(),
        "CompositeLookup",
        "a composite index served a predicate that pins only one of its fields"
    );
    assert_eq!(db.query(&one_field).unwrap(), before);
}

#[test]
fn a_longer_composite_index_is_preferred_over_a_shorter_one() {
    // Both cover the predicate; the longer narrows harder, so choosing the
    // shorter would leave work for the residual filter that the index could
    // have done.
    let t = Tmp::new("longest");
    let mut db = seeded(t.path(), 400);
    db.create_composite_index("users", &fields()).unwrap();
    db.create_composite_index("users", &["country".into(), "age".into(), "id".into()])
        .unwrap();

    let all_three = LogicalPlan::new(LogicalOp::scan("users").filter(Expr::And(vec![
        Expr::eq("country", "NO"),
        Expr::eq("age", 4i64),
        Expr::eq("id", 4u64),
    ])));
    let plan = db.plan(&all_three);
    match plan.root.access_path() {
        PhysicalOp::CompositeLookup { fields, .. } => {
            assert_eq!(fields.len(), 3, "picked the shorter composite index");
        }
        other => panic!("expected a composite lookup, got {}", other.name()),
    }
}

#[test]
fn the_key_is_rebound_from_this_querys_literals_not_a_cached_one() {
    // The plan cache stores a shape-invariant decision; the literals must
    // come from the query being run. This is the composite equivalent of
    // the single-field rebinding test in the planner's own suite.
    let t = Tmp::new("rebind");
    let mut db = seeded(t.path(), 400);
    db.create_composite_index("users", &fields()).unwrap();

    let no4 = db.query(&both_pinned()).unwrap();
    let se7 = LogicalPlan::new(LogicalOp::scan("users").filter(Expr::And(vec![
        Expr::eq("country", "SE"),
        Expr::eq("age", 7i64),
    ])));
    let got = db.query(&se7).unwrap();

    assert!(!got.is_empty());
    assert_ne!(no4, got, "the second query reused the first query's key");
    for (_, r) in &got {
        assert_eq!(
            r.get("country"),
            Some(&adabt_core::value::Value::Str("SE".into()))
        );
        assert_eq!(r.get("age"), Some(&adabt_core::value::Value::I64(7)));
    }
}

#[test]
fn it_stays_correct_under_updates_and_deletes() {
    let t = Tmp::new("maintenance");
    let mut db = seeded(t.path(), 200);
    db.create_composite_index("users", &fields()).unwrap();

    let movers: Vec<RecordId> = db
        .query(&both_pinned())
        .unwrap()
        .iter()
        .map(|(i, _)| *i)
        .collect();
    assert!(!movers.is_empty());
    for id in &movers {
        let mut rec = db.get("users", *id).unwrap().unwrap();
        rec.set("country", "DK");
        db.update("users", *id, rec).unwrap();
    }
    assert!(
        db.query(&both_pinned()).unwrap().is_empty(),
        "stale composite entries survived an update"
    );

    let dk = LogicalPlan::new(LogicalOp::scan("users").filter(Expr::And(vec![
        Expr::eq("country", "DK"),
        Expr::eq("age", 4i64),
    ])));
    let before = db.query(&dk).unwrap().len();
    db.delete("users", movers[0]).unwrap();
    assert_eq!(db.query(&dk).unwrap().len(), before - 1);
}

#[test]
fn a_range_predicate_never_selects_a_hash_backed_composite_index() {
    let t = Tmp::new("range");
    let mut db = seeded(t.path(), 400);
    db.create_composite_index("users", &fields()).unwrap();
    let ranged = LogicalPlan::new(LogicalOp::scan("users").filter(Expr::And(vec![
        Expr::eq("country", "NO"),
        Expr::cmp("age", CmpOp::Gt, 4i64),
    ])));
    let before_rows = db.query(&ranged).unwrap();
    assert_ne!(
        db.plan(&ranged).root.access_path().name(),
        "CompositeLookup"
    );
    assert!(!before_rows.is_empty());
}

#[test]
fn one_field_is_refused_rather_than_silently_making_a_useless_index() {
    let t = Tmp::new("one-field");
    let mut db = seeded(t.path(), 10);
    assert!(db
        .create_composite_index("users", &["country".into()])
        .is_err());
}

#[test]
fn it_survives_a_restart() {
    let t = Tmp::new("restart");
    let expected = {
        let mut db = seeded(t.path(), 400);
        db.create_composite_index("users", &fields()).unwrap();
        db.query(&both_pinned()).unwrap()
    };
    let mut db = Database::open(t.path(), Policy::manual(0)).unwrap();
    assert_eq!(db.query(&both_pinned()).unwrap(), expected);
    assert_eq!(
        db.plan(&both_pinned()).root.access_path().name(),
        "CompositeLookup",
        "the composite index did not come back after a restart"
    );
}

// -- automatic selection ----------------------------------------------------

/// The composite index shipped in M25 and *nothing chose it*: it was reachable
/// only by naming it explicitly. The reason was not a missing heuristic — it
/// was a missing signal. Telemetry recorded how often each field was filtered
/// and never which fields were filtered *together*, and no amount of reasoning
/// over per-field counts can recover that. Two individually-hot fields are not
/// evidence that any query constrains both.
///
/// These prove the signal exists, reaches the optimizer, and results in an
/// index the planner then uses — end to end, through the real engine rather
/// than a fixture.
mod automatic {
    use super::*;
    use adabt_core::policy::Policy;

    fn hot(dir: &Path, pinned_together: bool) -> Database {
        let mut db = Database::open(dir, Policy::manual(5)).unwrap();
        db.create_collection("users", Schema::dynamic()).unwrap();
        for i in 0..2_000u64 {
            db.insert(
                "users",
                RecordId(i),
                Record::new()
                    .with("country", COUNTRIES[(i % 4) as usize])
                    .with("age", (20 + i % 50) as i64)
                    .with("name", format!("n{i}")),
            )
            .unwrap();
        }
        for i in 0..60u64 {
            let plan = if pinned_together {
                LogicalPlan::new(LogicalOp::scan("users").filter(Expr::And(vec![
                    Expr::eq("country", COUNTRIES[(i % 4) as usize]),
                    Expr::eq("age", (20 + i % 50) as i64),
                ])))
            } else if i % 2 == 0 {
                LogicalPlan::new(
                    LogicalOp::scan("users")
                        .filter(Expr::eq("country", COUNTRIES[(i % 4) as usize])),
                )
            } else {
                LogicalPlan::new(
                    LogicalOp::scan("users").filter(Expr::eq("age", (20 + i % 50) as i64)),
                )
            };
            db.query(&plan).unwrap();
        }
        db
    }

    /// Whether the engine actually built one.
    ///
    /// The end-to-end statement, not "did the optimizer consider it". A
    /// composite index answers to the NUL-joined name of its fields, so its
    /// presence in the catalog is unambiguous.
    fn built_composite(db: &Database) -> bool {
        db.index_specs()
            .iter()
            .any(|s| s.field.contains(adabt_index::COMPOSITE_SEP))
    }

    #[test]
    fn a_workload_that_pins_two_fields_together_makes_one_available() {
        let t = Tmp::new("auto-yes");
        let mut db = hot(t.path(), true);
        let report = db.optimize().unwrap();
        assert!(
            built_composite(&db),
            "sixty queries pinning both fields did not produce a composite \
             index; applied: {:?}, rejected: {:?}",
            report.applied,
            report.rejected
        );
    }

    /// The control, and the more important half. The same fields, the same
    /// number of queries, never constrained at the same time — a composite
    /// index would serve none of them.
    #[test]
    fn a_workload_that_filters_them_separately_does_not() {
        let t = Tmp::new("auto-no");
        let mut db = hot(t.path(), false);
        let report = db.optimize().unwrap();
        assert!(
            !built_composite(&db),
            "two separately-hot fields were read as evidence for a composite \
             index; applied: {:?}",
            report.applied
        );
    }
}
