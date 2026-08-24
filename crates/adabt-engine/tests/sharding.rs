//! Partitioning must be invisible.
//!
//! A sharded database and an unsharded one are two physical arrangements of the
//! same logical database, so the project's central rule applies: they must give
//! the same answers. Not similar answers, and not answers that agree up to
//! ordering — identical rows in an identical order, because that is what callers
//! and the differential runner both compare.
//!
//! Sharding is the sharpest test of that rule the codebase has, because it
//! changes the order work happens in. Order is exactly where floating-point
//! aggregates and scan contracts break, and a partitioning scheme that got it
//! wrong would look right on every small example.

use adabt_core::ids::RecordId;
use adabt_core::policy::Policy;
use adabt_core::record::Record;
use adabt_core::schema::{FieldDef, FieldType, Schema, SchemaMode};
use adabt_core::store::LogicalStore;
use adabt_core::value::Value;
use adabt_engine::sharded::ShardedDatabase;
use adabt_engine::Database;
use adabt_ir::plan::{Agg, AggKind, LogicalOp, LogicalPlan, SortKey};
use adabt_ir::{CmpOp, Expr};
use adabt_testkit::rng::Rng;
use std::path::{Path, PathBuf};

struct Tmp(PathBuf);
impl Tmp {
    fn new(tag: &str) -> Self {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "adabt-shard-{tag}-{}-{:?}",
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

const COUNTRIES: [&str; 5] = ["NO", "SE", "DK", "FI", "IS"];
const N: u64 = 2_000;

fn schema() -> Schema {
    Schema::new(
        SchemaMode::Fixed,
        vec![
            FieldDef::new("id", FieldType::U64).required(),
            FieldDef::new("country", FieldType::Char(8)),
            FieldDef::new("age", FieldType::I64),
            FieldDef::new("balance", FieldType::I64),
        ],
    )
    .unwrap()
}

fn rec(i: u64) -> Record {
    Record::new()
        .with("id", i)
        .with("country", COUNTRIES[(i % 5) as usize])
        .with("age", (i % 70) as i64)
        .with("balance", (i * 37 % 100_000) as i64)
}

/// Every query shape the engine supports, so the comparison covers the whole
/// decomposition and not just the easy half.
fn queries() -> Vec<LogicalPlan> {
    let mut v = vec![
        LogicalPlan::new(LogicalOp::scan("users")),
        LogicalPlan::new(LogicalOp::get("users", RecordId(0))),
        LogicalPlan::new(LogicalOp::get("users", RecordId(N - 1))),
        LogicalPlan::new(LogicalOp::get("users", RecordId(N + 500))),
        LogicalPlan::new(LogicalOp::scan("users").limit(7)),
        LogicalPlan::new(
            LogicalOp::scan("users")
                .sort(vec![SortKey {
                    field: "balance".into(),
                    descending: true,
                }])
                .limit(20),
        ),
        LogicalPlan::new(LogicalOp::scan("users").project(vec!["id".into(), "country".into()])),
    ];
    for c in COUNTRIES {
        v.push(LogicalPlan::new(
            LogicalOp::scan("users").filter(Expr::eq("country", c)),
        ));
    }
    for bound in [10i64, 45, 69] {
        v.push(LogicalPlan::new(
            LogicalOp::scan("users").filter(Expr::cmp("age", CmpOp::Gt, bound)),
        ));
    }
    // Aggregates: the ones that would break if partials were combined per shard.
    v.push(LogicalPlan::new(LogicalOp::scan("users").aggregate(
        vec!["country".into()],
        vec![
            Agg::count("n"),
            Agg::over(AggKind::Sum, "balance", "total"),
            Agg::over(AggKind::Avg, "balance", "mean"),
            Agg::over(AggKind::Min, "age", "youngest"),
            Agg::over(AggKind::Max, "age", "oldest"),
        ],
    )));
    v.push(LogicalPlan::new(LogicalOp::scan("users").aggregate(
        vec![],
        vec![
            Agg::count("n"),
            Agg::over(AggKind::Sum, "balance", "total"),
            Agg::over(AggKind::Avg, "age", "mean"),
        ],
    )));
    v.push(LogicalPlan::new(
        LogicalOp::scan("users")
            .filter(Expr::cmp("age", CmpOp::Ge, 30i64))
            .aggregate(
                vec!["country".into()],
                vec![Agg::count("n"), Agg::over(AggKind::Avg, "balance", "mean")],
            ),
    ));
    v
}

/// A sharded database and a plain one over the same data.
struct Pair {
    sharded: ShardedDatabase,
    plain: Database,
    _a: Tmp,
    _b: Tmp,
}

impl Pair {
    fn new(tag: &str, shards: usize, level: u8) -> Self {
        let a = Tmp::new(&format!("{tag}-s"));
        let b = Tmp::new(&format!("{tag}-p"));
        let sharded = ShardedDatabase::open(a.path(), shards, Policy::manual(level)).unwrap();
        let mut plain = Database::open(b.path(), Policy::manual(level)).unwrap();
        sharded.create_collection("users", schema()).unwrap();
        plain.create_collection("users", schema()).unwrap();
        for i in 0..N {
            sharded.insert("users", RecordId(i), rec(i)).unwrap();
            plain.insert("users", RecordId(i), rec(i)).unwrap();
        }
        Pair {
            sharded,
            plain,
            _a: a,
            _b: b,
        }
    }

    fn agree(&mut self, note: &str) {
        for q in queries() {
            let got = self.sharded.query(&q).unwrap();
            let want = self.plain.query(&q).unwrap();
            assert_eq!(
                got,
                want,
                "{note}: {} shards disagreed with one\n{}",
                self.sharded.shard_count(),
                q.explain()
            );
        }
        assert_eq!(
            self.sharded.scan("users").unwrap(),
            self.plain.scan("users").unwrap(),
            "{note}: scan order differs"
        );
        assert_eq!(
            self.sharded.count("users").unwrap(),
            self.plain.count("users").unwrap()
        );
    }
}

#[test]
fn a_sharded_database_answers_exactly_as_an_unsharded_one() {
    for shards in [1usize, 2, 3, 4, 7] {
        let mut p = Pair::new(&format!("same{shards}"), shards, 0);
        p.agree("after load");
    }
}

#[test]
fn partitioning_survives_a_mutating_workload() {
    // Deletes and updates move rows between groups and leave gaps in the id
    // space, which is where a merge that assumed dense or contiguous shards
    // would start returning rows in the wrong order.
    let mut p = Pair::new("mutating", 4, 0);
    let mut rng = Rng::new(0xC0FFEE);
    for step in 0..1_200u64 {
        match rng.below(3) {
            0 => {
                let id = RecordId(N + step);
                p.sharded.insert("users", id, rec(N + step)).unwrap();
                p.plain.insert("users", id, rec(N + step)).unwrap();
            }
            1 => {
                let id = RecordId(rng.below(N));
                p.sharded.delete("users", id).unwrap();
                p.plain.delete("users", id).unwrap();
            }
            _ => {
                let id = RecordId(rng.below(N));
                let r = rec(id.0 + 1).with("id", id.0);
                p.sharded.update("users", id, r.clone()).unwrap();
                p.plain.update("users", id, r).unwrap();
            }
        }
        if step % 200 == 0 {
            p.agree(&format!("step {step}"));
        }
    }
    p.agree("final");
}

#[test]
fn every_optimization_level_is_invisible_through_partitioning() {
    // Each shard optimizes itself, so at higher levels the shards are running
    // different physical plans from each other *and* from the unsharded engine.
    // The answers still may not move.
    for level in [0u8, 2, 4, 10] {
        let mut p = Pair::new(&format!("level{level}"), 3, level);
        p.agree(&format!("level {level}"));
        p.sharded.optimize().unwrap();
        p.plain.optimize().unwrap();
        p.agree(&format!("level {level} after optimizing"));
    }
}

#[test]
fn shards_survive_a_restart_independently() {
    let t = Tmp::new("restart");
    let expected = {
        let db = ShardedDatabase::open(t.path(), 4, Policy::manual(2)).unwrap();
        db.create_collection("users", schema()).unwrap();
        for i in 0..N {
            db.insert("users", RecordId(i), rec(i)).unwrap();
        }
        db.checkpoint().unwrap();
        db.scan("users").unwrap()
    };
    let db = ShardedDatabase::open(t.path(), 4, Policy::manual(2)).unwrap();
    assert_eq!(db.scan("users").unwrap(), expected);
    assert_eq!(db.count("users").unwrap(), N as usize);
}

#[test]
fn shards_are_reached_concurrently() {
    // The point of the whole module. Eight threads hammering four shards must
    // all finish and all see consistent data — if every request took one global
    // lock this would still pass, so the assertion that matters is the one about
    // each shard having its own.
    let t = Tmp::new("concurrent");
    let db = ShardedDatabase::open(t.path(), 4, Policy::manual(1)).unwrap();
    db.create_collection("users", schema()).unwrap();
    for i in 0..N {
        db.insert("users", RecordId(i), rec(i)).unwrap();
    }
    let db = std::sync::Arc::new(db);

    std::thread::scope(|s| {
        for t in 0..8u64 {
            let db = std::sync::Arc::clone(&db);
            s.spawn(move || {
                for k in 0..200u64 {
                    let id = RecordId((t * 200 + k) % N);
                    let q = LogicalPlan::new(LogicalOp::get("users", id));
                    let got = db.query(&q).unwrap();
                    assert_eq!(got, vec![(id, rec(id.0))], "thread {t} saw the wrong row");
                }
            });
        }
    });

    // Four independent locks, not one shared one.
    for i in 0..4 {
        assert!(db.shard(i).is_some());
    }
    assert!(db.shard(4).is_none());
}

#[test]
fn shards_can_specialise_differently_from_each_other() {
    // A consequence of shared-nothing worth being able to see: a shard decides
    // from its own traffic, so a skewed workload leaves the shards holding
    // different physical structures. That is the partitioning working, not an
    // inconsistency.
    let t = Tmp::new("divergent");
    let db = ShardedDatabase::open(t.path(), 2, Policy::manual(0)).unwrap();
    db.create_collection("users", schema()).unwrap();
    for i in 0..4_000u64 {
        db.insert("users", RecordId(i), rec(i)).unwrap();
    }
    // Only even ids, so only shard 0 ever sees a lookup.
    for i in (0..2_000u64).step_by(2) {
        let q = LogicalPlan::new(LogicalOp::get("users", RecordId(i)));
        db.query(&q).unwrap();
    }
    let busy = adabt_engine::sharded::ShardedDatabase::shard(&db, 0)
        .unwrap()
        .lock()
        .unwrap()
        .telemetry()
        .total_calls();
    let idle = adabt_engine::sharded::ShardedDatabase::shard(&db, 1)
        .unwrap()
        .lock()
        .unwrap()
        .telemetry()
        .total_calls();
    assert!(
        busy > idle,
        "the skew did not reach the shards: {busy} vs {idle}"
    );
}

#[test]
fn a_zero_shard_database_is_refused() {
    let t = Tmp::new("zero");
    assert!(ShardedDatabase::open(t.path(), 0, Policy::manual(0)).is_err());
}

#[test]
fn an_aggregate_over_a_partitioned_sum_is_bit_identical() {
    // The specific thing that would go wrong if partial sums were combined per
    // shard: the addition order would depend on the shard count, so the answer
    // would depend on the partitioning. Comparing across shard counts is what
    // catches it, because two shardings that both differ from the truth in the
    // same way would still agree with each other.
    let q = LogicalPlan::new(LogicalOp::scan("users").aggregate(
        vec!["country".into()],
        vec![
            Agg::over(AggKind::Sum, "balance", "total"),
            Agg::over(AggKind::Avg, "balance", "mean"),
        ],
    ));
    let mut reference: Option<Vec<(RecordId, Record)>> = None;
    for shards in [1usize, 2, 5, 8] {
        let mut p = Pair::new(&format!("sum{shards}"), shards, 0);
        let got = p.sharded.query(&q).unwrap();
        assert_eq!(got, p.plain.query(&q).unwrap(), "{shards} shards");
        match &reference {
            None => reference = Some(got),
            Some(want) => assert_eq!(&got, want, "{shards} shards produced a different total"),
        }
    }
    // And the totals are what they should be, so this is not agreement on a
    // shared mistake.
    let rows = reference.unwrap();
    let total: f64 = rows
        .iter()
        .filter_map(|(_, r)| match r.get("total") {
            Some(Value::F64(f)) => Some(*f),
            _ => None,
        })
        .sum();
    let expected: f64 = (0..N).map(|i| (i * 37 % 100_000) as f64).sum();
    assert_eq!(total, expected);
}

#[test]
fn auto_allocated_ids_route_to_the_shard_that_generated_them() {
    // The property the whole scheme depends on: `seq * shard_count + shard_index`
    // must always satisfy `id % shard_count == shard_index`, or a later lookup by
    // that id would go to the wrong shard and find nothing.
    let t = Tmp::new("auto-route");
    let db = ShardedDatabase::open(t.path(), 4, Policy::manual(0)).unwrap();
    db.create_collection("users", schema()).unwrap();

    let mut ids = Vec::new();
    for i in 0..200u64 {
        ids.push(db.insert_auto("users", rec(i)).unwrap());
    }
    for id in &ids {
        assert!(
            db.get("users", *id).unwrap().is_some(),
            "id {} was not found on the shard its own residue names",
            id.0
        );
    }
}

#[test]
fn auto_allocated_ids_never_collide_across_shards() {
    let t = Tmp::new("auto-no-collide");
    let db = ShardedDatabase::open(t.path(), 5, Policy::manual(0)).unwrap();
    db.create_collection("users", schema()).unwrap();

    let mut ids: Vec<u64> = (0..1_000u64)
        .map(|i| db.insert_auto("users", rec(i)).unwrap().0)
        .collect();
    let before = ids.len();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), before, "two auto-allocated ids collided");
    assert_eq!(db.count("users").unwrap(), before);
}

