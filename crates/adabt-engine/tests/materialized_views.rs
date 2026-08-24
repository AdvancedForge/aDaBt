//! Materialized views against the scan that would otherwise have answered.
//!
//! One assertion carries this file: **a maintained total and a recomputed one
//! are the same number.** Everything else is arrangement.
//!
//! It is worth saying why that is not obvious. A view is not a cache — it is
//! never invalidated and never recomputed, so an error in maintenance does not
//! wash out on the next read. It accumulates. A view that loses one row on a
//! particular kind of update is wrong by one from then until the database is
//! restarted, and the number it returns stays plausible the whole time. So the
//! tests here mutate hard and compare against a scan after every step, rather
//! than checking a total once and trusting it.

use adabt_core::ids::RecordId;
use adabt_core::policy::Policy;
use adabt_core::record::Record;
use adabt_core::schema::Schema;
use adabt_core::store::LogicalStore;
use adabt_core::value::Value;
use adabt_engine::Database;
use adabt_ir::plan::{Agg, AggKind, LogicalOp, LogicalPlan};
use adabt_ir::Expr;
use adabt_testkit::rng::Rng;
use std::path::{Path, PathBuf};

struct Tmp(PathBuf);
impl Tmp {
    fn new(tag: &str) -> Self {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "adabt-mv-{tag}-{}-{:?}",
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

const COUNTRIES: [&str; 5] = ["NO", "SE", "DK", "FI", "IS"];
const N: u64 = 3_000;

fn rec(i: u64) -> Record {
    let mut r = Record::new()
        .with("id", i)
        .with("country", COUNTRIES[(i % 5) as usize])
        .with("tier", ((i % 3) as i64).to_string());
    // Every third record has no email, so COUNT(email) differs from COUNT(*).
    if i % 3 != 0 {
        r = r.with("email", format!("u{i}@example.com"));
    }
    r
}

fn count_by_country() -> LogicalPlan {
    LogicalPlan::new(
        LogicalOp::scan("users").aggregate(vec!["country".into()], vec![Agg::count("n")]),
    )
}

fn count_by_country_and_tier() -> LogicalPlan {
    LogicalPlan::new(LogicalOp::scan("users").aggregate(
        vec!["country".into(), "tier".into()],
        vec![
            Agg::count("rows"),
            Agg::over(AggKind::Count, "email", "emails"),
        ],
    ))
}

fn total() -> LogicalPlan {
    LogicalPlan::new(LogicalOp::scan("users").aggregate(vec![], vec![Agg::count("n")]))
}

/// Fill a database, then raise it to `level`.
///
/// The level is applied *after* the rows exist, which is not a convenience.
/// `materialized_view` is not applicable to an empty database — maintaining a
/// total over no rows saves nothing — so opening an empty directory at level 4
/// leaves the optimization switched off, and a test written that way would
/// compare a scan against a scan and pass without ever building a view.
fn seeded(dir: &Path, level: u8) -> Database {
    let mut db = Database::open(dir, Policy::manual(0)).unwrap();
    db.create_collection("users", Schema::dynamic()).unwrap();
    for i in 0..N {
        db.insert("users", RecordId(i), rec(i)).unwrap();
    }
    db.set_level(level).unwrap();
    db
}

/// Answer the same query with views on and with views off.
///
/// The second database is opened at level 0 over its own directory: what is
/// being compared is the maintained total against a recomputed one, so the
/// recomputing side has to genuinely recompute.
struct Both {
    with: Database,
    without: Database,
    _a: Tmp,
    _b: Tmp,
}

impl Both {
    fn new(tag: &str) -> Self {
        let a = Tmp::new(&format!("{tag}-view"));
        let b = Tmp::new(&format!("{tag}-scan"));
        Both {
            with: seeded(a.path(), 4),
            without: seeded(b.path(), 0),
            _a: a,
            _b: b,
        }
    }

    fn agree_on(&mut self, q: &LogicalPlan) {
        let viewed = self.with.query(q).unwrap();
        let scanned = self.without.query(q).unwrap();
        assert_eq!(
            viewed,
            scanned,
            "the view and the scan disagree:\n{}",
            q.explain()
        );
    }

    fn insert(&mut self, i: u64, r: Record) {
        self.with.insert("users", RecordId(i), r.clone()).unwrap();
        self.without.insert("users", RecordId(i), r).unwrap();
    }
    fn update(&mut self, i: u64, r: Record) {
        self.with.update("users", RecordId(i), r.clone()).unwrap();
        self.without.update("users", RecordId(i), r).unwrap();
    }
    fn delete(&mut self, i: u64) {
        self.with.delete("users", RecordId(i)).unwrap();
        self.without.delete("users", RecordId(i)).unwrap();
    }
}

#[test]
fn a_view_and_a_scan_return_the_same_rows() {
    let mut b = Both::new("same");
    for q in [count_by_country(), count_by_country_and_tier(), total()] {
        b.agree_on(&q);
        // Twice: the first call builds the view, the second reads it.
        b.agree_on(&q);
    }
    assert_eq!(
        b.with.materialized_views(),
        3,
        "{}",
        b.with.explain_materialized_views()
    );
    assert_eq!(b.without.materialized_views(), 0);
}

#[test]
fn a_view_stays_correct_through_a_long_mutating_workload() {
    // The test that would catch an error that accumulates. A view is never
    // recomputed, so one lost row stays lost.
    let mut b = Both::new("mutating");
    b.agree_on(&count_by_country());
    b.agree_on(&count_by_country_and_tier());
    assert_eq!(
        b.with.materialized_views(),
        2,
        "no views were built, so this test would pass against a scan"
    );

    let mut rng = Rng::new(0xA5A5_1234);
    for step in 0..1_500u64 {
        match rng.below(4) {
            0 => b.insert(N + step, rec(N + step)),
            // Moves the record between groups, which is the case a naive
            // implementation gets wrong: decrement one group, increment another.
            1 => {
                let i = rng.below(N);
                b.update(i, rec(i + 1).with("id", i));
            }
            2 => b.delete(rng.below(N)),
            // Removes the counted field without changing the group, so
            // COUNT(*) must hold still while COUNT(email) drops.
            _ => {
                let i = rng.below(N);
                let mut r = rec(i);
                r.remove("email");
                b.update(i, r);
            }
        }
        if step % 100 == 0 {
            b.agree_on(&count_by_country());
            b.agree_on(&count_by_country_and_tier());
            b.agree_on(&total());
        }
    }
    b.agree_on(&count_by_country());
    b.agree_on(&count_by_country_and_tier());
    b.agree_on(&total());
}

#[test]
fn emptying_a_group_makes_it_disappear_from_both() {
    // A group nothing is in produces no row from a scan, so the view must not
    // produce one either — a zero where the scan says nothing is a divergence
    // even though the number is arguably right.
    let mut b = Both::new("empty");
    b.agree_on(&count_by_country());
    for i in 0..N {
        if i % 5 == 4 {
            b.delete(i); // every "IS"
        }
    }
    b.agree_on(&count_by_country());
    let rows = b.with.query(&count_by_country()).unwrap();
    assert_eq!(rows.len(), 4, "the emptied group is still being reported");
    assert!(!rows
        .iter()
        .any(|(_, r)| r.get("country") == Some(&Value::from("IS"))));
}

#[test]
fn a_filtered_aggregate_is_never_answered_from_a_view() {
    // The view holds totals, not rows. Answering a filtered question from one
    // would return the unfiltered answer, which is wrong in the worst way: it
    // looks like a perfectly ordinary result.
    let mut b = Both::new("filtered");
    b.agree_on(&count_by_country());

    let filtered = LogicalPlan::new(
        LogicalOp::scan("users")
            .filter(Expr::eq("country", "NO"))
            .aggregate(vec!["country".into()], vec![Agg::count("n")]),
    );
    b.agree_on(&filtered);
    let rows = b.with.query(&filtered).unwrap();
    assert_eq!(rows.len(), 1, "a filtered aggregate returned every group");
    assert_eq!(rows[0].1.get("n"), Some(&Value::U64(N / 5)));
}

#[test]
fn a_maintained_sum_matches_a_scanned_one_bit_for_bit() {
    // The claim the exactness budget makes. Every value here is an integer and
    // the totals stay far below 2^53, so incremental arithmetic and scanned
    // arithmetic must agree exactly — not nearly, exactly, because the results
    // are compared as `Value::F64` and equality on those is bitwise.
    let mut b = Both::new("sums");
    let q = LogicalPlan::new(LogicalOp::scan("users").aggregate(
        vec!["country".into()],
        vec![
            Agg::over(AggKind::Sum, "id", "total"),
            Agg::over(AggKind::Avg, "id", "mean"),
            Agg::count("n"),
        ],
    ));
    b.agree_on(&q);
    b.agree_on(&q);
    assert!(
        b.with.materialized_views() > 0,
        "the sum was not materialized, so this proves nothing"
    );

    // Deletions and moves between groups, which is where an incremental sum
    // that is merely *close* starts to drift.
    let mut rng = Rng::new(0x5115);
    for step in 0..800u64 {
        match rng.below(3) {
            0 => b.insert(N + step, rec(N + step)),
            1 => b.delete(rng.below(N)),
            _ => {
                let i = rng.below(N);
                b.update(i, rec(i + 1).with("id", i));
            }
        }
        if step % 100 == 0 {
            b.agree_on(&q);
        }
    }
    b.agree_on(&q);
}

#[test]
fn a_fractional_column_falls_back_to_the_scan_rather_than_drifting() {
    // Non-integer values make incremental and scanned arithmetic disagree in
    // the low bits, so the view stops answering and the scan takes over. The
    // answers must stay identical throughout — the fallback is invisible except
    // in how long it takes.
    let t = Tmp::new("fractional");
    let mut db = Database::open(t.path(), Policy::manual(0)).unwrap();
    db.create_collection("users", Schema::dynamic()).unwrap();
    for i in 0..N {
        db.insert(
            "users",
            RecordId(i),
            Record::new()
                .with("country", COUNTRIES[(i % 5) as usize])
                .with("amount", 0.1f64 * (i % 97) as f64),
        )
        .unwrap();
    }
    db.set_level(4).unwrap();

    let q = LogicalPlan::new(LogicalOp::scan("users").aggregate(
        vec!["country".into()],
        vec![Agg::over(AggKind::Sum, "amount", "total")],
    ));
    let first = db.query(&q).unwrap();

    let mut plain = Database::open(&t.path().join("plain"), Policy::manual(0)).unwrap();
    plain.create_collection("users", Schema::dynamic()).unwrap();
    for i in 0..N {
        plain
            .insert(
                "users",
                RecordId(i),
                Record::new()
                    .with("country", COUNTRIES[(i % 5) as usize])
                    .with("amount", 0.1f64 * (i % 97) as f64),
            )
            .unwrap();
    }
    assert_eq!(
        db.query(&q).unwrap(),
        plain.query(&q).unwrap(),
        "a fractional sum diverged from the scan"
    );
    assert_eq!(db.query(&q).unwrap(), first);
}

#[test]
fn a_delete_reaches_the_column_store_even_with_no_index_to_maintain() {
    // A regression. Removing a row used to be told to the derived structures
    // only when the *old record* had been read, and the old record was only read
    // when an index needed it — so a collection with a column store and no index
    // went on aggregating rows that had been deleted. The answer stayed
    // plausible, which is what made it worth a test of its own.
    let t = Tmp::new("tombstone");
    let mut db = seeded(t.path(), 4);
    assert!(db.has_column_store("users"), "no column store to regress");
    assert_eq!(db.index_specs().len(), 0, "an index would mask the bug");

    let q = LogicalPlan::new(LogicalOp::scan("users").aggregate(
        vec!["country".into()],
        vec![Agg::over(AggKind::Sum, "id", "total")],
    ));
    db.query(&q).unwrap();
    for i in 0..N {
        if i % 5 == 0 {
            db.delete("users", RecordId(i)).unwrap();
        }
    }
    let rows = db.query(&q).unwrap();
    assert!(
        !rows
            .iter()
            .any(|(_, r)| r.get("country") == Some(&Value::from("NO"))),
        "a wholly deleted group is still being aggregated: {rows:?}"
    );
    assert_eq!(db.count("users").unwrap(), (N - N / 5) as usize);
}

#[test]
fn turning_views_off_returns_the_database_to_the_scan_and_the_same_answers() {
    let t = Tmp::new("off");
    let mut db = seeded(t.path(), 4);
    let before = db.query(&count_by_country()).unwrap();
    assert_eq!(db.materialized_views(), 1);

    db.set_level(0).unwrap();
    assert_eq!(
        db.materialized_views(),
        0,
        "views survived being switched off"
    );
    assert_eq!(db.query(&count_by_country()).unwrap(), before);

    // And back on again, rebuilt from the primary.
    db.set_level(4).unwrap();
    assert_eq!(db.query(&count_by_country()).unwrap(), before);
    assert_eq!(db.materialized_views(), 1);
}

#[test]
fn a_view_is_rebuilt_from_the_primary_after_a_restart() {
    // Views are derived and are not persisted. What must survive is the answer.
    let t = Tmp::new("restart");
    let expected = {
        let mut db = seeded(t.path(), 4);
        let rows = db.query(&count_by_country()).unwrap();
        db.checkpoint().unwrap();
        rows
    };
    let mut db = Database::open(t.path(), Policy::manual(4)).unwrap();
    assert_eq!(
        db.materialized_views(),
        0,
        "a view came back without a query"
    );
    assert_eq!(db.query(&count_by_country()).unwrap(), expected);
    assert_eq!(db.materialized_views(), 1);
}

#[test]
fn a_view_answers_in_time_that_does_not_grow_with_the_table() {
    // The point of the whole thing. Five groups instead of three thousand rows.
    let q = count_by_country();

    // A write before every measurement, so the result cache — which level 4 also
    // switches on — cannot answer instead. Without this the "view" side would be
    // timing a cache hit and the comparison would prove nothing about views.
    let measure = |db: &mut Database, mark: u64| {
        let mut best = u128::MAX;
        for k in 0..20u64 {
            db.insert("users", RecordId(mark + k), rec(mark + k))
                .unwrap();
            let s = std::time::Instant::now();
            db.query(&q).unwrap();
            best = best.min(s.elapsed().as_nanos());
        }
        best
    };

    let t = Tmp::new("speed");
    let mut db = seeded(t.path(), 4);
    db.query(&q).unwrap(); // builds the view
    assert_eq!(db.materialized_views(), 1, "no view to measure");
    let viewed = measure(&mut db, 100_000);

    let t2 = Tmp::new("speed-scan");
    let mut plain = seeded(t2.path(), 0);
    let scanned = measure(&mut plain, 100_000);

    assert!(
        viewed * 10 < scanned,
        "the view answered in {viewed}ns against the scan's {scanned}ns, \
         which is not the order-of-magnitude difference the design claims"
    );
}
