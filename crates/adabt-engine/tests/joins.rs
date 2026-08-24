//! `Database::query` executing a `LogicalOp::Join`.
//!
//! The central property under test, more than for any other operator this
//! project has: **which algorithm executed a join must never change the
//! answer.** A join has two, chosen by whether an index exists on the right
//! side's join field — `hash_join_and_the_indexed_fast_path_agree` is the
//! direct evidence that choosing between them never changes a result set,
//! the same property `indexes_never_change_query_results` in `engine.rs`
//! already proves for ordinary scans.

use adabt_core::error::Error;
use adabt_core::ids::RecordId;
use adabt_core::policy::Policy;
use adabt_core::record::Record;
use adabt_core::schema::{FieldDef, FieldType, Schema, SchemaMode};
use adabt_core::store::LogicalStore;
use adabt_core::value::Value;
use adabt_engine::sharded::ShardedDatabase;
use adabt_engine::Database;
use adabt_index::IndexKind;
use adabt_ir::plan::{JoinKind, LogicalOp, LogicalPlan};
use adabt_ir::{CmpOp, Expr};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

struct Tmp(PathBuf);
impl Tmp {
    fn new(tag: &str) -> Self {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "adabt-joins-{tag}-{}-{:?}",
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

fn users_schema() -> Schema {
    Schema::new(
        SchemaMode::Strict,
        vec![
            FieldDef::new("id", FieldType::U64).required(),
            FieldDef::new("name", FieldType::Str { max_len: Some(32) }),
        ],
    )
    .unwrap()
}

fn orders_schema() -> Schema {
    Schema::new(
        SchemaMode::Strict,
        vec![
            FieldDef::new("id", FieldType::U64).required(),
            FieldDef::new("user_id", FieldType::U64),
            FieldDef::new("amount", FieldType::I64),
        ],
    )
    .unwrap()
}

fn user(i: u64) -> Record {
    Record::new()
        .with("id", i)
        .with("name", format!("user-{i}"))
}

fn order(id: u64, user_id: u64, amount: i64) -> Record {
    Record::new()
        .with("id", id)
        .with("user_id", user_id)
        .with("amount", amount)
}

/// 5 users (0..5), orders: user 0 has none, user 1 has one, user 2 has three
/// (a fan-out case), users 3 and 4 have one each — a mix deliberately chosen
/// to exercise unmatched, single-match and multi-match rows in one dataset.
fn seeded(dir: &Path) -> Database {
    let mut db = Database::open(dir, Policy::conventional()).unwrap();
    db.create_collection("users", users_schema()).unwrap();
    db.create_collection("orders", orders_schema()).unwrap();
    for i in 0..5u64 {
        db.insert("users", RecordId(i), user(i)).unwrap();
    }
    let orders = [
        (100u64, 1u64, 10i64),
        (101, 2, 20),
        (102, 2, 30),
        (103, 2, 40),
        (104, 3, 50),
        (105, 4, 60),
    ];
    for (id, user_id, amount) in orders {
        db.insert("orders", RecordId(id), order(id, user_id, amount))
            .unwrap();
    }
    db
}

fn join_plan(kind: JoinKind) -> LogicalPlan {
    LogicalPlan::new(LogicalOp::scan("users").join(
        LogicalOp::scan("orders"),
        kind,
        ("id", "user_id"),
    ))
}

/// Rows keyed by the pair `(users.id, orders.id or none)`, so two runs can be
/// compared as sets without caring about the fabricated `RecordId` each row
/// happens to get.
fn fingerprint(rows: &[(RecordId, Record)]) -> Vec<(Option<i64>, Option<i64>, Option<i64>)> {
    let mut out: Vec<_> = rows
        .iter()
        .map(|(_, r)| {
            let uid = match r.get("users.id") {
                Some(Value::U64(n)) => Some(*n as i64),
                _ => None,
            };
            let oid = match r.get("orders.id") {
                Some(Value::U64(n)) => Some(*n as i64),
                _ => None,
            };
            let amount = match r.get("orders.amount") {
                Some(Value::I64(n)) => Some(*n),
                _ => None,
            };
            (uid, oid, amount)
        })
        .collect();
    out.sort();
    out
}

#[test]
fn an_inner_join_matches_rows_on_both_sides() {
    let t = Tmp::new("inner");
    let mut db = seeded(t.path());
    let rows = db.query(&join_plan(JoinKind::Inner)).unwrap();
    // user 0 has no orders and is absent; users 1,3,4 contribute one row
    // each; user 2 contributes three (its fan-out).
    assert_eq!(rows.len(), 6, "{rows:?}");
    for (_, r) in &rows {
        assert!(r.get("orders.id").is_some(), "inner join must always match");
    }
}

#[test]
fn a_left_join_includes_unmatched_left_rows_with_right_fields_absent() {
    let t = Tmp::new("left");
    let mut db = seeded(t.path());
    let rows = db.query(&join_plan(JoinKind::Left)).unwrap();
    // The 6 matched rows plus user 0's single unmatched row.
    assert_eq!(rows.len(), 7, "{rows:?}");

    let unmatched: Vec<_> = rows
        .iter()
        .filter(|(_, r)| r.get("orders.id").is_none())
        .collect();
    assert_eq!(unmatched.len(), 1);
    assert_eq!(
        unmatched[0].1.get("users.name"),
        Some(&Value::Str("user-0".into()))
    );
    // Absent, not an explicit null — merge_joined_fields never calls `set`
    // for a field the matched side does not have.
    assert!(unmatched[0].1.get("orders.amount").is_none());
}

#[test]
fn duplicate_matches_produce_one_output_row_per_pair() {
    let t = Tmp::new("fanout");
    let mut db = seeded(t.path());
    let rows = db.query(&join_plan(JoinKind::Inner)).unwrap();
    let user2_rows: Vec<_> = rows
        .iter()
        .filter(|(_, r)| r.get("users.id") == Some(&Value::U64(2)))
        .collect();
    assert_eq!(user2_rows.len(), 3, "user 2 has three orders");
    let mut amounts: Vec<i64> = user2_rows
        .iter()
        .filter_map(|(_, r)| match r.get("orders.amount") {
            Some(Value::I64(n)) => Some(*n),
            _ => None,
        })
        .collect();
    amounts.sort();
    assert_eq!(amounts, vec![20, 30, 40]);
}

#[test]
fn fields_are_prefixed_by_collection_name_and_never_collide() {
    let t = Tmp::new("prefix");
    let mut db = seeded(t.path());
    let rows = db.query(&join_plan(JoinKind::Inner)).unwrap();
    let (_, r) = rows
        .iter()
        .find(|(_, r)| r.get("users.id") == Some(&Value::U64(1)))
        .unwrap();
    // Both sides declare a field named "id"; both must survive distinctly.
    assert_eq!(r.get("users.id"), Some(&Value::U64(1)));
    assert_eq!(r.get("orders.id"), Some(&Value::U64(100)));
    assert!(
        r.get("id").is_none(),
        "an unqualified \"id\" would be ambiguous"
    );
}

#[test]
fn a_right_side_row_with_no_join_key_never_matches() {
    // `orders.user_id` is nullable; a row that simply never set it must not
    // become a match candidate for any left row, under either join kind.
    let t = Tmp::new("null-right");
    let mut db = seeded(t.path());
    db.insert(
        "orders",
        RecordId(999),
        Record::new().with("id", 999u64).with("amount", 5i64),
    )
    .unwrap();

    for kind in [JoinKind::Inner, JoinKind::Left] {
        let rows = db.query(&join_plan(kind)).unwrap();
        assert!(
            rows.iter()
                .all(|(_, r)| r.get("orders.id") != Some(&Value::U64(999))),
            "a keyless right row matched under {kind:?}"
        );
    }
}

#[test]
fn a_left_row_that_is_missing_the_join_key_still_gets_its_unmatched_row() {
    // `users` is `Strict` with `id` required, so there is no way to insert a
    // user record genuinely missing it. Naming a field no user has at all
    // ("missing_field") as the join key produces the same condition the
    // join's own logic has to handle — `Record::get` returning `None` — from
    // the plan rather than the data.
    let t = Tmp::new("null-left-direct");
    let mut db = seeded(t.path());
    let rows = db
        .query(&LogicalPlan::new(LogicalOp::scan("users").join(
            LogicalOp::scan("orders"),
            JoinKind::Left,
            ("missing_field", "user_id"),
        )))
        .unwrap();
    // Every left row is missing the join field, so every one of the 5
    // seeded users must appear exactly once, unmatched.
    assert_eq!(rows.len(), 5, "{rows:?}");
    assert!(rows.iter().all(|(_, r)| r.get("orders.id").is_none()));
}

/// Rows in the order the join actually emitted them, *unsorted* — the
/// difference from `fingerprint` matters: this project counts row order as
/// part of an answer (see `fetch_batches`'s own doc comment in
/// `adabt-exec`), so a test that sorts before comparing cannot tell a
/// genuine order divergence between two algorithms from agreement.
fn ordered_pairs(rows: &[(RecordId, Record)]) -> Vec<(Option<u64>, Option<u64>)> {
    rows.iter()
        .map(|(_, r)| {
            let uid = match r.get("users.id") {
                Some(Value::U64(n)) => Some(*n),
                _ => None,
            };
            let oid = match r.get("orders.id") {
                Some(Value::U64(n)) => Some(*n),
                _ => None,
            };
            (uid, oid)
        })
        .collect()
}

#[test]
fn an_explicit_null_join_key_never_matches_another_null_with_or_without_an_index() {
    // `NULL = NULL` is unknown, not true, so two rows whose join key is an
    // explicit null must not join — and critically, that must not depend on
    // whether an index happens to exist, or index presence changes the
    // answer.
    //
    // Note on what this does and does not prove: `normalize_for_storage`
    // (`adabt-core`'s `store.rs`) strips explicit nulls from every record on
    // write, so a null read back from a collection is indistinguishable from
    // an absent field regardless of schema mode. That means this test passes
    // even against a join that mishandles nulls — it documents the
    // storage-level guarantee, not the join's own logic. The join's own
    // null handling is covered where it is actually reachable, by
    // `adabt-exec`'s `hash_join_never_matches_a_null_key`, which builds rows
    // with genuine nulls directly rather than through a store that would
    // normalize them away.
    let build = |dir: &Path, indexed: bool| -> Vec<(RecordId, Record)> {
        let mut db = Database::open(dir, Policy::conventional()).unwrap();
        db.create_collection("users", Schema::dynamic()).unwrap();
        db.create_collection("orders", Schema::dynamic()).unwrap();
        db.insert(
            "users",
            RecordId(1),
            Record::new().with("id", Value::Null).with("name", "u"),
        )
        .unwrap();
        db.insert(
            "orders",
            RecordId(100),
            Record::new()
                .with("id", 100u64)
                .with("user_id", Value::Null),
        )
        .unwrap();
        if indexed {
            db.create_index("orders", "user_id", IndexKind::Hash)
                .unwrap();
        }
        db.query(&join_plan(JoinKind::Inner)).unwrap()
    };

    let t1 = Tmp::new("null-key-hash");
    let without_index = build(t1.path(), false);
    let t2 = Tmp::new("null-key-indexed");
    let with_index = build(t2.path(), true);

    assert!(
        without_index.is_empty(),
        "a null join key matched another null under the hash join: {without_index:?}"
    );
    assert!(
        with_index.is_empty(),
        "a null join key matched another null under the indexed join: {with_index:?}"
    );
}

#[test]
fn the_two_join_algorithms_agree_on_row_order_exactly_not_just_as_a_set() {
    // The strong form of the agreement property. `hash_join` walks the left
    // side in order and, per left row, emits matches in right-side scan
    // order; the indexed path walks the left side in the same order and
    // emits `index_lookup`'s ids sorted ascending. Those two are only the
    // same sequence because a bare `HeapScan`'s rows arrive in ascending
    // RecordId order — which is exactly the condition the fast path checks
    // for before engaging. If either side of that reasoning ever stops
    // holding, this test fails and `hash_join_and_the_indexed_fast_path_agree`
    // (which sorts) would not.
    let t1 = Tmp::new("order-hash");
    let mut db_no_index = seeded(t1.path());
    let t2 = Tmp::new("order-indexed");
    let mut db_indexed = seeded(t2.path());
    db_indexed
        .create_index("orders", "user_id", IndexKind::Hash)
        .unwrap();

    for kind in [JoinKind::Inner, JoinKind::Left] {
        let via_hash = db_no_index.query(&join_plan(kind)).unwrap();
        let via_indexed = db_indexed.query(&join_plan(kind)).unwrap();
        assert_eq!(
            ordered_pairs(&via_hash),
            ordered_pairs(&via_indexed),
            "the two join algorithms emitted {kind:?} rows in different orders"
        );
    }
}

#[test]
fn hash_join_and_the_indexed_fast_path_agree() {
    let t1 = Tmp::new("agree-hash");
    let mut db_no_index = seeded(t1.path());
    let via_hash_join = db_no_index.query(&join_plan(JoinKind::Inner)).unwrap();

    let t2 = Tmp::new("agree-indexed");
    let mut db_indexed = seeded(t2.path());
    db_indexed
        .create_index("orders", "user_id", IndexKind::Hash)
        .unwrap();
    let via_indexed_loop = db_indexed.query(&join_plan(JoinKind::Inner)).unwrap();

    assert_eq!(
        fingerprint(&via_hash_join),
        fingerprint(&via_indexed_loop),
        "the same join must answer identically whether or not an index exists"
    );

    // And for Left, including the unmatched row.
    let via_hash_left = db_no_index.query(&join_plan(JoinKind::Left)).unwrap();
    let via_indexed_left = db_indexed.query(&join_plan(JoinKind::Left)).unwrap();
    assert_eq!(fingerprint(&via_hash_left), fingerprint(&via_indexed_left));
}

#[test]
fn a_filter_on_the_right_side_is_still_applied_under_the_fast_path() {
    // A filter above the right scan disqualifies the indexed bypass (see
    // `exec::run`'s `Join` arm) — this proves that matters, not just that it
    // happens: without the check, this would incorrectly include orders
    // amounting to 10, 20 and 50, which the filter excludes.
    let t = Tmp::new("filtered-right");
    let mut db = seeded(t.path());
    db.create_index("orders", "user_id", IndexKind::Hash)
        .unwrap();
    let plan = LogicalPlan::new(LogicalOp::scan("users").join(
        LogicalOp::scan("orders").filter(Expr::cmp("amount", CmpOp::Gt, 25i64)),
        JoinKind::Inner,
        ("id", "user_id"),
    ));
    let rows = db.query(&plan).unwrap();
    let mut amounts: Vec<i64> = rows
        .iter()
        .filter_map(|(_, r)| match r.get("orders.amount") {
            Some(Value::I64(n)) => Some(*n),
            _ => None,
        })
        .collect();
    amounts.sort();
    assert_eq!(amounts, vec![30, 40, 50, 60], "{rows:?}");
}

#[test]
fn self_joins_are_rejected() {
    let t = Tmp::new("self");
    let mut db = seeded(t.path());
    let plan = LogicalPlan::new(LogicalOp::scan("orders").join(
        LogicalOp::scan("orders"),
        JoinKind::Inner,
        ("user_id", "user_id"),
    ));
    let err = db.query(&plan).unwrap_err();
    assert!(matches!(err, Error::Unsupported(_)), "{err}");
    // The database is unharmed by the refusal.
    assert_eq!(db.count("orders").unwrap(), 6);
}

#[test]
fn nested_joins_are_rejected() {
    let t = Tmp::new("nested");
    let mut db = seeded(t.path());
    db.create_collection("shipments", orders_schema()).unwrap();
    let inner = LogicalOp::scan("users").join(
        LogicalOp::scan("orders"),
        JoinKind::Inner,
        ("id", "user_id"),
    );
    let plan =
        LogicalPlan::new(inner.join(LogicalOp::scan("shipments"), JoinKind::Inner, ("id", "id")));
    let err = db.query(&plan).unwrap_err();
    assert!(matches!(err, Error::Unsupported(_)), "{err}");
}

#[test]
fn a_join_respects_the_query_memory_budget() {
    let t = Tmp::new("budget");
    let mut policy = Policy::conventional();
    policy.constraints.max_query_ram_bytes = Some(16);
    let mut db = Database::open(t.path(), policy).unwrap();
    db.create_collection("users", users_schema()).unwrap();
    db.create_collection("orders", orders_schema()).unwrap();
    for i in 0..200u64 {
        db.insert("users", RecordId(i), user(i)).unwrap();
    }
    let err = db.query(&join_plan(JoinKind::Inner)).unwrap_err();
    assert!(matches!(err, Error::Cancelled(_)), "{err}");
}

#[test]
fn a_join_honors_a_preset_cancel_flag() {
    let t = Tmp::new("cancel");
    let mut db = seeded(t.path());
    let cancel = Arc::new(AtomicBool::new(true));
    let err = db
        .query_cancellable(&join_plan(JoinKind::Inner), cancel)
        .unwrap_err();
    assert!(matches!(err, Error::Cancelled(_)));
}

#[test]
fn sharded_database_rejects_a_join_cleanly_instead_of_panicking() {
    let t = Tmp::new("sharded");
    let sdb = ShardedDatabase::open(t.path(), 3, Policy::conventional()).unwrap();
    sdb.create_collection("users", users_schema()).unwrap();
    sdb.create_collection("orders", orders_schema()).unwrap();
    let err = sdb.query(&join_plan(JoinKind::Inner)).unwrap_err();
    assert!(matches!(err, Error::Unsupported(_)), "{err}");

    // The escape hatch this refusal points to actually works: a single
    // shard's own `Database` can run the same join over its own data.
    let mut guard = sdb.shard(0).unwrap().lock().unwrap();
    guard.insert("users", RecordId(0), user(0)).unwrap();
    guard
        .insert("orders", RecordId(100), order(100, 0, 5))
        .unwrap();
    drop(guard);
    let mut guard = sdb.shard(0).unwrap().lock().unwrap();
    let rows = guard.query(&join_plan(JoinKind::Inner)).unwrap();
    assert_eq!(rows.len(), 1);
}

#[test]
fn explain_describes_a_join_plan() {
    let t = Tmp::new("explain");
    let db = seeded(t.path());
    let text = db.explain(&join_plan(JoinKind::Inner));
    assert!(text.contains("Join"), "{text}");
    assert!(text.contains("users"), "{text}");
    assert!(text.contains("orders"), "{text}");
}

#[test]
fn a_join_result_is_a_normal_row_set_ready_for_further_operators() {
    // Nothing further wraps a join in these tests, but the executor treats
    // a Join's output exactly like any other operator's — this is the
    // evidence, not an assumption: a `Limit` above the join still bounds it.
    let t = Tmp::new("limit-above");
    let mut db = seeded(t.path());
    let plan = LogicalPlan::new(
        LogicalOp::scan("users")
            .join(
                LogicalOp::scan("orders"),
                JoinKind::Inner,
                ("id", "user_id"),
            )
            .limit(2),
    );
    let rows = db.query(&plan).unwrap();
    assert_eq!(rows.len(), 2);
}

#[test]
fn joins_do_not_diverge_across_repeated_runs() {
    // A cheap stand-in for the differential runner this milestone
    // deliberately does not extend (see the M23 notes): running the same
    // join twice must produce the same fingerprint both times.
    let t = Tmp::new("repeat");
    let mut db = seeded(t.path());
    let a = fingerprint(&db.query(&join_plan(JoinKind::Left)).unwrap());
    let b = fingerprint(&db.query(&join_plan(JoinKind::Left)).unwrap());
    assert_eq!(a, b);
}