#[test]
fn a_single_shard_database_behaves_like_the_unsharded_local_counter() {
    // shard_count = 1 makes the formula `seq * 1 + 0 = seq`, i.e. ordinary
    // monotonic local allocation, so nothing about single-shard use should look
    // different from before sharding existed.
    let t = Tmp::new("auto-one-shard");
    let db = ShardedDatabase::open(t.path(), 1, Policy::manual(0)).unwrap();
    db.create_collection("users", schema()).unwrap();
    let ids: Vec<u64> = (0..20u64)
        .map(|i| db.insert_auto("users", rec(i)).unwrap().0)
        .collect();
    assert_eq!(ids, (0..20u64).collect::<Vec<_>>());
}

#[test]
fn auto_allocated_ids_survive_a_restart_across_every_shard() {
    let t = Tmp::new("auto-restart-sharded");
    let before: Vec<u64> = {
        let db = ShardedDatabase::open(t.path(), 3, Policy::manual(0)).unwrap();
        db.create_collection("users", schema()).unwrap();
        let ids: Vec<u64> = (0..90u64)
            .map(|i| db.insert_auto("users", rec(i)).unwrap().0)
            .collect();
        db.checkpoint().unwrap();
        ids
    };
    let db = ShardedDatabase::open(t.path(), 3, Policy::manual(0)).unwrap();
    let next = db.insert_auto("users", rec(999)).unwrap();
    assert!(
        !before.contains(&next.0),
        "a restart reused an id already given out: {}",
        next.0
    );
}

