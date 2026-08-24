//! The slow-query log: an opt-in sink for queries that take at least a
//! configured threshold, end to end.

use adabt_core::ids::RecordId;
use adabt_core::policy::Policy;
use adabt_core::record::Record;
use adabt_core::schema::Schema;
use adabt_core::store::LogicalStore;
use adabt_engine::Database;
use adabt_ir::plan::{LogicalOp, LogicalPlan};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

struct Tmp(PathBuf);
impl Tmp {
    fn new(tag: &str) -> Self {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "adabt-slow-query-{tag}-{}-{:?}",
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

fn seeded(dir: &Path, n: u64) -> Database {
    let mut db = Database::open(dir, Policy::conventional()).unwrap();
    db.create_collection("c", Schema::dynamic()).unwrap();
    for i in 0..n {
        db.insert("c", RecordId(i), Record::new().with("i", i))
            .unwrap();
    }
    db
}

#[test]
fn a_query_past_the_threshold_reaches_the_sink() {
    let t = Tmp::new("over");
    let mut db = seeded(t.path(), 100);
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&events);
    // Zero: any measurable query duration crosses it.
    db.set_slow_query_sink(Duration::ZERO, move |e| {
        sink.lock()
            .unwrap()
            .push((e.rows_scanned, e.rows_returned, e.explain.clone()));
    });

    db.query(&LogicalPlan::new(LogicalOp::scan("c"))).unwrap();

    let seen = events.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].0, 100, "rows_scanned");
    assert_eq!(seen[0].1, 100, "rows_returned");
    assert!(seen[0].2.contains("logical"), "{}", seen[0].2);
}

#[test]
fn a_query_under_the_threshold_never_reaches_the_sink() {
    let t = Tmp::new("under");
    let mut db = seeded(t.path(), 100);
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&events);
    db.set_slow_query_sink(Duration::from_secs(3600), move |e| {
        sink.lock().unwrap().push(e.rows_scanned);
    });

    db.query(&LogicalPlan::new(LogicalOp::scan("c"))).unwrap();

    assert!(events.lock().unwrap().is_empty());
}

#[test]
fn no_sink_configured_means_no_cost_and_no_events() {
    let t = Tmp::new("none");
    let mut db = seeded(t.path(), 100);
    // Nothing configured — just proving this does not panic or misbehave.
    let rows = db.query(&LogicalPlan::new(LogicalOp::scan("c"))).unwrap();
    assert_eq!(rows.len(), 100);
}

#[test]
fn disabling_the_log_stops_further_events() {
    let t = Tmp::new("disable");
    let mut db = seeded(t.path(), 50);
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&events);
    db.set_slow_query_sink(Duration::ZERO, move |_| {
        sink.lock().unwrap().push(());
    });
    db.query(&LogicalPlan::new(LogicalOp::scan("c"))).unwrap();
    assert_eq!(events.lock().unwrap().len(), 1);

    db.disable_slow_query_log();
    db.query(&LogicalPlan::new(LogicalOp::scan("c"))).unwrap();
    assert_eq!(
        events.lock().unwrap().len(),
        1,
        "no new event after disabling"
    );
}
