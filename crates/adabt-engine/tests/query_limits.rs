//! Per-query memory budgets and cooperative cancellation, at the engine
//! layer.
//!
//! `adabt-exec`'s own suite is the evidence that the mechanism inside the
//! executor is correct — a budget refuses a `Sort` that would overrun it, a
//! cancel flag is polled during a scan and honored whether it was set before
//! the call or partway through. This only has to show `Database` and
//! `ShardedDatabase` actually wire `Policy::constraints.max_query_ram_bytes` and
//! `query_cancellable`'s flag through to that mechanism.

use adabt_core::error::Error;
use adabt_core::ids::RecordId;
use adabt_core::policy::Policy;
use adabt_core::record::Record;
use adabt_core::schema::Schema;
use adabt_core::store::LogicalStore;
use adabt_engine::sharded::ShardedDatabase;
use adabt_engine::Database;
use adabt_ir::plan::{LogicalOp, LogicalPlan, SortKey};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

struct Tmp(PathBuf);
impl Tmp {
    fn new(tag: &str) -> Self {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "adabt-query-limits-{tag}-{}-{:?}",
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
    Record::new().with("i", i).with("pad", "x".repeat(200))
}

fn sort_query() -> LogicalPlan {
    LogicalPlan::new(LogicalOp::scan("c").sort(vec![SortKey {
        field: "i".into(),
        descending: false,
    }]))
}

fn seeded(dir: &Path, policy: Policy, n: u64) -> Database {
    let mut db = Database::open(dir, policy).unwrap();
    db.create_collection("c", Schema::dynamic()).unwrap();
    for i in 0..n {
        db.insert("c", RecordId(i), rec(i)).unwrap();
    }
    db
}

#[test]
fn a_query_over_the_ram_budget_is_refused() {
    let t = Tmp::new("over");
    let mut policy = Policy::conventional();
    policy.constraints.max_query_ram_bytes = Some(512);
    let mut db = seeded(t.path(), policy, 2000);

    let err = db.query(&sort_query()).unwrap_err();
    assert!(matches!(err, Error::Cancelled(_)), "{err}");
    // And the database is still fully usable — a refused query is not a
    // damaged one.
    assert_eq!(db.count("c").unwrap(), 2000);
}

#[test]
fn a_query_within_the_ram_budget_succeeds() {
    let t = Tmp::new("under");
    let mut policy = Policy::conventional();
    policy.constraints.max_query_ram_bytes = Some(10 * 1024 * 1024);
    let mut db = seeded(t.path(), policy, 2000);

    let rows = db.query(&sort_query()).unwrap();
    assert_eq!(rows.len(), 2000);
}

#[test]
fn an_unset_budget_leaves_a_large_query_unbounded() {
    let t = Tmp::new("unset");
    let mut db = seeded(t.path(), Policy::conventional(), 3000);
    let rows = db.query(&sort_query()).unwrap();
    assert_eq!(rows.len(), 3000);
}

#[test]
fn query_cancellable_honors_a_flag_set_before_the_call() {
    let t = Tmp::new("precancel");
    let mut db = seeded(t.path(), Policy::conventional(), 500);
    let cancel = Arc::new(AtomicBool::new(true));

    let err = db.query_cancellable(&sort_query(), cancel).unwrap_err();
    assert!(matches!(err, Error::Cancelled(_)));
}

#[test]
fn query_cancellable_with_an_unset_flag_behaves_like_query() {
    let t = Tmp::new("noop-cancel");
    let mut db = seeded(t.path(), Policy::conventional(), 500);
    let cancel = Arc::new(AtomicBool::new(false));

    let rows = db.query_cancellable(&sort_query(), cancel).unwrap();
    assert_eq!(rows.len(), 500);
}

#[test]
fn cancelling_one_query_does_not_affect_the_next() {
    let t = Tmp::new("scoped");
    let mut db = seeded(t.path(), Policy::conventional(), 200);
    let cancel = Arc::new(AtomicBool::new(true));
    assert!(db.query_cancellable(&sort_query(), cancel).is_err());

    // `pending_cancel` must have been cleared after the cancelled call —
    // an ordinary query right after must not inherit it.
    let rows = db.query(&sort_query()).unwrap();
    assert_eq!(rows.len(), 200);
}

#[test]
fn sharded_database_respects_the_shared_policys_ram_budget() {
    let t = Tmp::new("shard-budget");
    let mut policy = Policy::conventional();
    policy.constraints.max_query_ram_bytes = Some(512);
    let sdb = ShardedDatabase::open(t.path(), 3, policy).unwrap();
    sdb.create_collection("c", Schema::dynamic()).unwrap();
    for i in 0..2000u64 {
        sdb.insert("c", RecordId(i), rec(i)).unwrap();
    }

    let err = sdb.query(&sort_query()).unwrap_err();
    assert!(matches!(err, Error::Cancelled(_)), "{err}");
}

#[test]
fn sharded_database_query_cancellable_honors_a_preset_flag() {
    let t = Tmp::new("shard-cancel");
    let sdb = ShardedDatabase::open(t.path(), 2, Policy::conventional()).unwrap();
    sdb.create_collection("c", Schema::dynamic()).unwrap();
    for i in 0..100u64 {
        sdb.insert("c", RecordId(i), rec(i)).unwrap();
    }
    let cancel = Arc::new(AtomicBool::new(true));

    let err = sdb.query_cancellable(&sort_query(), cancel).unwrap_err();
    assert!(matches!(err, Error::Cancelled(_)));
}
