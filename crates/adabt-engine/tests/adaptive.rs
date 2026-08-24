//! The adaptive driver, end to end.
//!
//! The central claim being tested: **the same workload under different
//! priorities converges on different physical configurations, and neither
//! changes an answer.**

use adabt_core::ids::RecordId;
use adabt_core::policy::{Constraints, Mode, Policy, Priorities};
use adabt_core::record::Record;
use adabt_core::schema::{FieldDef, FieldType, Schema, SchemaMode};
use adabt_core::store::LogicalStore;
use adabt_engine::Database;
use adabt_ir::plan::{Agg, LogicalOp, LogicalPlan};
use adabt_ir::{CmpOp, Expr};
use std::path::{Path, PathBuf};

struct Tmp(PathBuf);
impl Tmp {
    fn new(tag: &str) -> Self {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "adabt-adaptive-{tag}-{}-{:?}",
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
        SchemaMode::Fixed,
        vec![
            FieldDef::new("id", FieldType::U64).required(),
            FieldDef::new("country", FieldType::Char(8)),
            FieldDef::new("age", FieldType::I64),
            FieldDef::new("notes", FieldType::Char(96)),
        ],
    )
    .unwrap()
}

const COUNTRIES: [&str; 4] = ["NO", "SE", "DK", "FI"];

fn adaptive(priority: Priorities) -> Policy {
    Policy {
        mode: Mode::Adaptive,
        priority,
        ..Policy::conventional()
    }
}

fn seed(db: &mut Database, n: u64) {
    db.create_collection("users", schema()).unwrap();
    for i in 0..n {
        db.insert(
            "users",
            RecordId(i),
            Record::new()
                .with("id", i)
                .with("country", COUNTRIES[(i % 4) as usize])
                .with("age", (i % 70) as i64)
                .with("notes", format!("note {i}")),
        )
        .unwrap();
    }
}

fn read_queries() -> Vec<LogicalPlan> {
    vec![
        LogicalPlan::new(LogicalOp::scan("users").filter(Expr::eq("country", "NO"))),
        LogicalPlan::new(LogicalOp::scan("users").filter(Expr::eq("country", "SE"))),
        LogicalPlan::new(LogicalOp::scan("users").filter(Expr::cmp("age", CmpOp::Gt, 40i64))),
        LogicalPlan::new(
            LogicalOp::scan("users").aggregate(vec!["country".into()], vec![Agg::count("n")]),
        ),
    ]
}

/// Drive a read-heavy workload, letting the driver observe between rounds.
fn run_workload(db: &mut Database, rounds: usize) {
    for _ in 0..rounds {
        for q in read_queries() {
            db.query(&q).unwrap();
        }
        for i in 0..200u64 {
            db.get("users", RecordId(i)).unwrap();
        }
        db.optimize().unwrap();
    }
}

#[test]
fn different_priorities_converge_on_different_configurations() {
    // The premise of the entire project, as one assertion.
    let ts = Tmp::new("speed");
    let tr = Tmp::new("resources");

    let mut fast = Database::open(
        ts.path(),
        adaptive(Priorities {
            speed: 10,
            resources: 2,
            freedom: 5,
        }),
    )
    .unwrap();
    seed(&mut fast, 5_000);
    run_workload(&mut fast, 6);

    let mut lean = Database::open(
        tr.path(),
        adaptive(Priorities {
            speed: 2,
            resources: 10,
            freedom: 5,
        }),
    )
    .unwrap();
    seed(&mut lean, 5_000);
    run_workload(&mut lean, 6);

    let fast_cfg = fast.config().describe();
    let lean_cfg = lean.config().describe();
    assert_ne!(
        fast_cfg, lean_cfg,
        "both priorities produced the same configuration, so priorities do nothing"
    );

    // The resource-priority database should have chosen compression; the
    // speed-priority one should not have paid its latency cost.
    assert!(
        lean.config().is_enabled_anywhere("record_compression"),
        "a resources-priority policy did not choose compression: {lean_cfg}\n{}",
        lean.explain_optimizations()
    );
    assert!(
        !fast.config().is_enabled_anywhere("record_compression"),
        "a speed-priority policy chose compression despite its latency cost: {fast_cfg}"
    );
}

#[test]
fn adaptation_never_changes_an_answer() {
    let baseline = {
        let t = Tmp::new("answers-manual");
        let mut db = Database::open(t.path(), Policy::manual(0)).unwrap();
        seed(&mut db, 5_000);
        read_queries()
            .iter()
            .map(|q| db.query(q).unwrap())
            .collect::<Vec<_>>()
    };

    for (tag, priority) in [
        (
            "speed",
            Priorities {
                speed: 10,
                resources: 2,
                freedom: 5,
            },
        ),
        (
            "lean",
            Priorities {
                speed: 2,
                resources: 10,
                freedom: 5,
            },
        ),
        ("balanced", Priorities::default()),
    ] {
        let t = Tmp::new(&format!("answers-{tag}"));
        let mut db = Database::open(t.path(), adaptive(priority)).unwrap();
        seed(&mut db, 5_000);
        run_workload(&mut db, 6);
        for (q, want) in read_queries().iter().zip(&baseline) {
            assert_eq!(
                &db.query(q).unwrap(),
                want,
                "{tag} adaptation changed an answer\nconfig: {}",
                db.config().describe()
            );
        }
    }
}

