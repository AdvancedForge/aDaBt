//! Partial indexes: indexing only the rows that matter.
//!
//! An index on `orders.status` where 99% of rows are `shipped` and every query
//! asks for `pending` stores and maintains all of them. A partial index stores
//! the 1%. It is smaller, faster to probe, and — the part that matters most on
//! an engine whose write path re-indexes on every update — far cheaper to
//! maintain, since a write to a record the condition excludes touches nothing.
//!
//! The hard part is not building one, it is knowing when it may be *used*. A
//! partial index is a legal access path only for a query whose own predicate
//! guarantees every row it wants is present. This engine tests that
//! syntactically — the predicate must contain the condition as a conjunct —
//! which is much weaker than real implication and deliberately so. Being too
//! weak costs a slower plan. Being too clever costs correct answers, and these
//! tests are mostly about the second.

use adabt_core::ids::RecordId;
use adabt_core::index_kind::IndexKind;
use adabt_core::policy::Policy;
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
            "adabt-partial-{tag}-{}-{:?}",
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

/// One row in ten is pending; the rest are shipped. The shape a partial index
/// exists for.
fn seeded(dir: &Path) -> Database {
    let mut db = Database::open(dir, Policy::manual(0)).unwrap();
    db.create_collection("orders", Schema::dynamic()).unwrap();
    for i in 0..N {
        db.insert(
            "orders",
            RecordId(i),
            Record::new()
                .with("status", if i % 10 == 0 { "pending" } else { "shipped" })
                .with("region", if i % 2 == 0 { "north" } else { "south" })
                .with("total", (i % 500) as i64),
        )
        .unwrap();
    }
    db
}

fn pending() -> Expr {
    Expr::eq("status", "pending")
}

/// `WHERE status = 'pending' AND region = 'north'`
fn pending_north() -> LogicalPlan {
    LogicalPlan::new(
        LogicalOp::scan("orders").filter(Expr::And(vec![pending(), Expr::eq("region", "north")])),
    )
}

fn with_partial(dir: &Path) -> Database {
    let mut db = seeded(dir);
    db.create_partial_index("orders", "region", pending(), IndexKind::Hash)
        .unwrap();
    db
}

#[test]
fn a_partial_index_returns_exactly_what_a_scan_would() {
    let dir = Tmp::new("same");
    let mut db = seeded(dir.path());
    let expected = db.query(&pending_north()).unwrap();

    db.create_partial_index("orders", "region", pending(), IndexKind::Hash)
        .unwrap();
    let actual = db.query(&pending_north()).unwrap();

    assert!(!expected.is_empty(), "the fixture must produce rows");
    assert_eq!(actual, expected, "a partial index changed the answer");
}

#[test]
fn the_planner_uses_it_when_the_predicate_guarantees_the_condition() {
    let dir = Tmp::new("used");
    let db = with_partial(dir.path());
    let explain = db.explain(&pending_north());
    assert!(
        explain.contains("partial"),
        "the planner did not use the partial index:\n{explain}"
    );
}

/// The case that matters. A query that does *not* guarantee the condition must
/// not touch the index — its rows are a subset, and reading them as if they
/// were the whole collection silently loses every shipped order.
#[test]
fn a_query_without_the_condition_must_not_use_it() {
    let dir = Tmp::new("unguarded");
    let mut db = with_partial(dir.path());

    // Same indexed field, no `status` constraint at all.
    let plan = LogicalPlan::new(LogicalOp::scan("orders").filter(Expr::eq("region", "north")));
    let explain = db.explain(&plan);
    assert!(
        !explain.contains("partial"),
        "a partial index was used for a query that does not imply its condition:\n{explain}"
    );

    let rows = db.query(&plan).unwrap();
    assert_eq!(
        rows.len(),
        (N / 2) as usize,
        "half the orders are in the north; a partial index leaked into the plan"
    );
}

/// `OR` guarantees neither side, so it must never satisfy a condition.
#[test]
fn a_disjunction_containing_the_condition_does_not_imply_it() {
    let dir = Tmp::new("or");
    let mut db = with_partial(dir.path());

    let plan = LogicalPlan::new(
        LogicalOp::scan("orders").filter(Expr::Or(vec![pending(), Expr::eq("region", "north")])),
    );
    let explain = db.explain(&plan);
    assert!(
        !explain.contains("partial"),
        "an OR was treated as implying one of its branches:\n{explain}"
    );

    let scanned = {
        let dir2 = Tmp::new("or-ref");
        let mut plain = seeded(dir2.path());
        plain.query(&plan).unwrap()
    };
    assert_eq!(db.query(&plan).unwrap(), scanned);
}

