//! The reason this database exists, in one runnable file.
//!
//! Same queries before and after. Nobody configures anything. The engine
//! watches the traffic, proposes a structure, proves it on shadowed paths,
//! and only then serves traffic through it — and the plan text changes while
//! the answers do not.
//!
//! Run with: cargo run -p adabt-engine --example watch_it_optimize

use adabt_core::ids::RecordId;
use adabt_core::policy::{Durability, Mode, Override, Policy};
use adabt_core::record::Record;
use adabt_core::schema::Schema;
use adabt_core::store::LogicalStore;
use adabt_engine::Database;
use adabt_ir::plan::{Agg, LogicalOp, LogicalPlan};
use std::time::{Duration, Instant};

/// Queries timed on each side; the median of them is what is reported.
const TIMED_RUNS: usize = 51;

/// Untimed queries run first on each side, so the timing measures the
/// steady state rather than caches, branch predictors and freshly allocated
/// memory warming up after a change.
const WARMUP_RUNS: usize = 20;

/// The median latency of `plan` over [`TIMED_RUNS`] queries, after
/// [`WARMUP_RUNS`] untimed ones. One timing of a query this fast is mostly
/// noise; a median of many is what a reader can reproduce.
fn median_latency(db: &mut Database, plan: &LogicalPlan) -> Duration {
    for _ in 0..WARMUP_RUNS {
        db.query(plan).unwrap();
    }
    let mut times: Vec<Duration> = (0..TIMED_RUNS)
        .map(|_| {
            let t = Instant::now();
            db.query(plan).unwrap();
            t.elapsed()
        })
        .collect();
    times.sort_unstable();
    times[TIMED_RUNS / 2]
}

fn main() {
    let mut dir = std::env::temp_dir();
    dir.push(format!("adabt-watch-{}", std::process::id()));

    // Level 6 permits the workload-aware proposals: structures chosen from
    // what the traffic does rather than from the schema. The result cache is
    // switched off so the timings below measure answering the query, not
    // replaying a cached answer.
    let mut policy = Policy::manual(6);
    if let Mode::Manual { overrides, .. } = &mut policy.mode {
        overrides.push(Override::toggle("result_cache", false));
    }
    policy.guarantees.durability = Durability::Relaxed;
    let mut db = Database::open(&dir, policy).expect("open");
    db.create_collection("events", Schema::dynamic()).unwrap();

    println!("seeding…");
    let batch: Vec<(RecordId, Record)> = (0..50_000)
        .map(|i| {
            (
                RecordId(i),
                Record::new()
                    .with("id", i)
                    .with("kind", ["click", "view", "buy"][(i % 3) as usize])
                    .with("region", ["eu", "us", "ap"][(i % 3) as usize])
                    .with("amount", (i % 100) as i64),
            )
        })
        .collect();
    db.insert_batch("events", batch).unwrap();

    let by_region = LogicalPlan::new(
        LogicalOp::scan("events").aggregate(vec!["region".into()], vec![Agg::count("n")]),
    );

    // Teach the optimizer what this workload looks like. In production this
    // is just... the workload happening.
    println!("warming the telemetry…");
    for _ in 0..30 {
        db.query(&by_region).unwrap();
    }

    let before = db.query(&by_region).unwrap();
    let plain = median_latency(&mut db, &by_region);
    println!(
        "\nbefore optimize(): {} groups in {plain:?} (median of {TIMED_RUNS})",
        before.len()
    );
    println!("{}", db.plan(&by_region).explain());

    println!("\noptimize() — propose, prove, promote…");
    let report = db.optimize().unwrap();
    println!("applied: {:?}", report.applied);

    // The first queries after a structure lands pay for its construction and
    // for cold caches; `median_latency` runs those untimed, so this measures
    // what the structure is worth rather than the moment it arrived.
    let after = db.query(&by_region).unwrap();
    let tuned = median_latency(&mut db, &by_region);
    println!(
        "\nafter optimize():  {} groups in {tuned:?} (median of {TIMED_RUNS})",
        after.len()
    );
    println!("{}", db.plan(&by_region).explain());
    // The plan above is what a cache miss would run; a maintained view
    // answers this shape before planning is reached at all.
    println!("\nmaintained views:\n{}", db.explain_materialized_views());

    assert_eq!(before, after, "optimization changed the answer");
    if tuned < plain {
        println!(
            "\nfaster by {:.1}x — with identical rows.",
            plain.as_secs_f64() / tuned.as_secs_f64().max(1e-9)
        );
    } else {
        println!("\nnot faster here; the optimizer keeps structures that pay where it matters.");
    }

    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}