#[test]
fn the_driver_waits_for_evidence_before_touching_anything() {
    let t = Tmp::new("evidence");
    let mut db = Database::open(
        t.path(),
        adaptive(Priorities {
            speed: 10,
            resources: 5,
            freedom: 5,
        }),
    )
    .unwrap();
    db.create_collection("users", schema()).unwrap();
    // A handful of operations is startup, not a workload.
    for i in 0..20u64 {
        db.insert(
            "users",
            RecordId(i),
            Record::new().with("id", i).with("age", 1i64),
        )
        .unwrap();
    }
    db.optimize().unwrap();
    assert!(
        db.config().is_empty(),
        "the driver acted on 20 operations: {}",
        db.config().describe()
    );
}

#[test]
fn the_driver_settles_rather_than_oscillating() {
    // An optimizer that flips things on and off forever is strictly worse than
    // one that does nothing: every flip pays a rebuild and invalidates caches.
    let t = Tmp::new("stable");
    let mut db = Database::open(
        t.path(),
        adaptive(Priorities {
            speed: 8,
            resources: 4,
            freedom: 5,
        }),
    )
    .unwrap();
    seed(&mut db, 5_000);

    let mut configs = Vec::new();
    for _ in 0..14 {
        for q in read_queries() {
            db.query(&q).unwrap();
        }
        db.optimize().unwrap();
        configs.push(db.config().describe());
    }

    // The last several cycles must agree: the driver reached a fixed point.
    let tail = &configs[configs.len() - 4..];
    assert!(
        tail.iter().all(|c| c == &tail[0]),
        "the driver never settled:\n{}",
        configs.join("\n")
    );
}

#[test]
fn a_hard_memory_ceiling_is_respected_by_the_driver() {
    let t = Tmp::new("ceiling");
    let mut policy = adaptive(Priorities {
        speed: 10,
        resources: 1,
        freedom: 5,
    });
    policy.constraints = Constraints {
        // Tight enough that memory-hungry optimizations cannot all fit.
        max_ram_bytes: Some(64 * 1024),
        ..Constraints::default()
    };
    let mut db = Database::open(t.path(), policy).unwrap();
    seed(&mut db, 5_000);
    run_workload(&mut db, 6);

    let text = db.explain_optimizations();
    assert!(
        text.contains("exceeds constraints"),
        "a speed-hungry policy under a tiny ceiling should have hit it:\n{text}"
    );
}

#[test]
fn every_adaptive_decision_is_explained() {
    let t = Tmp::new("explain");
    let mut db = Database::open(
        t.path(),
        adaptive(Priorities {
            speed: 9,
            resources: 3,
            freedom: 5,
        }),
    )
    .unwrap();
    seed(&mut db, 5_000);
    run_workload(&mut db, 4);

    let text = db.explain_optimizations();
    assert!(text.contains("requested by: adaptive"), "{text}");
    // The rationale must carry the score that justified it, not just a name.
    assert!(text.contains("score"), "{text}");
}

#[test]
fn the_driver_drops_indexes_it_created_once_the_planner_stops_choosing_them() {
    // The M7 finding, made actionable: an index the planner never picks costs
    // write maintenance and memory for nothing, and watching queries arrive
    // cannot reveal that — only watching which access paths get chosen can.
    let t = Tmp::new("unused-index");
    let mut db = Database::open(
        t.path(),
        adaptive(Priorities {
            speed: 6,
            resources: 7,
            freedom: 5,
        }),
    )
    .unwrap();
    seed(&mut db, 2_000);

    // Phase one: equality queries, so the driver builds an index.
    let equality = LogicalPlan::new(LogicalOp::scan("users").filter(Expr::eq("country", "NO")));
    for _ in 0..6 {
        for _ in 0..120 {
            db.query(&equality).unwrap();
        }
        db.optimize().unwrap();
    }
    assert!(
        !db.index_specs().is_empty(),
        "the driver never built an index to begin with:\n{}",
        db.explain_optimizations()
    );

    // Phase two: the workload changes to range predicates, which a hash index
    // cannot serve. The index is now pure overhead.
    db.probe().reset();
    let range_only =
        LogicalPlan::new(LogicalOp::scan("users").filter(Expr::cmp("age", CmpOp::Gt, 30i64)));
    // Enough traffic per cycle to clear `MIN_OBSERVATIONS`, which telemetry
    // decay turned from a lifetime total into a sustained rate: a database that
    // has gone quiet is not one the optimizer should be redesigning.
    for _ in 0..8 {
        for _ in 0..300 {
            db.query(&range_only).unwrap();
        }
        db.optimize().unwrap();
    }

    let text = db.explain_optimizations();
    let specs = db.index_specs();
    assert!(
        !specs.iter().any(|s| s.field == "country"),
        "the index the planner never chose survived:\nconfig: {}\n{text}",
        db.config().describe()
    );
    assert!(
        text.contains("has not chosen"),
        "the drop was not explained:\n{text}"
    );

    // And the field the workload now filters on gets a structure that can serve
    // it. Proposing a hash index for a range predicate was the M7 loss.
    if let Some(age) = specs.iter().find(|s| s.field == "age") {
        assert_eq!(
            age.kind,
            adabt_core::index_kind::IndexKind::BTree,
            "a range-filtered field was given a hash index the planner cannot use"
        );
    }
}
