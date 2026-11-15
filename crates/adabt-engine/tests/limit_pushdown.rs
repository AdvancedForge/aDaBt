//! A `Limit` pushed into an index fetch must return what level 0 returns.
//!
//! Level 0 has no indexes, so every query below is a scan, a filter and a
//! truncation there: the plain meaning of the query. The other databases hold
//! the same rows behind b-tree and hash indexes, where the planner pushes the
//! limit into the fetch (`LimitedFetch`) whenever only filters and
//! projections stand between them. Every query is asked of both, across
//! limits, residual filters the index cannot answer, sorts and aggregates
//! that must block the push, and deletes and updates inside the range, and
//! the answers must be identical, order included.

use adabt_core::ids::RecordId;
use adabt_core::index_kind::IndexKind;
use adabt_core::policy::Policy;
use adabt_core::record::Record;
use adabt_core::schema::{FieldDef, FieldType, Schema, SchemaMode};
use adabt_core::store::LogicalStore;
use adabt_engine::Database;
use adabt_ir::plan::{Agg, AggKind, LogicalOp, LogicalPlan, SortKey};
use adabt_ir::{CmpOp, Expr};
use std::path::{Path, PathBuf};

struct Tmp(PathBuf);
impl Tmp {
    fn new(tag: &str) -> Self {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "adabt-limitpush-{tag}-{}-{:?}",
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
            FieldDef::new("balance", FieldType::I64),
        ],
    )
    .unwrap()
}

const COUNTRIES: [&str; 4] = ["NO", "SE", "DK", "FI"];
const ROWS: u64 = 3_000;

fn user(i: u64) -> Record {
    // Ages are scattered over ids (a multiplicative hash, not `i % 60`), so
    // the index's key order and the answer's id order genuinely disagree:
    // the first rows of a range by key are not the first by id.
    Record::new()
        .with("id", i)
        .with("country", COUNTRIES[(i * 7 % 4) as usize])
        .with("age", (i * 37 % 61) as i64)
        .with("balance", (i * 13 % 5000) as i64)
}

fn seed(db: &mut Database) {
    db.create_collection("users", schema()).unwrap();
    for i in 0..ROWS {
        db.insert("users", RecordId(i), user(i)).unwrap();
    }
}

/// Deletes and updates inside and across the ranges the queries ask about:
/// a deleted row must not be returned or counted toward a limit, and a row
/// whose age moved must be judged on its new age.
fn churn(db: &mut Database) {
    for i in 0..ROWS {
        if i % 5 == 0 {
            db.delete("users", RecordId(i)).unwrap();
        } else if i % 11 == 0 {
            let mut r = user(i);
            r.set("age", ((i * 37 + 20) % 61) as i64);
            r.set("country", "ZZ");
            db.update("users", RecordId(i), r).unwrap();
        }
    }
    // Re-insert some deleted ids with fresh ages, so the index holds ids
    // that were removed and came back under another key.
    for i in (0..ROWS).step_by(15) {
        let mut r = user(i);
        r.set("age", 25i64);
        db.insert("users", RecordId(i), r).unwrap();
    }
}

fn indexed(tag: &str, level: u8) -> (Tmp, Database) {
    let t = Tmp::new(tag);
    let mut db = Database::open(t.path(), Policy::manual(level)).unwrap();
    seed(&mut db);
    db.create_index("users", "age", IndexKind::BTree).unwrap();
    db.create_index("users", "country", IndexKind::Hash)
        .unwrap();
    (t, db)
}

fn baseline(tag: &str) -> (Tmp, Database) {
    let t = Tmp::new(tag);
    let mut db = Database::open(t.path(), Policy::manual(0)).unwrap();
    seed(&mut db);
    (t, db)
}

fn filters() -> Vec<Expr> {
    vec![
        // Fully answered by the index.
        Expr::cmp("age", CmpOp::Gt, 40i64),
        Expr::cmp("age", CmpOp::Ge, 59i64),
        Expr::cmp("age", CmpOp::Le, 3i64),
        Expr::And(vec![
            Expr::cmp("age", CmpOp::Ge, 20i64),
            Expr::cmp("age", CmpOp::Lt, 25i64),
        ]),
        // An empty range.
        Expr::cmp("age", CmpOp::Gt, 1_000i64),
        // A residual the index cannot answer: the limit counts after it.
        Expr::And(vec![
            Expr::cmp("age", CmpOp::Ge, 10i64),
            Expr::eq("country", "SE"),
        ]),
        Expr::And(vec![
            Expr::cmp("age", CmpOp::Lt, 30i64),
            Expr::cmp("balance", CmpOp::Gt, 4_900i64),
        ]),
        // Equality through a hash index, with and without a residual.
        Expr::eq("country", "DK"),
        Expr::And(vec![
            Expr::eq("country", "ZZ"),
            Expr::cmp("balance", CmpOp::Lt, 2_500i64),
        ]),
    ]
}

