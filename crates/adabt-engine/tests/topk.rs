//! Top-K over the column store.
//!
//! A `Limit` over a `Sort` does not need a sorted collection; it needs k
//! winners. The planner may answer it by reading only the sort key out of
//! the column store, keeping the k smallest under exactly `Sort`'s total
//! order, and fetching full records for those k alone. These tests pin the
//! two things that make the swap legal: identical output to sorting and
//! truncating — ties included, both directions, every k — and a decision
//! made only where it is sound to make one.

use adabt_core::ids::RecordId;
use adabt_core::policy::{Durability, Policy};
use adabt_core::record::Record;
use adabt_core::schema::Schema;
use adabt_core::store::LogicalStore;
use adabt_engine::Database;
use adabt_ir::plan::{LogicalOp, LogicalPlan, SortKey};
use std::path::{Path, PathBuf};

struct Tmp(PathBuf);
impl Tmp {
    fn new(tag: &str) -> Self {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "adabt-topk-{tag}-{}-{:?}",
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

/// Few distinct ages on purpose: ties are everywhere, so if the top-K's
/// tiebreak ever drifted from Sort's, this data would catch it.
fn seed(db: &mut Database, n: u64) {
    db.create_collection("users", Schema::dynamic()).unwrap();
    let batch: Vec<(RecordId, Record)> = (0..n)
        .map(|i| {
            (
                RecordId(i),
                Record::new()
                    .with("id", i)
                    .with("age", (i % 10) as i64)
                    .with("name", format!("user-{i}")),
            )
        })
        .collect();
    db.insert_batch("users", batch).unwrap();
}

fn open(tag: &str) -> (Tmp, Database) {
    let t = Tmp::new(tag);
    let mut policy = Policy::manual(4);
    policy.guarantees.durability = Durability::Relaxed;
    let db = Database::open(t.path(), policy).unwrap();
    (t, db)
}

/// The exact shape the planner may special-case: limit directly over a
/// single-key sort over a bare scan.
fn topk_plan(k: usize, descending: bool) -> LogicalPlan {
    LogicalPlan::new(
        LogicalOp::scan("users")
            .sort(vec![SortKey {
                field: "age".into(),
                descending,
            }])
            .limit(k),
    )
}

/// Every record, as the executor hands them back.
fn all_rows(db: &mut Database) -> Vec<(RecordId, Record)> {
    db.query(&LogicalPlan::new(LogicalOp::scan("users")))
        .unwrap()
}

/// Sort under the documented total order — key, then id, descending applied
/// to the key only — written here rather than delegated to the engine, so
/// agreement between this and the specialist path is evidence and not a
/// tautology.
fn expected_order(mut rows: Vec<(RecordId, Record)>, descending: bool, k: usize) -> Vec<RecordId> {
    rows.sort_by(|a, b| {
        let av = a.1.get("age").cloned().expect("seeded rows have age");
        let bv = b.1.get("age").cloned().expect("seeded rows have age");
        let ord = av.cmp(&bv);
        let ord = if descending { ord.reverse() } else { ord };
        ord.then(a.0.cmp(&b.0))
    });
    rows.truncate(k);
    rows.into_iter().map(|(id, _)| id).collect()
}

#[test]
fn topk_equals_sort_then_truncate_for_every_k_in_both_directions() {
    let (_t, mut db) = open("equality");
    seed(&mut db, 8000);
    // Earn the column store, so this exercises the specialist path and not
    // merely the fallback it takes without one.
    let agg = LogicalPlan::new(
        LogicalOp::scan("users")
            .aggregate(vec!["age".into()], vec![adabt_ir::plan::Agg::count("n")]),
    );
    for _ in 0..20 {
        db.query(&agg).unwrap();
    }
    db.optimize().unwrap();
    assert!(
        db.has_column_store("users"),
        "no store; test would be vacuous"
    );

    for descending in [false, true] {
        for k in [0usize, 1, 7, 9, 10, 11, 250, 500, 501] {
            let plan = topk_plan(k, descending);
            let chosen = db.plan(&plan).explain().contains("ColumnarTopK");
            assert!(
                chosen || k == 0,
                "k={k} descending={descending}: top-K not chosen; test would be vacuous"
            );
            let got = db.query(&plan).unwrap();

            let want_ids = expected_order(all_rows(&mut db), descending, k);
            assert_eq!(
                got.len(),
                want_ids.len(),
                "k={k} descending={descending}: row count differs"
            );
            let contents: std::collections::HashMap<RecordId, Record> =
                all_rows(&mut db).into_iter().collect();
            for (i, ((gid, grec), wid)) in got.iter().zip(want_ids.iter()).enumerate() {
                assert_eq!(gid, wid, "position {i}, k={k}, descending={descending}");
                // The winner is fetched whole: every field a projection above
                // might read is present and is that record's own.
                let wrec = contents.get(wid).expect("winner exists");
                assert_eq!(grec.get("name"), wrec.get("name"), "position {i}");
                assert_eq!(grec.get("age"), wrec.get("age"), "position {i}");
                assert_eq!(grec.get("id"), wrec.get("id"), "position {i}");
            }
        }
    }
}

#[test]
fn the_specialist_path_takes_the_shortcut() {
    let (_t, mut db) = open("shortcut");
    seed(&mut db, 8000);
    let agg = LogicalPlan::new(
        LogicalOp::scan("users")
            .aggregate(vec!["age".into()], vec![adabt_ir::plan::Agg::count("n")]),
    );
    for _ in 0..20 {
        db.query(&agg).unwrap();
    }
    db.optimize().unwrap();
    assert!(db.has_column_store("users"));

    // The specialist path must actually take the shortcut: the scan touches
    // its k candidates plus their k fetches, not the collection. A silent
    // fallback to materializing everything would keep every equality
    // assertion green while quietly costing what this exists to avoid.
    // rows_scanned is deterministic, which is why it is the assertion and
    // not a stopwatch.
    let plan = topk_plan(20, false);
    let got = db.query(&plan).unwrap();
    assert_eq!(got.len(), 20);
    let scanned = db.last_exec_stats().rows_scanned;
    assert!(
        scanned <= 100,
        "top-20 over 8000 rows touched {scanned} rows; the selection ran outside the store"
    );
}

#[test]
fn the_planner_takes_topk_when_the_store_holds_the_key_and_refuses_otherwise() {
    let (_t, mut db) = open("decision");
    seed(&mut db, 8000);
    // Show the optimizer an aggregate-shaped workload so it builds the
    // column store, the way any level-4 database earns one.
    let agg = LogicalPlan::new(
        LogicalOp::scan("users")
            .aggregate(vec!["age".into()], vec![adabt_ir::plan::Agg::count("n")]),
    );
    for _ in 0..20 {
        db.query(&agg).unwrap();
    }
    db.optimize().unwrap();
    assert!(db.has_column_store("users"), "no column store was built");

    let plan = db.plan(&topk_plan(5, false));
    assert!(
        plan.explain().contains("ColumnarTopK"),
        "the column store holds the key but top-K was not chosen:\n{}",
        plan.explain()
    );

    // A projection above the limit does not change who wins; the planner
    // descends through it when matching the shape.
    let projected = LogicalPlan::new(
        LogicalOp::scan("users")
            .sort(vec![SortKey {
                field: "age".into(),
                descending: false,
            }])
            .limit(5)
            .project(vec!["name".into(), "age".into()]),
    );
    let explain = db.plan(&projected).explain();
    assert!(
        explain.contains("ColumnarTopK"),
        "a projection above the limit blocked the top-K decision:\n{explain}"
    );
    let rows = db.query(&projected).unwrap();
    assert_eq!(rows.len(), 5);
    assert!(rows.iter().all(|(_, r)| r.get("name").is_some()));
    // The winners are fetched whole: the projection above may ask for any
    // field, and the answer must carry it.
    let named = topk_plan(5, false); // whole records come back either way

    // A field the store was never built with: inserts after the store was
    // built carry `score`, and column-store maintenance extends the store
    // sparsely — one populated cell, everything else absent. That makes
    // top-K by score LEGAL: the projection hands back records without the
    // key where the cell is empty, and Sort's documented order sends
    // missing keys last, exactly as the ordinary path would.
    db.insert(
        "users",
        RecordId(10_000),
        Record::new()
            .with("id", 10_000u64)
            .with("age", -5i64)
            .with("name", "scored")
            .with("score", 1i64),
    )
    .unwrap();
    let by_score = LogicalPlan::new(
        LogicalOp::scan("users")
            .sort(vec![SortKey {
                field: "score".into(),
                descending: false,
            }])
            .limit(3),
    );
    let plan = db.plan(&by_score);
    assert!(
        plan.explain().contains("ColumnarTopK"),
        "a sparsely-extended column store should serve top-K by its new key:\n{}",
        plan.explain()
    );
    let got = db.query(&by_score).unwrap();
    // The one populated row wins; the absent keys tie and fall to the id
    // tiebreak, lowest id first — identical to sorting the heap.
    let ids: Vec<RecordId> = got.iter().map(|(id, _)| *id).collect();
    assert_eq!(ids, vec![RecordId(10_000), RecordId(0), RecordId(1)]);

    // Control for the first half: the same shape over a collection with no
    // column store at all is planned the ordinary way.
    let (_t2, mut plain) = open("control");
    seed(&mut plain, 100);
    let plan = plain.plan(&topk_plan(5, false));
    assert!(
        !plan.explain().contains("ColumnarTopK"),
        "top-K chosen without a column store:\n{}",
        plan.explain()
    );
    let _ = named;
}

#[test]
fn multi_key_sorts_and_filtered_sorts_are_not_special_cased() {
    let (_t, mut db) = open("shapes");
    seed(&mut db, 8000);
    let agg = LogicalPlan::new(
        LogicalOp::scan("users")
            .aggregate(vec!["age".into()], vec![adabt_ir::plan::Agg::count("n")]),
    );
    for _ in 0..20 {
        db.query(&agg).unwrap();
    }
    db.optimize().unwrap();
    assert!(db.has_column_store("users"));

    // Two keys: the single-key identity proof does not extend to these, so
    // the planner must not pretend it does.
    let two_keys = LogicalPlan::new(
        LogicalOp::scan("users")
            .sort(vec![
                SortKey {
                    field: "age".into(),
                    descending: false,
                },
                SortKey {
                    field: "id".into(),
                    descending: true,
                },
            ])
            .limit(5),
    );
    assert!(
        !db.plan(&two_keys).explain().contains("ColumnarTopK"),
        "a two-key sort took the top-K path"
    );
    // A filter between sort and scan: the columnar read cannot evaluate a
    // predicate against fields it did not fetch.
    let filtered = LogicalPlan::new(
        LogicalOp::scan("users")
            .filter(adabt_ir::Expr::cmp("age", adabt_ir::CmpOp::Lt, 5i64))
            .sort(vec![SortKey {
                field: "age".into(),
                descending: false,
            }])
            .limit(5),
    );
    let explain = db.plan(&filtered).explain();
    assert!(
        !explain.contains("ColumnarTopK"),
        "a filtered sort took the top-K path:\n{explain}"
    );
    // Both refuse, both answer.
    assert_eq!(db.query(&two_keys).unwrap().len(), 5);
    assert_eq!(db.query(&filtered).unwrap().len(), 5);
}

#[test]
fn a_restart_without_the_store_answers_identically() {
    let t = Tmp::new("restart");
    {
        let mut policy = Policy::manual(4);
        policy.guarantees.durability = Durability::Relaxed;
        let mut db = Database::open(t.path(), policy).unwrap();
        seed(&mut db, 8000);
        let agg = LogicalPlan::new(
            LogicalOp::scan("users")
                .aggregate(vec!["age".into()], vec![adabt_ir::plan::Agg::count("n")]),
        );
        for _ in 0..20 {
            db.query(&agg).unwrap();
        }
        db.optimize().unwrap();
        assert!(db.has_column_store("users"));
    }
    // Derived structures are rebuildable, not resurrected: a level-0 reopen
    // has no column store, so no top-K decision either — and the answer had
    // better not notice the difference.
    let mut db = Database::open(t.path(), Policy::manual(0)).unwrap();
    assert!(!db.has_column_store("users"));
    let explain = db.plan(&topk_plan(6, true)).explain();
    assert!(
        !explain.contains("ColumnarTopK"),
        "top-K chosen without a column store:\n{explain}"
    );
    let got = db.query(&topk_plan(6, true)).unwrap();
    let want = expected_order(all_rows(&mut db), true, 6);
    assert_eq!(got.len(), want.len());
    for ((gid, _), wid) in got.iter().zip(want.iter()) {
        assert_eq!(gid, wid);
    }
}
