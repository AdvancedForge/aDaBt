//! One-off probe: what does the planner choose for an indexed equality query
//! before and after `optimize()`, and does an experiment run at all?

use adabt_core::ids::RecordId;
use adabt_core::index_kind::IndexKind;
use adabt_core::policy::Policy;
use adabt_core::record::Record;
use adabt_core::schema::Schema;
use adabt_core::store::LogicalStore;
use adabt_engine::Database;
use adabt_ir::plan::{LogicalOp, LogicalPlan};
use adabt_ir::Expr;

fn main() {
    let rows = 50_000u64;
    let mut p = std::env::temp_dir();
    p.push(format!("adabt-probe-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    let mut policy = Policy::manual(4);
    policy.guarantees.durability = adabt_core::policy::Durability::Relaxed;
    let mut db = Database::open(&p, policy).expect("open");
    db.create_collection("users", Schema::dynamic()).unwrap();
    let batch: Vec<(RecordId, Record)> = (0..rows)
        .map(|i| {
            let c = ["NO", "SE", "DK", "FI", "IS", "NL", "BE", "IE"][(i % 8) as usize];
            (
                RecordId(i),
                Record::new().with("id", i).with("country", c).with("age", (18 + i % 60) as i64),
            )
        })
        .collect();
    db.insert_batch("users", batch).unwrap();
    db.create_index("users", "country", IndexKind::Hash).unwrap();

    let plan = LogicalPlan::new(
        LogicalOp::scan("users")
            .filter(Expr::eq("country", "NO"))
            .project(vec!["age".into()]),
    );

    let t = std::time::Instant::now();
    let n = db.query(&plan).unwrap().len();
    println!("before optimize: {:?} for {n} rows", t.elapsed());
    println!("last_stats: {:?}", db.last_exec_stats());
    println!("plan:\n{}", db.explain(&plan));
    for _ in 0..40 { db.query(&plan).unwrap(); }
    let report = db.optimize().unwrap();
    println!("\noptimize report: {:?}", report.applied);
    println!("live experiments: {}", db.experiments().count());

    // Settle any experiment like the harness now does.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while db.experiments().next().is_some() {
        for _ in 0..50 { let _ = db.query(&plan); }
        db.advance_experiments().unwrap();
        if std::time::Instant::now() > deadline {
            db.abort_experiment("probe deadline").unwrap();
            break;
        }
    }
    println!("finished experiments: {}", db.finished_experiments().len());
    for e in db.finished_experiments() {
        println!("  verdict: {} -> {:?}", e.experiment.decision.optimization, e.phase());
    }

    let t = std::time::Instant::now();
    let n = db.query(&plan).unwrap().len();
    println!("\nafter optimize: {:?} for {n} rows", t.elapsed());
    println!("last_stats: {:?}", db.last_exec_stats());
    println!("plan:\n{}", db.explain(&plan));

    // And the grouped aggregate, which got WORSE after tuning in the
    // comparison harness — explain why before publishing that.
    use adabt_ir::plan::Agg;
    let gplan = LogicalPlan::new(
        LogicalOp::scan("users").aggregate(vec!["country".into()], vec![Agg::count("n")]),
    );
    let t = std::time::Instant::now();
    let n = db.query(&gplan).unwrap().len();
    println!("\ngroup-by after optimize: {:?} for {n} groups", t.elapsed());
    for k in 1..=3 {
        let t = std::time::Instant::now();
        db.query(&gplan).unwrap();
        println!("group-by repeat #{k}: {:?}", t.elapsed());
    }
    // Same question without any tuning, as a control.
    println!("matviews before any of the above:\n{}", db.explain_materialized_views());
    println!("last_stats: {:?}", db.last_exec_stats());
    println!("plan:\n{}", db.explain(&gplan));
    println!("\nmaterialized views:\n{}", db.explain_materialized_views());
    drop(db);
    let _ = std::fs::remove_dir_all(&p);
}
