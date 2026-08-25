//! The smallest useful aDaBt: open a database, put records in, query them
//! out through a logical plan.
//!
//! Run with: cargo run -p adabt-engine --example quickstart

use adabt_core::ids::RecordId;
use adabt_core::policy::{Durability, Policy};
use adabt_core::record::Record;
use adabt_core::schema::Schema;
use adabt_core::store::LogicalStore;
use adabt_engine::Database;
use adabt_ir::plan::{LogicalOp, LogicalPlan};
use adabt_ir::Expr;

fn main() {
    let mut dir = std::env::temp_dir();
    dir.push(format!("adabt-quickstart-{}", std::process::id()));

    // Level 0 is the conventional configuration: nothing specialised, every
    // structure plain. The database earns its fanciness from here if you let
    // it — see the watch_it_optimize example.
    let mut policy = Policy::manual(0);
    // Relaxed keeps this example fast; leave Strict on and every write fsyncs,
    // which is what you want when the data matters more than the demo.
    policy.guarantees.durability = Durability::Relaxed;
    let mut db = Database::open(&dir, policy).expect("open");

    db.create_collection("users", Schema::dynamic()).unwrap();
    let batch: Vec<(RecordId, Record)> = (0..1000)
        .map(|i| {
            (
                RecordId(i),
                Record::new()
                    .with("id", i)
                    .with("country", ["NO", "SE", "DK", "FI"][(i % 4) as usize])
                    .with("age", (18 + i % 60) as i64),
            )
        })
        .collect();
    db.insert_batch("users", batch).unwrap();

    // Queries are logical plans: WHAT to ask, never HOW. The planner picks
    // the access path, and may pick a different one next Tuesday without the
    // question changing.
    let adults_in_norway = LogicalPlan::new(
        LogicalOp::scan("users")
            .filter(Expr::And(vec![
                Expr::eq("country", "NO"),
                Expr::cmp("age", adabt_ir::CmpOp::Ge, 18i64),
            ]))
            .project(vec!["age".into()]),
    );

    let rows = db.query(&adults_in_norway).unwrap();
    println!("{} adult Norwegians", rows.len());
    println!("{}", db.explain(&adults_in_norway));

    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}
