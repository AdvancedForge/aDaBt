//! SQL, end to end: parsed to the IR, then executed by the real engine.
//!
//! `adabt-ir`'s own suite proves the parser produces the plan the builder
//! API would have. This proves those plans actually run, and — the point of
//! putting SQL last — that they run against joins, aggregates and indexes
//! that already existed rather than against a surface built to flatter the
//! parser.

use adabt_core::ids::RecordId;
use adabt_core::policy::Policy;
use adabt_core::record::Record;
use adabt_core::schema::Schema;
use adabt_core::store::LogicalStore;
use adabt_core::value::Value;
use adabt_engine::Database;
use adabt_index::IndexKind;
use adabt_ir::parse_select;
use std::path::{Path, PathBuf};

struct Tmp(PathBuf);
impl Tmp {
    fn new(tag: &str) -> Self {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "adabt-sql-{tag}-{}-{:?}",
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

const COUNTRIES: [&str; 4] = ["NO", "SE", "DK", "FI"];

fn seeded(dir: &Path) -> Database {
    let mut db = Database::open(dir, Policy::conventional()).unwrap();
    db.create_collection("users", Schema::dynamic()).unwrap();
    db.create_collection("orders", Schema::dynamic()).unwrap();
    for i in 0..40u64 {
        db.insert(
            "users",
            RecordId(i),
            Record::new()
                .with("id", i)
                .with("country", COUNTRIES[(i % 4) as usize])
                .with("age", (18 + i % 30) as i64),
        )
        .unwrap();
    }
    for i in 0..20u64 {
        db.insert(
            "orders",
            RecordId(i),
            Record::new()
                .with("id", i)
                .with("user_id", i)
                .with("total", (i * 10) as i64),
        )
        .unwrap();
    }
    db
}

fn run(db: &mut Database, sql: &str) -> Vec<(RecordId, Record)> {
    let plan = parse_select(sql).unwrap_or_else(|e| panic!("{sql}\n  -> {e}"));
    db.query(&plan)
        .unwrap_or_else(|e| panic!("{sql}\n  -> {e}"))
}

#[test]
fn a_select_star_returns_every_row() {
    let t = Tmp::new("star");
    let mut db = seeded(t.path());
    assert_eq!(run(&mut db, "SELECT * FROM users").len(), 40);
}

#[test]
fn a_where_clause_filters_exactly_as_the_builder_api_would() {
    let t = Tmp::new("where");
    let mut db = seeded(t.path());
    let via_sql = run(&mut db, "SELECT * FROM users WHERE country = 'NO'");

    use adabt_ir::plan::{LogicalOp, LogicalPlan};
    use adabt_ir::Expr;
    let via_builder = db
        .query(&LogicalPlan::new(
            LogicalOp::scan("users").filter(Expr::eq("country", "NO")),
        ))
        .unwrap();

    assert_eq!(via_sql.len(), 10);
    assert_eq!(via_sql, via_builder, "SQL and the builder API disagreed");
}

#[test]
fn and_or_and_comparisons_execute() {
    let t = Tmp::new("bool");
    let mut db = seeded(t.path());
    let rows = run(
        &mut db,
        "SELECT * FROM users WHERE country = 'NO' AND age >= 20",
    );
    assert!(!rows.is_empty());
    for (_, r) in &rows {
        assert_eq!(r.get("country"), Some(&Value::Str("NO".into())));
        match r.get("age") {
            Some(Value::I64(a)) => assert!(*a >= 20),
            other => panic!("unexpected age {other:?}"),
        }
    }
}

#[test]
fn in_like_and_is_null_execute() {
    let t = Tmp::new("preds");
    let mut db = seeded(t.path());
    assert_eq!(
        run(&mut db, "SELECT * FROM users WHERE country IN ('NO', 'SE')").len(),
        20
    );
    assert_eq!(
        run(&mut db, "SELECT * FROM users WHERE country LIKE 'N%'").len(),
        10
    );
    // No row has this field, so IS NULL matches every row and IS NOT NULL none.
    assert_eq!(
        run(&mut db, "SELECT * FROM users WHERE nothing IS NULL").len(),
        40
    );
    assert_eq!(
        run(&mut db, "SELECT * FROM users WHERE nothing IS NOT NULL").len(),
        0
    );
}

#[test]
fn order_by_and_limit_execute() {
    let t = Tmp::new("order");
    let mut db = seeded(t.path());
    let rows = run(&mut db, "SELECT * FROM users ORDER BY age DESC LIMIT 5");
    assert_eq!(rows.len(), 5);
    let ages: Vec<i64> = rows
        .iter()
        .filter_map(|(_, r)| match r.get("age") {
            Some(Value::I64(a)) => Some(*a),
            _ => None,
        })
        .collect();
    let mut sorted = ages.clone();
    sorted.sort_by(|a, b| b.cmp(a));
    assert_eq!(ages, sorted, "ORDER BY DESC did not sort descending");
}

#[test]
fn group_by_with_count_executes() {
    let t = Tmp::new("group");
    let mut db = seeded(t.path());
    let rows = run(&mut db, "SELECT COUNT(*) AS n FROM users GROUP BY country");
    assert_eq!(rows.len(), 4, "four countries");
    for (_, r) in &rows {
        assert_eq!(r.get("n"), Some(&Value::U64(10)));
    }
}

#[test]
fn a_join_written_in_sql_executes() {
    let t = Tmp::new("join");
    let mut db = seeded(t.path());
    let rows = run(
        &mut db,
        "SELECT * FROM users JOIN orders ON users.id = orders.user_id",
    );
    assert_eq!(rows.len(), 20, "20 orders each matching one user");
    for (_, r) in &rows {
        assert!(r.get("users.id").is_some());
        assert!(r.get("orders.total").is_some());
    }
}

#[test]
fn a_left_join_written_in_sql_keeps_unmatched_rows() {
    let t = Tmp::new("leftjoin");
    let mut db = seeded(t.path());
    let rows = run(
        &mut db,
        "SELECT * FROM users LEFT JOIN orders ON users.id = orders.user_id",
    );
    assert_eq!(rows.len(), 40, "every user appears, matched or not");
    let unmatched = rows
        .iter()
        .filter(|(_, r)| r.get("orders.id").is_none())
        .count();
    assert_eq!(unmatched, 20);
}

#[test]
fn a_qualified_column_in_a_where_clause_matches_a_joined_row() {
    // The reason qualified names are kept joined rather than translated:
    // `users.id` in SQL is the same string a joined row carries.
    let t = Tmp::new("qualified");
    let mut db = seeded(t.path());
    let rows = run(
        &mut db,
        "SELECT * FROM users JOIN orders ON users.id = orders.user_id WHERE users.id = 3",
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].1.get("users.id"), Some(&Value::U64(3)));
}

#[test]
fn sql_uses_an_index_exactly_as_the_builder_api_does() {
    // SQL is a front-end, not a second engine: the plan it produces must go
    // through the same planner and pick the same access path.
    let t = Tmp::new("index");
    let mut db = seeded(t.path());
    db.create_index("users", "country", IndexKind::Hash)
        .unwrap();
    let plan = parse_select("SELECT * FROM users WHERE country = 'NO'").unwrap();
    assert_eq!(db.plan(&plan).root.access_path().name(), "IndexLookup");
    assert_eq!(db.query(&plan).unwrap().len(), 10);
}

#[test]
fn an_unsupported_statement_is_refused_before_it_reaches_the_engine() {
    assert!(parse_select("DELETE FROM users").is_err());
    assert!(parse_select("SELECT * FROM a JOIN b ON a.x=b.x JOIN c ON a.y=c.y").is_err());
}
