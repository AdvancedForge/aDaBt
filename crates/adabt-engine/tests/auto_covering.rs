//! Automatic covering-index selection.
//!
//! A covering index removes the fetch from a lookup, and the fetch is the
//! measured majority of what a lookup costs here — so the structure matters
//! more than in most engines. It existed since M25 and nothing proposed it,
//! which is the shipped-but-unreachable pattern again: per-field filter
//! counts cannot show that the queries filtering `country` keep asking for
//! the same projection, any more than they could show that two fields were
//! filtered *together*. The signal is co-occurrence of a filtered field with
//! a stable projection; these tests pin that the proposal fires on stable
//! evidence, refuses unstable evidence, and that what it builds is the real
//! structure the planner already knows how to serve.

use adabt_core::ids::RecordId;
use adabt_core::policy::{Durability, Policy};
use adabt_core::record::Record;
use adabt_core::schema::Schema;
use adabt_core::store::LogicalStore;
use adabt_engine::Database;
use adabt_ir::plan::{LogicalOp, LogicalPlan};
use adabt_ir::Expr;
use std::path::{Path, PathBuf};

struct Tmp(PathBuf);
impl Tmp {
    fn new(tag: &str) -> Self {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "adabt-autocover-{tag}-{}-{:?}",
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

const N: u64 = 2_000;

fn open(tag: &str) -> (Tmp, Database) {
    let t = Tmp::new(tag);
    // Level 5 is where workload-aware proposals live; durability relaxed so
    // seeding does not dominate the test's runtime.
    let mut policy = Policy::manual(6);
    policy.guarantees.durability = Durability::Relaxed;
    let mut db = Database::open(t.path(), policy).unwrap();
    db.create_collection("users", Schema::dynamic()).unwrap();
    let batch: Vec<(RecordId, Record)> = (0..N)
        .map(|i| {
            (
                RecordId(i),
                Record::new()
                    .with("id", i)
                    .with("country", ["NO", "SE", "DK", "FI"][(i % 4) as usize])
                    .with("age", (18 + i % 60) as i64)
                    .with("name", format!("user-{i}")),
            )
        })
        .collect();
    db.insert_batch("users", batch).unwrap();
    (t, db)
}

/// The shape whose evidence proposes the index: equality on country,
/// projecting name and age.
fn eq_project() -> LogicalPlan {
    LogicalPlan::new(
        LogicalOp::scan("users")
            .filter(Expr::eq("country", "NO"))
            .project(vec!["name".into(), "age".into()]),
    )
}

/// A range-filtered field with a stable projection: the b-tree covering
/// question.
fn range_project() -> LogicalPlan {
    LogicalPlan::new(
        LogicalOp::scan("users")
            .filter(Expr::And(vec![
                Expr::cmp("age", adabt_ir::CmpOp::Ge, 30i64),
                Expr::cmp("age", adabt_ir::CmpOp::Lt, 35i64),
            ]))
            .project(vec!["name".into(), "id".into()]),
    )
}

fn spec_names(db: &Database) -> Vec<String> {
    // A spec's `field` is the name the index answers to, which for composite
    // and covering indexes is the NUL-joined encoding.
    db.index_specs().iter().map(|s| s.field.clone()).collect()
}

#[test]
fn a_stable_projection_beside_a_filtered_field_builds_a_covering_index() {
    let (_t, mut db) = open("stable");
    for _ in 0..25 {
        db.query(&eq_project()).unwrap();
    }
    db.optimize().unwrap();

    // The name is the canonical one — filtered field included among its
    // covered fields, sorted — because that is what create_covering_index
    // would have built for the same idea.
    let want = adabt_index::covering_name(
        "country",
        &["age".to_string(), "country".to_string(), "name".to_string()],
    );
    assert!(
        spec_names(&db).contains(&want),
        "no covering index was proposed; specs are {:?}",
        spec_names(&db)
    );

    // And the planner serves the shape through it — the whole point.
    let explain = db.plan(&eq_project()).explain();
    assert!(
        explain.contains("CoveringLookup"),
        "the covering index exists but the plan does not use it:\n{explain}"
    );
}

#[test]
fn an_unstable_projection_is_not_evidence() {
    let (_t, mut db) = open("unstable");
    // The same field, filtered just as often, but each query asks for a
    // different projection. No single projection is stable, so nothing
    // should be built for any of them.
    let rotations: Vec<Vec<&str>> = vec![
        vec!["name"],
        vec!["age"],
        vec!["name", "id"],
        vec!["id", "country"],
        vec!["age", "country"],
    ];
    for round in 0..25 {
        let fields = &rotations[round % rotations.len()];
        let plan = LogicalPlan::new(
            LogicalOp::scan("users")
                .filter(Expr::eq("country", "NO"))
                .project(fields.iter().map(|f| f.to_string()).collect()),
        );
        db.query(&plan).unwrap();
    }
    db.optimize().unwrap();

    let names = spec_names(&db);
    let covering: Vec<&String> = names.iter().filter(|n| n.contains('\u{1}')).collect();
    assert!(
        covering.is_empty(),
        "unstable projections still produced {covering:?}"
    );
}

#[test]
fn a_stable_projection_beside_a_range_filtered_field_builds_a_btree_covering() {
    let (_t, mut db) = open("range");
    for _ in 0..25 {
        db.query(&range_project()).unwrap();
    }
    db.optimize().unwrap();

    let want = adabt_index::covering_name(
        "age",
        &["age".to_string(), "id".to_string(), "name".to_string()],
    );
    assert!(
        spec_names(&db).contains(&want),
        "no covering index was proposed for the range-filtered field; specs are {:?}",
        spec_names(&db)
    );

    // The planner serves it through the range-capable covering path, and
    // the answer matches a plain scan of everything.
    let explain = db.plan(&range_project()).explain();
    assert!(
        explain.contains("CoveringRange"),
        "the b-tree covering index exists but the plan does not use it:\n{explain}"
    );
    let via_cover = db.query(&range_project()).unwrap();
    let all = LogicalPlan::new(
        LogicalOp::scan("users")
            .filter(Expr::And(vec![
                Expr::cmp("age", adabt_ir::CmpOp::Ge, 30i64),
                Expr::cmp("age", adabt_ir::CmpOp::Lt, 35i64),
            ]))
            .project(vec!["name".into()]),
    );
    let mut want_ids: Vec<RecordId> = db.query(&all).unwrap().iter().map(|(i, _)| *i).collect();
    let mut got_ids: Vec<RecordId> = via_cover.iter().map(|(i, _)| *i).collect();
    want_ids.sort_unstable();
    got_ids.sort_unstable();
    assert_eq!(got_ids, want_ids, "the covering range changed who matches");
}

#[test]
fn a_hash_backed_covering_index_is_never_chosen_for_a_range() {
    let (_t, mut db) = open("hash-range");
    // Build the EQUALITY evidence first: this proposes a hash-backed
    // covering index on country.
    for _ in 0..25 {
        db.query(&eq_project()).unwrap();
    }
    db.optimize().unwrap();
    let names = spec_names(&db);
    assert!(
        names.iter().any(|n| n.contains('\u{1}')),
        "setup failed: no covering index was built"
    );

    // Now ask a range question. A hash-backed covering index has no order
    // to walk; choosing it would be a silent empty answer rather than an
    // error, which is exactly why the matcher checks the backing kind.
    let explain = db.plan(&range_project()).explain();
    assert!(
        !explain.contains("CoveringRange"),
        "a hash-backed covering index served a range:\n{explain}"
    );
    // Whatever path answers it, the survivors must be the ones the
    // predicate selects over a plain scan.
    let got = db.query(&range_project()).unwrap();
    let mut got_ids: Vec<RecordId> = got.iter().map(|(i, _)| *i).collect();
    got_ids.sort_unstable();
    let all = LogicalPlan::new(
        LogicalOp::scan("users")
            .filter(Expr::And(vec![
                Expr::cmp("age", adabt_ir::CmpOp::Ge, 30i64),
                Expr::cmp("age", adabt_ir::CmpOp::Lt, 35i64),
            ]))
            .project(vec!["name".into(), "id".into()]),
    );
    let mut want_ids: Vec<RecordId> = db.query(&all).unwrap().iter().map(|(i, _)| *i).collect();
    want_ids.sort_unstable();
    assert_eq!(got_ids, want_ids, "the range answer changed");
}

#[test]
fn the_proposed_index_answers_exactly_what_the_heap_answers() {
    let (_t, mut db) = open("answers");
    for _ in 0..25 {
        db.query(&eq_project()).unwrap();
    }
    db.optimize().unwrap();

    // The answer through whatever path the planner picks now, against the
    // same question asked over a fresh scan of everything.
    let via_index = db.query(&eq_project()).unwrap();
    let plain = LogicalPlan::new(
        LogicalOp::scan("users")
            .filter(Expr::And(vec![
                Expr::eq("country", "NO"),
                Expr::IsNotNull(Box::new(Expr::field("name"))),
                Expr::IsNotNull(Box::new(Expr::field("age"))),
            ]))
            .project(vec!["name".into(), "age".into()]),
    );
    let via_heap = db.query(&plain).unwrap();
    let mut a: Vec<RecordId> = via_index.iter().map(|(i, _)| *i).collect();
    let mut b: Vec<RecordId> = via_heap.iter().map(|(i, _)| *i).collect();
    a.sort_unstable();
    b.sort_unstable();
    assert_eq!(a, b, "the covering index changed who matches");
}
