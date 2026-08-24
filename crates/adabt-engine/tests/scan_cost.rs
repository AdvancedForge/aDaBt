//! A full scan must read each record once.
//!
//! `Database::all_ids` served the executor's id list out of
//! `LogicalStore::scan`, which decodes every record in the collection — and
//! then discarded all of them, because only the ids were wanted. The executor
//! immediately fetched and decoded each of those records again. Every full
//! scan therefore read and decoded the collection twice, and the second pass
//! produced nothing that the first had not already produced.
//!
//! Nothing was wrong with the *answers*, which is why it survived: the rows
//! were correct, the order was correct, and the differential rig — which
//! compares results, not work — was blind to it by construction. It is a cost
//! bug, so the test has to observe cost.
//!
//! The instrument is the buffer pool. Every record read goes through
//! `pool.get`, so `hits + misses` counts record reads exactly, without
//! measuring time and without a threshold that drifts with the machine. A
//! scan of N records should cost about N page gets. Two passes cost 2N, which
//! is what this refuses.

use adabt_core::ids::RecordId;
use adabt_core::policy::Policy;
use adabt_core::record::Record;
use adabt_core::schema::Schema;
use adabt_core::store::LogicalStore;
use adabt_engine::Database;
use adabt_ir::plan::{LogicalOp, LogicalPlan};
use std::path::{Path, PathBuf};

struct Tmp(PathBuf);
impl Tmp {
    fn new(tag: &str) -> Self {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "adabt-scan-cost-{tag}-{}-{:?}",
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

const N: u64 = 2_000;

fn seeded(dir: &Path) -> Database {
    // Level 0: no indexes, no direct arrays, no column store. A scan here is
    // the plain heap path, which is the path under test — an optimization
    // that answered the query from somewhere else would hide the double read
    // rather than prove it gone.
    let mut db = Database::open(dir, Policy::manual(0)).unwrap();
    db.create_collection("c", Schema::dynamic()).unwrap();
    for i in 0..N {
        db.insert("c", RecordId(i), Record::new().with("i", i))
            .unwrap();
    }
    db
}

fn page_gets(db: &Database) -> u64 {
    let s = db.buffer_stats();
    s.hits + s.misses
}

#[test]
fn a_full_scan_reads_each_record_once() {
    let dir = Tmp::new("once");
    let mut db = seeded(dir.path());

    let plan = LogicalPlan::new(LogicalOp::Scan {
        collection: "c".into(),
    });

    let before = page_gets(&db);
    let rows = db.query(&plan).unwrap();
    let cost = page_gets(&db) - before;

    assert_eq!(
        rows.len(),
        N as usize,
        "the scan must still return every row"
    );

    // One get per record, plus whatever the plan itself touches. Anything at
    // or above 2N means the collection was walked twice; the old code scored
    // exactly 2N. The ceiling is deliberately loose — this is testing the
    // difference between one pass and two, not counting individual gets.
    assert!(
        cost < 2 * N,
        "a scan of {N} records cost {cost} page reads; \
         at or above {} that is the collection being read twice",
        2 * N
    );
}

#[test]
fn ids_agree_with_scan_exactly() {
    // `LogicalStore::ids` exists only as a cheaper way to answer a question
    // `scan` already answers, so the two must never disagree. Deletes are
    // included because that is where they could: `ids` reads the directory
    // directly and has to apply the same tombstone rule `scan` applies.
    let dir = Tmp::new("agree");
    let mut db = seeded(dir.path());
    for i in (0..N).step_by(7) {
        db.delete("c", RecordId(i)).unwrap();
    }

    let from_scan: Vec<RecordId> = db
        .scan("c")
        .unwrap()
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    let from_ids = db.ids("c").unwrap();

    assert_eq!(
        from_ids, from_scan,
        "ids() and scan() must return the same ids in the same order"
    );
    assert!(from_ids.len() < N as usize, "the deletes must have taken");
}

#[test]
fn ids_costs_nothing_to_answer() {
    // The point of the override: the page directory is already in memory, so
    // enumerating ids should not touch a page at all. If this ever regresses
    // to a scan the count goes to N and this fails.
    let dir = Tmp::new("free");
    let mut db = seeded(dir.path());

    let before = page_gets(&db);
    let ids = db.ids("c").unwrap();
    let cost = page_gets(&db) - before;

    assert_eq!(ids.len(), N as usize);
    assert_eq!(
        cost, 0,
        "enumerating ids read {cost} pages; it should read none"
    );
}
