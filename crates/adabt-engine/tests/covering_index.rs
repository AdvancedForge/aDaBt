//! Covering indexes: answering a query without reading the records.
//!
//! An ordinary index answers "which ids" and the executor fetches each one. A
//! covering index stores the fields the query needs beside the id, so the
//! fetch never happens. That matters more here than in most engines: this
//! project measured its own fetch path and found it to be the *majority* of
//! what a lookup costs, so removing it removes the dominant term rather than
//! trimming a constant.
//!
//! Three things have to be true, and each gets a test that would fail without
//! it:
//!
//! 1. The rows are identical to what the heap would have returned.
//! 2. The heap is genuinely not read — asserted in page reads, not in time.
//! 3. The index is refused when its projection does not contain everything the
//!    query needs, rather than silently serving a partial row.

use adabt_core::ids::RecordId;
use adabt_core::index_kind::IndexKind;
use adabt_core::policy::Policy;
use adabt_core::record::Record;
use adabt_core::schema::Schema;
use adabt_core::store::LogicalStore;
use adabt_core::value::Value;
use adabt_engine::Database;
use adabt_ir::plan::{LogicalOp, LogicalPlan};
use adabt_ir::Expr;
use std::path::{Path, PathBuf};

struct Tmp(PathBuf);
impl Tmp {
    fn new(tag: &str) -> Self {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "adabt-covering-{tag}-{}-{:?}",
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

const N: u64 = 1_000;
const COUNTRIES: [&str; 4] = ["NO", "SE", "DK", "FI"];

fn seeded(dir: &Path) -> Database {
    let mut db = Database::open(dir, Policy::manual(0)).unwrap();
    db.create_collection("users", Schema::dynamic()).unwrap();
    for i in 0..N {
        db.insert(
            "users",
            RecordId(i),
            Record::new()
                .with("country", COUNTRIES[(i % 4) as usize])
                .with("city", format!("city-{}", i % 50))
                .with("age", (20 + i % 60) as i64)
                .with("bio", "x".repeat(80)),
        )
        .unwrap();
    }
    db
}

/// `SELECT city, age FROM users WHERE country = 'NO'`
fn city_age_in_norway() -> LogicalPlan {
    LogicalPlan::new(
        LogicalOp::scan("users")
            .filter(Expr::eq("country", "NO"))
            .project(vec!["city".into(), "age".into()]),
    )
}

fn page_gets(db: &Database) -> u64 {
    let s = db.buffer_stats();
    s.hits + s.misses
}

fn covers() -> Vec<String> {
    vec!["city".into(), "age".into()]
}

#[test]
fn a_covering_index_returns_exactly_what_the_heap_would_have() {
    let dir = Tmp::new("same");
    let mut db = seeded(dir.path());
    let plan = city_age_in_norway();

    let without = db.query(&plan).unwrap();
    db.create_covering_index("users", "country", &covers(), IndexKind::Hash)
        .unwrap();
    let with = db.query(&plan).unwrap();

    assert!(!without.is_empty(), "the fixture must produce rows");
    assert_eq!(
        with, without,
        "a covering index changed the answer — rows, order or contents"
    );
}

/// The claim, stated in the only unit that cannot drift with the machine.
#[test]
fn a_covering_query_does_not_read_the_heap_at_all() {
    let dir = Tmp::new("nofetch");
    let mut db = seeded(dir.path());
    db.create_covering_index("users", "country", &covers(), IndexKind::Hash)
        .unwrap();
    let plan = city_age_in_norway();

    // Warm, so plan-cache installation is not part of the measurement.
    db.query(&plan).unwrap();

    let before = page_gets(&db);
    let rows = db.query(&plan).unwrap();
    let cost = page_gets(&db) - before;

    assert_eq!(rows.len(), (N / 4) as usize);
    assert_eq!(
        cost, 0,
        "a covering query read {cost} pages; the whole point is that it reads none"
    );
}

#[test]
fn the_planner_actually_chooses_it() {
    let dir = Tmp::new("chosen");
    let mut db = seeded(dir.path());
    db.create_covering_index("users", "country", &covers(), IndexKind::Hash)
        .unwrap();
    let explain = db.explain(&city_age_in_norway());
    assert!(
        explain.contains("CoveringLookup"),
        "the planner did not choose the covering index:\n{explain}"
    );
}

/// A projection that does not contain everything the query reads is not a
/// partial answer to be topped up — it is not an answer. Refusing sends the
/// query down the ordinary path, which is slower and right.
#[test]
fn an_index_that_does_not_cover_everything_is_refused() {
    let dir = Tmp::new("partial");
    let mut db = seeded(dir.path());
    // Covers `city` but the query also wants `age`.
    db.create_covering_index("users", "country", &["city".to_string()], IndexKind::Hash)
        .unwrap();

    let plan = city_age_in_norway();
    let explain = db.explain(&plan);
    assert!(
        !explain.contains("CoveringLookup"),
        "an index missing a needed field was chosen anyway:\n{explain}"
    );

    let rows = db.query(&plan).unwrap();
    assert_eq!(rows.len(), (N / 4) as usize);
    for (_, r) in &rows {
        assert!(r.get("age").is_some(), "a row came back without its age");
    }
}

/// A query returning whole records cannot be served from a projection at any
/// size, and the planner must not try.
#[test]
fn a_query_wanting_whole_records_never_uses_a_covering_index() {
    let dir = Tmp::new("whole");
    let mut db = seeded(dir.path());
    db.create_covering_index("users", "country", &covers(), IndexKind::Hash)
        .unwrap();

    // No projection: every field escapes, including `bio`, which is covered by
    // nothing.
    let plan = LogicalPlan::new(LogicalOp::scan("users").filter(Expr::eq("country", "NO")));
    let explain = db.explain(&plan);
    assert!(
        !explain.contains("CoveringLookup"),
        "whole records were served from a projection:\n{explain}"
    );
    let rows = db.query(&plan).unwrap();
    assert!(rows.iter().all(|(_, r)| r.get("bio").is_some()));
}

/// Writes have to reach the projection, not just the key. This is the extra
/// cost a covering index carries, and getting it wrong serves stale data
/// rather than failing.
#[test]
fn updating_a_covered_field_updates_the_projection() {
    let dir = Tmp::new("update");
    let mut db = seeded(dir.path());
    db.create_covering_index("users", "country", &covers(), IndexKind::Hash)
        .unwrap();

    db.update(
        "users",
        RecordId(0),
        Record::new()
            .with("country", "NO")
            .with("city", "moved")
            .with("age", 99i64)
            .with("bio", "x"),
    )
    .unwrap();

    let rows = db.query(&city_age_in_norway()).unwrap();
    let moved = rows.iter().find(|(id, _)| *id == RecordId(0)).unwrap();
    assert_eq!(moved.1.get("city"), Some(&Value::Str("moved".into())));
    assert_eq!(moved.1.get("age"), Some(&Value::I64(99)));
}

/// Deleting must remove the row from the projection too, or the index serves
/// a record that no longer exists.
#[test]
fn deleting_removes_the_row_from_the_projection() {
    let dir = Tmp::new("delete");
    let mut db = seeded(dir.path());
    db.create_covering_index("users", "country", &covers(), IndexKind::Hash)
        .unwrap();

    let before = db.query(&city_age_in_norway()).unwrap().len();
    db.delete("users", RecordId(0)).unwrap();
    let after = db.query(&city_age_in_norway()).unwrap();

    assert_eq!(after.len(), before - 1);
    assert!(
        !after.iter().any(|(id, _)| *id == RecordId(0)),
        "a deleted record was still served from the covering index"
    );
}

/// A moved record leaves the key it used to be filed under.
#[test]
fn changing_the_indexed_field_moves_the_row() {
    let dir = Tmp::new("move");
    let mut db = seeded(dir.path());
    db.create_covering_index("users", "country", &covers(), IndexKind::Hash)
        .unwrap();

    db.update(
        "users",
        RecordId(0),
        Record::new()
            .with("country", "SE")
            .with("city", "stockholm")
            .with("age", 40i64)
            .with("bio", "x"),
    )
    .unwrap();

    let norway = db.query(&city_age_in_norway()).unwrap();
    assert!(
        !norway.iter().any(|(id, _)| *id == RecordId(0)),
        "a record that changed country is still filed under the old one"
    );
}

/// The composite-index restore bug, in its new home. A covering index over two
/// or more fields has a NUL in its name, exactly like a composite one, so
/// anything that recognises composite indexes by that NUL will grab it.
#[test]
fn a_covering_index_survives_a_restart_and_is_not_mistaken_for_a_composite() {
    let dir = Tmp::new("restart");
    let expected = {
        let mut db = seeded(dir.path());
        db.create_covering_index("users", "country", &covers(), IndexKind::Hash)
            .unwrap();
        db.query(&city_age_in_norway()).unwrap()
    };

    let mut db = Database::open(dir.path(), Policy::manual(0)).unwrap();
    let explain = db.explain(&city_age_in_norway());
    assert!(
        explain.contains("CoveringLookup"),
        "the covering index did not come back as one after a restart:\n{explain}"
    );
    assert_eq!(
        db.query(&city_age_in_norway()).unwrap(),
        expected,
        "a covering index came back after a restart holding different rows"
    );
}