/// A different-but-entailing condition is *not* recognised. This is a
/// documented limitation rather than a bug, and it is asserted so that anyone
/// who later teaches the planner real implication has to come here and say so
/// deliberately.
#[test]
fn a_stronger_predicate_is_not_recognised_as_implying_a_weaker_condition() {
    let dir = Tmp::new("weaker");
    let mut db = seeded(dir.path());
    db.create_partial_index(
        "orders",
        "region",
        Expr::cmp("total", adabt_ir::CmpOp::Gt, 100i64),
        IndexKind::Hash,
    )
    .unwrap();

    // total > 200 entails total > 100, but the rule here is syntactic.
    let plan = LogicalPlan::new(LogicalOp::scan("orders").filter(Expr::And(vec![
        Expr::cmp("total", adabt_ir::CmpOp::Gt, 200i64),
        Expr::eq("region", "north"),
    ])));
    let explain = db.explain(&plan);
    assert!(
        !explain.contains("partial"),
        "implication was inferred; if that is now intended, this test is the \
         place to record the new rule:\n{explain}"
    );
}

/// A record that stops qualifying must leave the index. Getting this wrong
/// serves rows the condition excludes.
#[test]
fn a_record_that_stops_qualifying_leaves_the_index() {
    let dir = Tmp::new("leaves");
    let mut db = with_partial(dir.path());

    let before = db.query(&pending_north()).unwrap().len();
    // RecordId(0) is pending and north.
    db.update(
        "orders",
        RecordId(0),
        Record::new()
            .with("status", "shipped")
            .with("region", "north")
            .with("total", 1i64),
    )
    .unwrap();

    let after = db.query(&pending_north()).unwrap();
    assert_eq!(after.len(), before - 1);
    assert!(!after.iter().any(|(id, _)| *id == RecordId(0)));
}

/// And one that starts qualifying must join it.
#[test]
fn a_record_that_starts_qualifying_joins_the_index() {
    let dir = Tmp::new("joins");
    let mut db = with_partial(dir.path());

    let before = db.query(&pending_north()).unwrap().len();
    // RecordId(2) is shipped and north.
    db.update(
        "orders",
        RecordId(2),
        Record::new()
            .with("status", "pending")
            .with("region", "north")
            .with("total", 1i64),
    )
    .unwrap();

    let after = db.query(&pending_north()).unwrap();
    assert_eq!(after.len(), before + 1);
    assert!(after.iter().any(|(id, _)| *id == RecordId(2)));
}

/// The restore hazard, and the worst version of it in this project. A partial
/// index rebuilt without its condition is a *full* index holding a subset of
/// the rows — answers that look right and are not.
#[test]
fn a_partial_index_comes_back_partial_after_a_restart() {
    let dir = Tmp::new("restart");
    let expected = {
        let mut db = with_partial(dir.path());
        db.query(&pending_north()).unwrap()
    };

    let mut db = Database::open(dir.path(), Policy::manual(0)).unwrap();
    let explain = db.explain(&pending_north());
    assert!(
        explain.contains("partial"),
        "the partial index did not come back as one:\n{explain}"
    );
    assert_eq!(db.query(&pending_north()).unwrap(), expected);

    // And a query that does not imply the condition still must not use it —
    // the condition survived, not just the name.
    let unguarded = LogicalPlan::new(LogicalOp::scan("orders").filter(Expr::eq("region", "north")));
    assert_eq!(db.query(&unguarded).unwrap().len(), (N / 2) as usize);
}

/// Smaller is the whole point, so it gets measured rather than assumed.
///
/// Compared in bytes rather than entries because bytes are what the engine
/// already reports, and because bytes are the thing the trade is actually
/// about: a partial index is worth building when it costs a fraction of a
/// full one to hold and to keep up to date.
#[test]
fn a_partial_index_costs_a_fraction_of_a_full_one() {
    let full = {
        let dir = Tmp::new("smaller-full");
        let mut db = seeded(dir.path());
        let empty = db.index_memory_bytes();
        db.create_index("orders", "region", IndexKind::Hash)
            .unwrap();
        db.index_memory_bytes() - empty
    };
    let partial = {
        let dir = Tmp::new("smaller-partial");
        let mut db = seeded(dir.path());
        let empty = db.index_memory_bytes();
        db.create_partial_index("orders", "region", pending(), IndexKind::Hash)
            .unwrap();
        db.index_memory_bytes() - empty
    };

    // One row in ten qualifies. The index is not ten times smaller — a hash
    // index has per-key overhead that does not shrink with the entries under
    // it — but it must be substantially smaller, and half is a floor loose
    // enough to survive an implementation change and tight enough to fail if
    // the condition is being ignored.
    assert!(
        partial < full / 2,
        "a partial index over a tenth of the rows cost {partial} bytes against \
         {full} for the full one; the condition is not being applied"
    );
}
