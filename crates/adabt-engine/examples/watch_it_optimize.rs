//! The reason this database exists, in one runnable file.
//!
//! Same queries before and after. Nobody configures anything. The engine
//! watches the traffic, proposes a structure, proves it on shadowed paths,
//! and only then serves traffic through it — and the plan text changes while
//! the answers do not.
//!
//! Run with: cargo run -p adabt-engine --example watch_it_optimize

use adabt_core::ids::RecordId;
use adabt_core::policy::{Durability, Policy};
use adabt_core::record::Record;
use adabt_core::schema::Schema;
use adabt_core::store::LogicalStore;
use adabt_engine::Database;
use adabt_ir::plan::{Agg, LogicalOp, LogicalPlan};
use std::time::Instant;

fn main() {
    let mut dir = std::env::temp_dir();
    dir.push(format!("adabt-watch-{}", std::process::id()));

    // Level 6 permits the workload-aware proposals: indexes chosen from what
    // the traffic does rather than from the schema.
    let mut policy = Policy::manual(6);
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

    let t = Instant::now();
    let before = db.query(&by_region).unwrap();
    let plain = t.elapsed();
    println!("\nbefore optimize(): {} groups in {plain:?}", before.len());
    println!("{}", db.plan(&by_region).explain());

    println!("\noptimize() — propose, prove, promote…");
    let report = db.optimize().unwrap();
    println!("applied: {:?}", report.applied);

    // The first query after a structure lands may pay its construction; a
    // benchmark that times that moment measures the building, not the
    // building's worth. Steady state is the honest comparison.
    let _ = db.query(&by_region).unwrap();

    let t = Instant::now();
    let after = db.query(&by_region).unwrap();
    let tuned = t.elapsed();
    println!("\nafter optimize():  {} groups in {tuned:?}", after.len());
    println!("{}", db.plan(&by_region).explain());
    println!("\n{}", db.explain_materialized_views());

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