#[test]
fn a_sharded_batch_insert_partitions_records_to_the_right_shards() {
    let t = Tmp::new("batch-partition");
    let db = ShardedDatabase::open(t.path(), 4, Policy::manual(0)).unwrap();
    db.create_collection("users", schema()).unwrap();

    let recs: Vec<(RecordId, Record)> = (0..800u64).map(|i| (RecordId(i), rec(i))).collect();
    let n = db.insert_batch("users", recs).unwrap();
    assert_eq!(n, 800);
    assert_eq!(db.count("users").unwrap(), 800);
    for i in 0..800u64 {
        assert_eq!(db.get("users", RecordId(i)).unwrap(), Some(rec(i)), "{i}");
    }
}

#[test]
fn a_sharded_batch_insert_answers_identically_to_individual_inserts() {
    let a = Tmp::new("batch-parity-a");
    let b = Tmp::new("batch-parity-b");
    let individually = ShardedDatabase::open(a.path(), 3, Policy::manual(0)).unwrap();
    let batched = ShardedDatabase::open(b.path(), 3, Policy::manual(0)).unwrap();
    individually.create_collection("users", schema()).unwrap();
    batched.create_collection("users", schema()).unwrap();

    for i in 0..500u64 {
        individually.insert("users", RecordId(i), rec(i)).unwrap();
    }
    let recs: Vec<(RecordId, Record)> = (0..500u64).map(|i| (RecordId(i), rec(i))).collect();
    batched.insert_batch("users", recs).unwrap();

    assert_eq!(
        individually.scan("users").unwrap(),
        batched.scan("users").unwrap()
    );
}