fn queries() -> Vec<LogicalPlan> {
    let mut out = Vec::new();
    for f in filters() {
        for n in [0usize, 1, 7, 50, 333, 100_000] {
            let base = || LogicalOp::scan("users").filter(f.clone());
            out.push(base().limit(n));
            out.push(base().project(vec!["id".into(), "age".into()]).limit(n));
            out.push(
                base()
                    .filter(Expr::cmp("balance", CmpOp::Ge, 1_000i64))
                    .limit(n),
            );
            out.push(base().limit(n).limit(n / 2 + 1));
            // Orders the index does not give: the limit must stay above.
            for (field, descending) in [("age", false), ("age", true), ("balance", true)] {
                out.push(
                    base()
                        .sort(vec![SortKey {
                            field: field.into(),
                            descending,
                        }])
                        .limit(n),
                );
            }
            // An aggregate above the limit counts groups, not rows; one below
            // it sees only the limited rows.
            out.push(
                base()
                    .aggregate(
                        vec!["country".into()],
                        vec![Agg::count("n"), Agg::over(AggKind::Sum, "balance", "s")],
                    )
                    .limit(n),
            );
            out.push(base().limit(n).aggregate(
                vec!["country".into()],
                vec![Agg::count("n"), Agg::over(AggKind::Max, "age", "m")],
            ));
        }
    }
    out.into_iter().map(LogicalPlan::new).collect()
}

fn assert_same(want: &mut Database, got: &mut Database, label: &str) {
    for q in queries() {
        let a = want.query(&q).unwrap();
        let b = got.query(&q).unwrap();
        assert_eq!(a, b, "{label}: answer changed for:\n{}", got.explain(&q));
    }
}

#[test]
fn every_pushed_limit_answers_as_level_zero_does() {
    let (_ta, mut want) = baseline("base");
    for level in [0u8, 1, 3, 10] {
        let (_tb, mut got) = indexed(&format!("idx{level}"), level);
        assert_same(&mut want, &mut got, &format!("level {level}, fresh"));
    }
}

#[test]
fn deletes_and_updates_in_the_range_answer_as_level_zero_does() {
    let (_ta, mut want) = baseline("churn-base");
    churn(&mut want);
    for level in [0u8, 1, 10] {
        let (_tb, mut got) = indexed(&format!("churn{level}"), level);
        churn(&mut got);
        assert_same(&mut want, &mut got, &format!("level {level}, after churn"));
    }
}

#[test]
fn the_pushed_limit_is_what_the_plan_says_and_bounds_the_fetches() {
    let (_t, mut db) = indexed("bounded", 0);
    let q = LogicalPlan::new(
        LogicalOp::scan("users")
            .filter(Expr::And(vec![
                Expr::cmp("age", CmpOp::Ge, 20i64),
                Expr::cmp("age", CmpOp::Lt, 25i64),
            ]))
            .limit(50),
    );
    let e = db.explain(&q);
    assert!(e.contains("LimitedFetch(first 50 by id"), "{e}");
    assert!(e.contains("IndexRange(users.age via btree)"), "{e}");
    let rows = db.query(&q).unwrap();
    assert_eq!(rows.len(), 50);
    let s = db.last_exec_stats();
    // About 5/61 of 3,000 rows are in the range, ~245: before the push every
    // one of them was fetched. A filter the index answers fully rejects
    // nothing, so exactly the 50 returned rows are fetched.
    assert_eq!(s.rows_scanned, 50, "{s:?}");
    assert_eq!(s.index_misses, 0, "{s:?}");
}

#[test]
fn a_residual_filter_is_judged_before_the_limit_counts() {
    let (_t, mut db) = indexed("residual", 0);
    let (_tb, mut want) = baseline("residual-base");
    let q = LogicalPlan::new(
        LogicalOp::scan("users")
            .filter(Expr::And(vec![
                Expr::cmp("age", CmpOp::Ge, 10i64),
                Expr::eq("country", "SE"),
            ]))
            .limit(20),
    );
    let rows = db.query(&q).unwrap();
    assert_eq!(rows, want.query(&q).unwrap());
    assert_eq!(rows.len(), 20, "the residual used up the limit");
    for (_, r) in &rows {
        assert_eq!(r.get("country"), Some(&"SE".into()));
    }
    let s = db.last_exec_stats();
    assert!(
        s.rows_scanned > 20 && s.rows_scanned < 200,
        "fetched {} rows for 20 survivors",
        s.rows_scanned
    );
}

#[test]
fn a_snapshot_taken_before_deletes_still_reads_its_own_rows() {
    let (_t, mut db) = indexed("snapshot", 0);
    let snap = db.snapshot();
    let before = db.scan_at("users", &snap).unwrap();
    let q = LogicalPlan::new(
        LogicalOp::scan("users")
            .filter(Expr::cmp("age", CmpOp::Gt, 40i64))
            .limit(10),
    );
    let first = db.query(&q).unwrap();
    for (id, _) in &first {
        db.delete("users", *id).unwrap();
    }
    // The live read moves past the deleted rows...
    let next = db.query(&q).unwrap();
    assert_eq!(next.len(), 10);
    assert!(next
        .iter()
        .all(|(id, _)| first.iter().all(|(f, _)| f != id)));
    // ...and the snapshot still holds every one of them.
    assert_eq!(db.scan_at("users", &snap).unwrap(), before);
    for (id, rec) in &first {
        assert_eq!(db.get_at("users", *id, &snap).unwrap().as_ref(), Some(rec));
    }
}
