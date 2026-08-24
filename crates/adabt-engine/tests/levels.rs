//! Level behaviour, and the invariant the whole project rests on:
//! **optimization must never change logical semantics.**
//!
//! Every level is run against the reference model with the same operation
//! sequence, and every query is run at every level and compared. If these pass,
//! the levels are real optimizations rather than merely different code paths.

use adabt_core::ids::RecordId;
use adabt_core::policy::{Consistency, Durability, Guarantees, Mode, Policy};
use adabt_core::record::Record;
use adabt_core::schema::{FieldDef, FieldType, Schema, SchemaMode};
use adabt_core::store::LogicalStore;
use adabt_engine::Database;
use adabt_ir::plan::{Agg, AggKind, LogicalOp, LogicalPlan, SortKey};
use adabt_ir::{CmpOp, Expr};
use adabt_testkit::differential::{run, seeds};
use adabt_testkit::generator::GenConfig;
use adabt_testkit::reference::ReferenceStore;
use std::path::{Path, PathBuf};

struct Tmp(PathBuf);
impl Tmp {
    fn new(tag: &str) -> Self {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "adabt-levels-{tag}-{}-{:?}",
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

/// Fixed-width on purpose, so `direct_lookup` is legal at level 10.
fn fixed_schema() -> Schema {
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
const LEVELS: [u8; 6] = [0, 1, 2, 3, 5, 10];

fn seed(db: &mut Database, n: u64) {
    db.create_collection("users", fixed_schema()).unwrap();
    for i in 0..n {
        db.insert(
            "users",
            RecordId(i),
            Record::new()
                .with("id", i)
                .with("country", COUNTRIES[(i % 4) as usize])
                .with("age", (i % 60) as i64)
                .with("balance", (i * 7 % 5000) as i64),
        )
        .unwrap();
    }
}

fn queries() -> Vec<LogicalPlan> {
    vec![
        LogicalPlan::new(LogicalOp::get("users", RecordId(42))),
        LogicalPlan::new(LogicalOp::get("users", RecordId(999_999))),
        LogicalPlan::new(LogicalOp::scan("users").filter(Expr::eq("country", "NO"))),
        LogicalPlan::new(LogicalOp::scan("users").filter(Expr::cmp("age", CmpOp::Gt, 30i64))),
        LogicalPlan::new(LogicalOp::scan("users").filter(Expr::And(vec![
            Expr::eq("country", "SE"),
            Expr::cmp("balance", CmpOp::Lt, 2000i64),
        ]))),
        LogicalPlan::new(LogicalOp::scan("users").filter(Expr::Or(vec![
            Expr::eq("country", "DK"),
            Expr::eq("age", 5i64),
        ]))),
        LogicalPlan::new(
            LogicalOp::scan("users")
                .filter(Expr::eq("country", "FI"))
                .sort(vec![SortKey {
                    field: "balance".into(),
                    descending: true,
                }])
                .limit(9),
        ),
        LogicalPlan::new(LogicalOp::scan("users").aggregate(
            vec!["country".into()],
            vec![Agg::count("n"), Agg::over(AggKind::Avg, "age", "mean")],
        )),
    ]
}

/// Run every query enough times that usage-driven optimizations have evidence.
fn warm(db: &mut Database) {
    for _ in 0..12 {
        for q in queries() {
            db.query(&q).unwrap();
        }
    }
}

#[test]
fn no_optimization_level_changes_any_answer() {
    let baseline = {
        let t = Tmp::new("baseline");
        let mut db = Database::open(t.path(), Policy::manual(0)).unwrap();
        seed(&mut db, 2_000);
        queries()
            .iter()
            .map(|q| db.query(q).unwrap())
            .collect::<Vec<_>>()
    };

    for level in LEVELS {
        let t = Tmp::new(&format!("lvl{level}"));
        let mut db = Database::open(t.path(), Policy::manual(level)).unwrap();
        seed(&mut db, 2_000);
        // Warm first, then re-optimize: this is where auto_index actually fires,
        // so the comparison is against a level that has really taken effect.
        warm(&mut db);
        db.optimize().unwrap();
        warm(&mut db);

        for (q, want) in queries().iter().zip(&baseline) {
            let got = db.query(q).unwrap();
            assert_eq!(
                &got,
                want,
                "level {level} changed the answer for:\n{}\nconfig: {}",
                db.explain(q),
                db.config().describe()
            );
        }
    }
}

#[test]
fn every_level_still_matches_the_reference_model() {
    let cfg = GenConfig::with_collections(vec![
        ("users".into(), fixed_schema()),
        ("events".into(), Schema::dynamic()),
    ]);
    for level in LEVELS {
        for (i, s) in seeds(0x1EAE1 + level as u64, 3).into_iter().enumerate() {
            let t = Tmp::new(&format!("ref{level}-{i}"));
            let mut a = ReferenceStore::new();
            let mut b = Database::open(t.path(), Policy::manual(level)).unwrap();
            for (name, schema) in &cfg.collections {
                a.create_collection(name, schema.clone()).unwrap();
                b.create_collection(name, schema.clone()).unwrap();
            }
            run(&mut a, &mut b, "reference", "engine", &cfg, s, 400)
                .unwrap_or_else(|d| panic!("level {level}: {d}"));
        }
    }
}

#[test]
fn a_higher_level_actually_enables_more() {
    let t = Tmp::new("more");
    let mut db = Database::open(t.path(), Policy::manual(0)).unwrap();
    seed(&mut db, 2_000);
    assert!(db.config().is_empty(), "level 0 enabled something");

    db.set_level(1).unwrap();
    assert!(db.config().is_enabled_anywhere("plan_cache"));
    assert!(db.config().is_enabled_anywhere("result_cache"));

    db.set_level(3).unwrap();
    assert!(db.config().is_enabled_anywhere("buffer_pool"));

    db.set_level(10).unwrap();
    assert!(db.config().is_enabled_anywhere("direct_lookup"));
    assert!(
        db.has_direct_array("users"),
        "direct_lookup was enabled but built no array"
    );
}

#[test]
fn lowering_the_level_takes_the_optimizations_back_out() {
    let t = Tmp::new("down");
    let mut db = Database::open(t.path(), Policy::manual(10)).unwrap();
    seed(&mut db, 2_000);
    db.set_level(10).unwrap();
    assert!(db.has_direct_array("users"));

    db.set_level(1).unwrap();
    assert!(!db.config().is_enabled_anywhere("direct_lookup"));
    assert!(
        !db.has_direct_array("users"),
        "the direct array survived being disabled"
    );
    assert!(db.config().is_enabled_anywhere("plan_cache"));
}

#[test]
fn the_result_cache_serves_repeated_identical_queries() {
    let t = Tmp::new("resultcache");
    let mut db = Database::open(t.path(), Policy::manual(1)).unwrap();
    seed(&mut db, 500);
    let q = LogicalPlan::new(LogicalOp::scan("users").filter(Expr::eq("country", "NO")));
    for _ in 0..20 {
        db.query(&q).unwrap();
    }
    assert!(
        db.result_cache_stats().hit_rate().unwrap() > 0.8,
        "result cache barely used: {:?}",
        db.result_cache_stats()
    );
}

#[test]
fn the_plan_cache_serves_one_shape_across_many_literals() {
    // The result cache answers repeated *identical* queries, so the plan cache
    // only earns its keep when the literals vary and the shape does not. That
    // is also exactly the case where caching plans rather than decisions would
    // return one query's rows for another.
    let t = Tmp::new("plancache");
    let mut db = Database::open(t.path(), Policy::manual(1)).unwrap();
    seed(&mut db, 800);

    for i in 0..200u64 {
        let q = LogicalPlan::new(LogicalOp::get("users", RecordId(i)));
        let rows = db.query(&q).unwrap();
        assert_eq!(rows.len(), 1, "id {i} not found");
        assert_eq!(
            rows[0].0,
            RecordId(i),
            "plan cache returned the wrong record"
        );
    }
    assert!(
        db.plan_cache_stats().hit_rate().unwrap() > 0.9,
        "plan cache barely used: {:?}",
        db.plan_cache_stats()
    );

    // Same for a shape whose literal drives an index lookup.
    db.create_index("users", "country", adabt_core::index_kind::IndexKind::Hash)
        .unwrap();
    for c in [COUNTRIES[0], COUNTRIES[1], COUNTRIES[2], COUNTRIES[3]] {
        let q = LogicalPlan::new(LogicalOp::scan("users").filter(Expr::eq("country", c)));
        for (_, r) in db.query(&q).unwrap() {
            assert_eq!(
                r.get("country"),
                Some(&adabt_core::value::Value::Str(c.to_string())),
                "an index lookup used a stale key"
            );
        }
    }
}

#[test]
fn a_write_invalidates_a_cached_result_rather_than_serving_it_stale() {
    let t = Tmp::new("stale");
    let mut db = Database::open(t.path(), Policy::manual(1)).unwrap();
    seed(&mut db, 200);
    let q = LogicalPlan::new(LogicalOp::scan("users").filter(Expr::eq("country", "NO")));
    let before = db.query(&q).unwrap().len();

    // Move one NO user elsewhere.
    let id = db.query(&q).unwrap()[0].0;
    let mut rec = db.get("users", id).unwrap().unwrap();
    rec.set("country", "ZZ");
    db.update("users", id, rec).unwrap();

    assert_eq!(
        db.query(&q).unwrap().len(),
        before - 1,
        "a stale cached result was served after a write"
    );
}

#[test]
fn auto_index_fires_once_there_is_evidence_and_is_explained() {
    let t = Tmp::new("autoindex");
    let mut db = Database::open(t.path(), Policy::manual(2)).unwrap();
    seed(&mut db, 3_000);
    assert!(db.index_specs().is_empty(), "indexed before any evidence");

    warm(&mut db);
    db.optimize().unwrap();

    let specs = db.index_specs();
    assert!(
        !specs.is_empty(),
        "auto_index never fired: {}",
        db.explain_optimizations()
    );
    assert!(specs.iter().any(|s| s.field == "country"));

    let e = db.explain_optimization("auto_index");
    assert!(e.contains("applied"), "{e}");
    // The kind is chosen from how the field is actually filtered, not fixed:
    // a field seen only under equality gets a hash index, a field seen under
    // ranges or inside an `Or` (which contributes no equality constraint) gets
    // an ordered one, because that serves both.
    assert!(
        e.contains("index on users.country"),
        "no index was created for the field the workload filters on:\n{e}"
    );
}

#[test]
fn direct_lookup_is_refused_for_a_variable_width_schema() {
    let t = Tmp::new("nodirect");
    let mut db = Database::open(t.path(), Policy::manual(0)).unwrap();
    db.create_collection(
        "docs",
        Schema::new(
            SchemaMode::Strict,
            vec![
                FieldDef::new("id", FieldType::U64).required(),
                FieldDef::new("body", FieldType::Str { max_len: None }),
            ],
        )
        .unwrap(),
    )
    .unwrap();
    db.set_level(10).unwrap();

    assert!(!db.has_direct_array("docs"));
    let e = db.explain_optimization("direct_lookup");
    assert!(e.contains("not applicable"), "{e}");
    assert!(e.contains("constant stride"), "{e}");
}

#[test]
fn strict_durability_still_holds_at_the_highest_level() {
    // Level 10 is aggressive, but it may not weaken a guarantee the policy set.
    let t = Tmp::new("durability");
    let policy = Policy {
        mode: Mode::Manual {
            level: 10,
            overrides: vec![],
        },
        guarantees: Guarantees {
            durability: Durability::Strict,
            consistency: Consistency::Strict,
        },
        ..Policy::conventional()
    };
    let mut db = Database::open(t.path(), policy).unwrap();
    seed(&mut db, 100);
    assert_eq!(db.durability(), Durability::Strict);
}

#[test]
fn the_decision_log_explains_the_whole_configuration() {
    let t = Tmp::new("explain");
    let mut db = Database::open(t.path(), Policy::manual(3)).unwrap();
    seed(&mut db, 100);
    let text = db.explain_optimizations();
    assert!(text.contains("requested by: manual"), "{text}");
    assert!(text.contains("level 3 preset"), "{text}");
    assert!(text.contains("plan_cache"), "{text}");
}

#[test]
fn direct_lookup_survives_writes_and_stays_consistent_with_the_heap() {
    let t = Tmp::new("directwrites");
    let mut db = Database::open(t.path(), Policy::manual(10)).unwrap();
    seed(&mut db, 1_000);
    db.set_level(10).unwrap();
    assert!(db.has_direct_array("users"));

    for i in 0..1_000u64 {
        if i % 3 == 0 {
            db.delete("users", RecordId(i)).unwrap();
        } else if i % 3 == 1 {
            db.update(
                "users",
                RecordId(i),
                Record::new()
                    .with("id", i)
                    .with("country", "ZZ")
                    .with("age", 99i64)
                    .with("balance", 1i64),
            )
            .unwrap();
        }
    }
    // Turning it off must not change a single answer.
    let with: Vec<_> = (0..1_000u64)
        .map(|i| db.get("users", RecordId(i)).unwrap())
        .collect();
    db.set_level(1).unwrap();
    assert!(!db.has_direct_array("users"));
    let without: Vec<_> = (0..1_000u64)
        .map(|i| db.get("users", RecordId(i)).unwrap())
        .collect();
    assert_eq!(with, without, "the direct array disagreed with the heap");
}

/// The resource axis, demonstrated rather than asserted.
///
/// Until record compression existed, every optimization spent resources to buy
/// latency, so a `resources`-priority policy had nothing to select and this
/// direction of the premise could not be tested at all.
#[test]
fn a_higher_level_can_reduce_storage_rather_than_only_spending_it() {
    let wide = Schema::new(
        SchemaMode::Fixed,
        vec![
            FieldDef::new("id", FieldType::U64).required(),
            FieldDef::new("balance", FieldType::I64).required(),
            FieldDef::new("name", FieldType::Char(64)),
            FieldDef::new("notes", FieldType::Char(128)),
        ],
    )
    .unwrap();
    let rec = |i: u64| {
        Record::new()
            .with("id", i)
            .with("balance", (i * 37 % 100_000) as i64)
            .with("name", format!("customer-{i}"))
            .with("notes", format!("acct {i}"))
    };

    let mut measured = Vec::new();
    for level in [1u8, 2] {
        let t = Tmp::new(&format!("storage-l{level}"));
        let mut db = Database::open(t.path(), Policy::manual(level)).unwrap();
        db.create_collection("wide", wide.clone()).unwrap();
        for i in 0..3_000u64 {
            db.insert("wide", RecordId(i), rec(i)).unwrap();
        }
        db.optimize().unwrap();
        measured.push((level, db.stored_bytes().unwrap(), db.compression_enabled()));
    }

    let (_, level1_bytes, l1_compressed) = measured[0];
    let (_, level2_bytes, l2_compressed) = measured[1];
    assert!(!l1_compressed, "level 1 should not compress");
    assert!(l2_compressed, "level 2 should compress");
    assert!(
        level2_bytes * 2 < level1_bytes,
        "level 2 stored {level2_bytes} bytes against level 1's {level1_bytes}"
    );
}

#[test]
fn compression_does_not_change_a_single_answer() {
    let wide = Schema::new(
        SchemaMode::Fixed,
        vec![
            FieldDef::new("id", FieldType::U64).required(),
            FieldDef::new("country", FieldType::Char(8)),
            FieldDef::new("notes", FieldType::Char(96)),
        ],
    )
    .unwrap();
    let rec = |i: u64| {
        Record::new()
            .with("id", i)
            .with("country", COUNTRIES[(i % 4) as usize])
            .with("notes", format!("note for {i}"))
    };

    let mut results = Vec::new();
    for level in [1u8, 2] {
        let t = Tmp::new(&format!("compress-answers-l{level}"));
        let mut db = Database::open(t.path(), Policy::manual(level)).unwrap();
        db.create_collection("wide", wide.clone()).unwrap();
        for i in 0..1_000u64 {
            db.insert("wide", RecordId(i), rec(i)).unwrap();
        }
        db.optimize().unwrap();
        let q = LogicalPlan::new(LogicalOp::scan("wide").filter(Expr::eq("country", "NO")));
        let mut rows = db.query(&q).unwrap();
        rows.extend((0..50u64).filter_map(|i| {
            db.get("wide", RecordId(i))
                .unwrap()
                .map(|r| (RecordId(i), r))
        }));
        results.push(rows);
    }
    assert_eq!(results[0], results[1], "compression changed the answers");
}

#[test]
fn a_column_store_answers_aggregates_without_changing_them() {
    let t0 = Tmp::new("agg-row");
    let t4 = Tmp::new("agg-col");
    let q = LogicalPlan::new(LogicalOp::scan("users").aggregate(
        vec!["country".into()],
        vec![Agg::count("n"), Agg::over(AggKind::Avg, "balance", "mean")],
    ));

    let mut row = Database::open(t0.path(), Policy::manual(1)).unwrap();
    seed(&mut row, 4_000);
    row.optimize().unwrap();
    let want = row.query(&q).unwrap();
    assert!(
        row.plan(&q).explain().contains("HeapScan"),
        "{}",
        row.plan(&q).explain()
    );

    let mut col = Database::open(t4.path(), Policy::manual(4)).unwrap();
    seed(&mut col, 4_000);
    col.optimize().unwrap();
    assert!(
        col.has_column_store("users"),
        "level 4 built no column store"
    );
    let got = col.query(&q).unwrap();

    assert!(
        col.plan(&q).explain().contains("ColumnScan"),
        "the column store was built but not used:\n{}",
        col.plan(&q).explain()
    );
    assert_eq!(got, want, "the column store changed the aggregate");
}

#[test]
fn a_column_store_is_not_used_where_it_cannot_reconstruct_the_answer() {
    // A plan returning whole records cannot be served columnar: the columnar
    // read only reconstructs the fields it is asked for.
    let t = Tmp::new("col-wholerow");
    let mut db = Database::open(t.path(), Policy::manual(4)).unwrap();
    seed(&mut db, 4_000);
    db.optimize().unwrap();
    assert!(db.has_column_store("users"));

    let whole = LogicalPlan::new(LogicalOp::scan("users").filter(Expr::eq("country", "NO")));
    let plan = db.plan(&whole);
    assert!(
        !plan.explain().contains("ColumnScan"),
        "a whole-record query was served columnar:\n{}",
        plan.explain()
    );
    // And the answer is still complete.
    for (_, r) in db.query(&whole).unwrap() {
        assert_eq!(r.len(), 4, "columnar leakage dropped fields");
    }
}

#[test]
fn column_store_maintenance_keeps_answers_correct_under_writes() {
    let t = Tmp::new("col-writes");
    let mut db = Database::open(t.path(), Policy::manual(4)).unwrap();
    seed(&mut db, 4_000);
    db.optimize().unwrap();
    assert!(db.has_column_store("users"));

    for i in 0..4_000u64 {
        if i % 5 == 0 {
            db.delete("users", RecordId(i)).unwrap();
        } else if i % 5 == 1 {
            db.update(
                "users",
                RecordId(i),
                Record::new()
                    .with("id", i)
                    .with("country", "ZZ")
                    .with("age", 1i64)
                    .with("balance", 2i64),
            )
            .unwrap();
        }
    }

    let q = LogicalPlan::new(
        LogicalOp::scan("users").aggregate(vec!["country".into()], vec![Agg::count("n")]),
    );
    let with_columns = db.query(&q).unwrap();

    // Turning the column store off must not change a single row.
    db.set_level(1).unwrap();
    assert!(!db.has_column_store("users"));
    assert_eq!(
        db.query(&q).unwrap(),
        with_columns,
        "the column store disagreed with the heap after writes"
    );
}