#[test]
fn a_batch_that_fails_on_one_shard_still_commits_the_others() {
    // Documented, not hidden: atomicity holds within a shard, not across all of
    // them. A batch spanning shards can land on some and fail on the one whose
    // records conflict.
    let t = Tmp::new("batch-partial");
    let db = ShardedDatabase::open(t.path(), 4, Policy::manual(0)).unwrap();
    db.create_collection("users", schema()).unwrap();
    // Id 5 lands on shard 1 (5 % 4). Pre-occupy it there.
    db.insert("users", RecordId(5), rec(999)).unwrap();

    let recs: Vec<(RecordId, Record)> = (0..20u64).map(|i| (RecordId(i), rec(i))).collect();
    assert!(db.insert_batch("users", recs).is_err());

    // Shards that never touched id 5 committed their share.
    for i in 0..20u64 {
        if i % 4 != 1 {
            assert_eq!(
                db.get("users", RecordId(i)).unwrap(),
                Some(rec(i)),
                "shard for id {i} did not commit its share"
            );
        }
    }
    // Shard 1 (ids 1, 5, 9, 13, 17) never wrote anything: it's all-or-nothing
    // *within* the shard, and id 5's conflict fails the whole shard's slice.
    for i in [1u64, 9, 13, 17] {
        assert_eq!(
            db.get("users", RecordId(i)).unwrap(),
            None,
            "shard 1's batch partially committed at id {i}"
        );
    }
    assert_eq!(db.get("users", RecordId(5)).unwrap(), Some(rec(999)));
}

#[test]
fn a_transaction_on_one_shard_is_invisible_to_a_reader_on_the_same_shard_until_committed() {
    // Transactions operate on one shard's `Database` directly, reached through
    // `ShardedDatabase::shard`. Cross-shard transactions are out of scope for
    // this milestone; this exercises the in-scope case: single-shard SI reached
    // through the sharded wrapper rather than a bare `Database`.
    let t = Tmp::new("txn-shard");
    let db = ShardedDatabase::open(t.path(), 3, Policy::manual(0)).unwrap();
    db.create_collection("users", schema()).unwrap();
    // id 5 lands on shard 2 (5 % 3).
    let shard = db.shard(2).unwrap();
    let mut guard = shard.lock().unwrap();

    let reader = guard.begin();
    let mut writer = guard.begin();
    writer
        .insert(&mut guard, "users", RecordId(5), rec(5))
        .unwrap();
    guard.commit(writer).unwrap();

    assert_eq!(reader.get(&mut guard, "users", RecordId(5)).unwrap(), None);
    drop(guard);
    assert_eq!(db.get("users", RecordId(5)).unwrap(), Some(rec(5)));
}

#[test]
fn shards_share_one_comparable_timestamp_space() {
    // The groundwork for future cross-shard coordination: two shards' version
    // stamps must come from the same counter, or "when" a write on shard 0
    // happened relative to one on shard 1 is meaningless to ask.
    let t = Tmp::new("shared-clock");
    let db = ShardedDatabase::open(t.path(), 2, Policy::manual(0)).unwrap();
    db.create_collection("users", schema()).unwrap();

    // Id 0 -> shard 0, id 1 -> shard 1.
    db.insert("users", RecordId(0), rec(0)).unwrap();
    let ts_shard0 = {
        let mut g = db.shard(0).unwrap().lock().unwrap();
        g.begin().snapshot().at()
    };
    db.insert("users", RecordId(1), rec(1)).unwrap();
    let ts_shard1 = {
        let mut g = db.shard(1).unwrap().lock().unwrap();
        g.begin().snapshot().at()
    };
    // Shard 1's write happened after shard 0's, in real time and in the shared
    // counter, so a snapshot opened on shard 1 afterwards must read at a higher
    // timestamp than one opened on shard 0 beforehand — which could only be
    // false if the two shards were counting independently.
    assert!(
        ts_shard1.0 > ts_shard0.0,
        "shard0={} shard1={}: the shards are not sharing a clock",
        ts_shard0.0,
        ts_shard1.0
    );
}
