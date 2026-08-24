//! Engine-level tests, including the property that matters most: creating or
//! dropping an index must never change an answer.

use adabt_core::ids::RecordId;
use adabt_core::policy::Policy;
use adabt_core::record::Record;
use adabt_core::schema::{FieldDef, FieldType, Schema, SchemaMode};
use adabt_core::store::LogicalStore;
use adabt_core::value::Value;
use adabt_engine::Database;
use adabt_index::IndexKind;
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
            "adabt-engine-{tag}-{}-{:?}",
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
        SchemaMode::Strict,
        vec![
            FieldDef::new("id", FieldType::U64).required(),
            FieldDef::new("country", FieldType::Str { max_len: Some(8) }),
            FieldDef::new("age", FieldType::I64),
            FieldDef::new("name", FieldType::Str { max_len: Some(32) }),
        ],
    )
    .unwrap()
}

const COUNTRIES: [&str; 4] = ["NO", "SE", "DK", "FI"];

fn seeded(dir: &Path, n: u64) -> Database {
    let mut db = Database::open(dir, Policy::conventional()).unwrap();
    db.create_collection("users", schema()).unwrap();
    for i in 0..n {
        db.insert(
            "users",
            RecordId(i),
            Record::new()
                .with("id", i)
                .with("country", COUNTRIES[(i % 4) as usize])
                .with("age", (i % 60) as i64)
                .with("name", format!("user{i}")),
        )
        .unwrap();
    }
    db
}

/// The central property of the whole project, at engine scale: adding a
/// physical structure must not change a single answer.
#[test]
fn indexes_never_change_query_results() {
    let t = Tmp::new("invariance");
    let mut db = seeded(t.path(), 800);

    let queries: Vec<LogicalPlan> = vec![
        LogicalPlan::new(LogicalOp::scan("users").filter(Expr::eq("country", "NO"))),
        LogicalPlan::new(LogicalOp::scan("users").filter(Expr::cmp("age", CmpOp::Gt, 30i64))),
        LogicalPlan::new(LogicalOp::scan("users").filter(Expr::And(vec![
            Expr::eq("country", "SE"),
            Expr::cmp("age", CmpOp::Lt, 20i64),
        ]))),
        LogicalPlan::new(LogicalOp::scan("users").filter(Expr::Or(vec![
            Expr::eq("country", "DK"),
            Expr::eq("age", 5i64),
        ]))),
        LogicalPlan::new(
            LogicalOp::scan("users")
                .filter(Expr::eq("country", "FI"))
                .sort(vec![SortKey {
                    field: "age".into(),
                    descending: true,
                }])
                .limit(7),
        ),
        LogicalPlan::new(LogicalOp::scan("users").aggregate(
            vec!["country".into()],
            vec![Agg::count("n"), Agg::over(AggKind::Avg, "age", "mean")],
        )),
    ];

    let baseline: Vec<_> = queries.iter().map(|q| db.query(q).unwrap()).collect();

    let configs: Vec<Vec<(&str, IndexKind)>> = vec![
        vec![("country", IndexKind::Hash)],
        vec![("country", IndexKind::BTree)],
        vec![("country", IndexKind::Bitmap)],
        vec![("age", IndexKind::BTree)],
        vec![("age", IndexKind::Hash)],
        vec![
            ("country", IndexKind::Hash),
            ("age", IndexKind::BTree),
            ("name", IndexKind::Hash),
        ],
        vec![("country", IndexKind::Bitmap), ("age", IndexKind::Bitmap)],
    ];

    for cfg in configs {
        for (field, kind) in &cfg {
            db.create_index("users", field, *kind).unwrap();
        }
        for (q, want) in queries.iter().zip(&baseline) {
            let got = db.query(q).unwrap();
            assert_eq!(
                &got,
                want,
                "index config {cfg:?} changed the answer for:\n{}",
                db.explain(q)
            );
        }
        for (field, kind) in &cfg {
            db.drop_index("users", field, *kind);
        }
        for (q, want) in queries.iter().zip(&baseline) {
            assert_eq!(
                &db.query(q).unwrap(),
                want,
                "dropping an index changed the answer"
            );
        }
    }
}

#[test]
fn an_index_actually_changes_the_plan_and_reduces_work() {
    let t = Tmp::new("effect");
    let mut db = seeded(t.path(), 2000);
    let q = LogicalPlan::new(LogicalOp::scan("users").filter(Expr::eq("country", "NO")));

    db.query(&q).unwrap();
    let scanned_without = db.last_exec_stats().rows_scanned;
    assert!(db.plan(&q).is_full_scan());

    db.create_index("users", "country", IndexKind::Hash)
        .unwrap();
    db.query(&q).unwrap();
    let scanned_with = db.last_exec_stats().rows_scanned;

    assert!(!db.plan(&q).is_full_scan(), "{}", db.explain(&q));
    assert!(
        scanned_with * 3 < scanned_without,
        "index did not reduce work: {scanned_with} vs {scanned_without}"
    );
}

#[test]
fn indexes_stay_correct_under_updates_and_deletes() {
    let t = Tmp::new("maintenance");
    let mut db = seeded(t.path(), 400);
    db.create_index("users", "country", IndexKind::Hash)
        .unwrap();
    let q = LogicalPlan::new(LogicalOp::scan("users").filter(Expr::eq("country", "NO")));

    let movers: Vec<RecordId> = db.query(&q).unwrap().iter().map(|(i, _)| *i).collect();
    for id in &movers {
        let mut rec = db.get("users", *id).unwrap().unwrap();
        rec.set("country", "SE");
        db.update("users", *id, rec).unwrap();
    }
    assert!(
        db.query(&q).unwrap().is_empty(),
        "stale index entries survived an update"
    );

    let se = LogicalPlan::new(LogicalOp::scan("users").filter(Expr::eq("country", "SE")));
    let before = db.query(&se).unwrap().len();
    for id in movers.iter().take(10) {
        db.delete("users", *id).unwrap();
    }
    assert_eq!(db.query(&se).unwrap().len(), before - 10);
}

#[test]
fn index_maintenance_matches_a_rebuild_from_scratch() {
    let t = Tmp::new("rebuild");
    let mut db = seeded(t.path(), 300);
    db.create_index("users", "age", IndexKind::BTree).unwrap();
    for i in 0..300u64 {
        if i % 3 == 0 {
            db.delete("users", RecordId(i)).unwrap();
        } else if i % 3 == 1 {
            let mut r = db.get("users", RecordId(i)).unwrap().unwrap();
            r.set("age", ((i * 7) % 60) as i64);
            db.update("users", RecordId(i), r).unwrap();
        }
    }
    let q = LogicalPlan::new(LogicalOp::scan("users").filter(Expr::cmp("age", CmpOp::Lt, 25i64)));
    let incremental = db.query(&q).unwrap();

    db.drop_index("users", "age", IndexKind::BTree);
    db.create_index("users", "age", IndexKind::BTree).unwrap();
    assert_eq!(db.query(&q).unwrap(), incremental);
}

#[test]
fn the_engine_matches_the_reference_model() {
    let cfg = GenConfig::with_collections(vec![
        ("users".into(), schema()),
        ("events".into(), Schema::dynamic()),
    ]);
    for (i, seed) in seeds(0xE0619, 8).into_iter().enumerate() {
        let t = Tmp::new(&format!("diff{i}"));
        let mut a = ReferenceStore::new();
        let mut b = Database::open(t.path(), Policy::conventional()).unwrap();
        for (name, s) in &cfg.collections {
            a.create_collection(name, s.clone()).unwrap();
            b.create_collection(name, s.clone()).unwrap();
        }
        b.create_index("users", "country", IndexKind::Hash).unwrap();
        b.create_index("users", "age", IndexKind::BTree).unwrap();
        run(&mut a, &mut b, "reference", "engine", &cfg, seed, 600)
            .unwrap_or_else(|d| panic!("{d}"));
    }
}

#[test]
fn indexes_are_dropped_with_their_collection() {
    let t = Tmp::new("dropcoll");
    let mut db = seeded(t.path(), 50);
    db.create_index("users", "country", IndexKind::Hash)
        .unwrap();
    assert_eq!(db.index_specs().len(), 1);
    db.drop_collection("users").unwrap();
    assert!(
        db.index_specs().is_empty(),
        "an index outlived its collection"
    );
}

#[test]
fn index_memory_is_reported_and_released() {
    let t = Tmp::new("memory");
    let mut db = seeded(t.path(), 1000);
    assert_eq!(db.index_memory_bytes(), 0);
    db.create_index("users", "name", IndexKind::BTree).unwrap();
    let with = db.index_memory_bytes();
    assert!(with > 10_000, "index memory implausibly small: {with}");
    db.drop_index("users", "name", IndexKind::BTree);
    assert_eq!(
        db.index_memory_bytes(),
        0,
        "dropping an index freed nothing"
    );
}

#[test]
fn creating_the_same_index_twice_is_idempotent() {
    let t = Tmp::new("dup");
    let mut db = seeded(t.path(), 20);
    db.create_index("users", "country", IndexKind::Hash)
        .unwrap();
    db.create_index("users", "country", IndexKind::Hash)
        .unwrap();
    assert_eq!(db.index_specs().len(), 1);
    db.create_index("users", "country", IndexKind::BTree)
        .unwrap();
    assert_eq!(db.index_specs().len(), 2);
}

#[test]
fn indexing_a_missing_collection_fails() {
    let t = Tmp::new("nocoll");
    let mut db = Database::open(t.path(), Policy::conventional()).unwrap();
    assert!(db.create_index("ghost", "f", IndexKind::Hash).is_err());
}

#[test]
fn telemetry_records_operations() {
    let t = Tmp::new("telemetry");
    let mut db = seeded(t.path(), 100);
    let snap = db.telemetry();
    assert!(snap.total_calls() >= 100);
    assert!(snap.write_fraction() > 0.9, "{}", snap.write_fraction());

    for i in 0..50u64 {
        db.get("users", RecordId(i)).unwrap();
    }
    let after = db.telemetry();
    assert!(
        after.write_fraction() < snap.write_fraction(),
        "reads were not recorded"
    );
}

#[test]
fn data_survives_a_reopen_and_indexes_rebuild_identically() {
    let t = Tmp::new("reopen");
    {
        let mut db = seeded(t.path(), 200);
        db.create_index("users", "country", IndexKind::Hash)
            .unwrap();
        db.checkpoint().unwrap();
    }
    let mut db = Database::open(t.path(), Policy::conventional()).unwrap();
    assert_eq!(db.count("users").unwrap(), 200);
    assert_eq!(db.count("users").unwrap(), 200);

    // The *definition* persists and the index is rebuilt on open. Its contents
    // are derived, so this is a scan rather than a restore — losing them costs
    // time, never data.
    assert_eq!(
        db.index_specs().len(),
        1,
        "the index definition did not survive a restart"
    );
    assert_eq!(db.index_specs()[0].field, "country");

    let q = LogicalPlan::new(LogicalOp::scan("users").filter(Expr::eq("country", "NO")));
    let with_rebuilt = db.query(&q).unwrap();
    assert_eq!(with_rebuilt.len(), 50);

    // And it answers identically to having no index at all.
    db.drop_index("users", "country", IndexKind::Hash);
    assert_eq!(db.query(&q).unwrap(), with_rebuilt);
}

#[test]
fn explain_shows_both_levels_and_the_chosen_access_path() {
    let t = Tmp::new("explain");
    let mut db = seeded(t.path(), 50);
    let q = LogicalPlan::new(LogicalOp::scan("users").filter(Expr::eq("country", "NO")));
    let before = db.explain(&q);
    assert!(before.contains("logical:"), "{before}");
    assert!(before.contains("HeapScan"), "{before}");

    db.create_index("users", "country", IndexKind::Hash)
        .unwrap();
    let after = db.explain(&q);
    assert!(after.contains("IndexLookup"), "{after}");
    assert!(after.contains("hash index"), "{after}");
}

#[test]
fn aggregate_values_are_correct_at_engine_level() {
    let t = Tmp::new("agg");
    let mut db = seeded(t.path(), 400);
    let q = LogicalPlan::new(
        LogicalOp::scan("users").aggregate(vec!["country".into()], vec![Agg::count("n")]),
    );
    let rows = db.query(&q).unwrap();
    assert_eq!(rows.len(), 4);
    for (_, r) in &rows {
        assert_eq!(r.get("n"), Some(&Value::U64(100)));
    }
}

mod auto_id {
    use super::*;

    fn rec(i: u64) -> Record {
        Record::new()
            .with("id", i)
            .with("country", COUNTRIES[(i % 4) as usize])
            .with("age", (i % 90) as i64)
            .with("name", format!("user-{i}"))
    }

    fn open(dir: &Path) -> Database {
        let mut db = Database::open(dir, Policy::conventional()).unwrap();
        db.create_collection("users", schema()).unwrap();
        db
    }

    #[test]
    fn auto_allocated_ids_are_monotonic_and_never_collide() {
        let t = Tmp::new("auto-monotonic");
        let mut db = open(t.path());
        let mut ids = Vec::new();
        for i in 0..500u64 {
            let id = db.insert_auto("users", rec(i)).unwrap();
            ids.push(id.0);
        }
        assert!(
            ids.windows(2).all(|w| w[0] < w[1]),
            "ids were not strictly increasing: {ids:?}"
        );
        let mut sorted = ids.clone();
        sorted.dedup();
        assert_eq!(sorted.len(), ids.len(), "an id was reused");
    }

    #[test]
    fn a_manual_insert_pushes_the_auto_counter_past_it() {
        let t = Tmp::new("auto-manual-mix");
        let mut db = open(t.path());
        db.insert("users", RecordId(100), rec(0)).unwrap();
        let next = db.insert_auto("users", rec(1)).unwrap();
        assert!(
            next.0 > 100,
            "an auto id ({}) collided with a manually-inserted one",
            next.0
        );
    }

    #[test]
    fn the_counter_survives_a_restart_without_reusing_a_deleted_id() {
        let t = Tmp::new("auto-restart");
        {
            let mut db = open(t.path());
            for i in 0..10u64 {
                db.insert_auto("users", rec(i)).unwrap();
            }
            // Delete the highest id. A naive "max existing id + 1" recomputation
            // would now hand that id straight back out.
            db.delete("users", RecordId(9)).unwrap();
            db.checkpoint().unwrap();
        }
        let mut db = Database::open(t.path(), Policy::conventional()).unwrap();
        let next = db.insert_auto("users", rec(99)).unwrap();
        assert!(
            next.0 >= 10,
            "the deleted id's slot was handed out again: got {}",
            next.0
        );
    }

    #[test]
    fn the_counter_survives_a_restart_with_uncheckpointed_writes() {
        // Everything after the last checkpoint is still only in the log, so the
        // counter recovered from the catalog alone would be behind. Recovery has
        // to advance it as those entries replay.
        let t = Tmp::new("auto-restart-uncheckpointed");
        {
            let mut db = open(t.path());
            for i in 0..5u64 {
                db.insert_auto("users", rec(i)).unwrap();
            }
            db.checkpoint().unwrap();
            for i in 5..15u64 {
                db.insert_auto("users", rec(i)).unwrap();
            }
            // No second checkpoint.
        }
        let db = Database::open(t.path(), Policy::conventional()).unwrap();
        let next = db.next_id("users").unwrap();
        assert!(next.0 >= 15, "got {}", next.0);
    }
}

mod batch_insert {
    use super::*;

    fn open(dir: &Path) -> Database {
        let mut db = Database::open(dir, Policy::conventional()).unwrap();
        db.create_collection("users", schema()).unwrap();
        db
    }

    fn rec(i: u64) -> Record {
        Record::new()
            .with("id", i)
            .with("country", COUNTRIES[(i % 4) as usize])
            .with("age", (i % 90) as i64)
            .with("name", format!("user-{i}"))
    }

    #[test]
    fn a_batch_insert_reindexes_every_record() {
        let t = Tmp::new("batch-reindex");
        let mut db = open(t.path());
        db.create_index("users", "country", IndexKind::Hash)
            .unwrap();

        let batch: Vec<(RecordId, Record)> = (0..300u64).map(|i| (RecordId(i), rec(i))).collect();
        let n = db.insert_batch("users", batch).unwrap();
        assert_eq!(n, 300);

        let q = LogicalPlan::new(LogicalOp::scan("users").filter(Expr::eq("country", "NO")));
        let rows = db.query(&q).unwrap();
        assert_eq!(rows.len(), 75, "the index did not see the batched rows");
    }

    #[test]
    fn a_batch_insert_answers_queries_identically_to_individual_inserts() {
        let a = Tmp::new("batch-parity-a");
        let b = Tmp::new("batch-parity-b");
        let mut individually = open(a.path());
        let mut batched = open(b.path());

        for i in 0..200u64 {
            individually.insert("users", RecordId(i), rec(i)).unwrap();
        }
        let recs: Vec<(RecordId, Record)> = (0..200u64).map(|i| (RecordId(i), rec(i))).collect();
        batched.insert_batch("users", recs).unwrap();

        assert_eq!(
            individually.scan("users").unwrap(),
            batched.scan("users").unwrap()
        );
    }
}

mod unique_constraints {
    use super::*;

    fn open(dir: &Path) -> Database {
        let mut db = Database::open(dir, Policy::conventional()).unwrap();
        db.create_collection("users", schema()).unwrap();
        db
    }

    fn rec(i: u64, country: &str) -> Record {
        Record::new()
            .with("id", i)
            .with("country", country)
            .with("age", (i % 90) as i64)
            .with("name", format!("user-{i}"))
    }

    #[test]
    fn a_second_write_with_the_same_value_is_refused() {
        let t = Tmp::new("uniq-basic");
        let mut db = open(t.path());
        db.add_unique_constraint("users", "name").unwrap();
        db.insert(
            "users",
            RecordId(1),
            Record::new().with("id", 1u64).with("name", "ada"),
        )
        .unwrap();
        let err = db
            .insert(
                "users",
                RecordId(2),
                Record::new().with("id", 2u64).with("name", "ada"),
            )
            .unwrap_err();
        assert!(matches!(
            err,
            adabt_core::error::Error::UniqueViolation { .. }
        ));
        // And nothing was written: the id was not consumed.
        assert_eq!(db.get("users", RecordId(2)).unwrap(), None);
    }

    #[test]
    fn declaring_a_constraint_over_existing_duplicates_is_refused() {
        let t = Tmp::new("uniq-preexisting");
        let mut db = open(t.path());
        db.insert("users", RecordId(1), rec(1, "NO")).unwrap();
        db.insert("users", RecordId(2), rec(2, "NO")).unwrap();
        let err = db.add_unique_constraint("users", "country").unwrap_err();
        assert!(matches!(
            err,
            adabt_core::error::Error::UniqueViolation { .. }
        ));
        // Refused, not partially applied: further duplicates are still legal.
        assert!(!db.has_unique_constraint("users", "country"));
        db.insert("users", RecordId(3), rec(3, "NO")).unwrap();
    }

    #[test]
    fn two_nulls_do_not_conflict() {
        let t = Tmp::new("uniq-nulls");
        let mut db = open(t.path());
        db.add_unique_constraint("users", "name").unwrap();
        db.insert("users", RecordId(1), Record::new().with("id", 1u64))
            .unwrap();
        db.insert("users", RecordId(2), Record::new().with("id", 2u64))
            .unwrap();
        assert_eq!(db.count("users").unwrap(), 2);
    }

    #[test]
    fn updating_a_record_to_keep_its_own_value_is_allowed() {
        let t = Tmp::new("uniq-self-update");
        let mut db = open(t.path());
        db.add_unique_constraint("users", "name").unwrap();
        let r = Record::new().with("id", 1u64).with("name", "ada");
        db.insert("users", RecordId(1), r.clone()).unwrap();
        // A no-op-ish update carrying the same value must not be refused as a
        // conflict with itself.
        assert!(db.update("users", RecordId(1), r).unwrap());
    }

    #[test]
    fn updating_one_record_to_anothers_value_is_refused() {
        let t = Tmp::new("uniq-update-conflict");
        let mut db = open(t.path());
        db.add_unique_constraint("users", "name").unwrap();
        db.insert(
            "users",
            RecordId(1),
            Record::new().with("id", 1u64).with("name", "ada"),
        )
        .unwrap();
        db.insert(
            "users",
            RecordId(2),
            Record::new().with("id", 2u64).with("name", "grace"),
        )
        .unwrap();
        let err = db
            .update(
                "users",
                RecordId(2),
                Record::new().with("id", 2u64).with("name", "ada"),
            )
            .unwrap_err();
        assert!(matches!(
            err,
            adabt_core::error::Error::UniqueViolation { .. }
        ));
        // The record was not overwritten.
        assert_eq!(
            db.get("users", RecordId(2)).unwrap().unwrap().get("name"),
            Some(&Value::from("grace"))
        );
    }

    #[test]
    fn a_constraint_persists_across_a_restart_and_still_enforces() {
        let t = Tmp::new("uniq-restart");
        {
            let mut db = open(t.path());
            db.add_unique_constraint("users", "name").unwrap();
            db.insert(
                "users",
                RecordId(1),
                Record::new().with("id", 1u64).with("name", "ada"),
            )
            .unwrap();
            db.checkpoint().unwrap();
        }
        let mut db = Database::open(t.path(), Policy::conventional()).unwrap();
        assert!(db.has_unique_constraint("users", "name"));
        let err = db
            .insert(
                "users",
                RecordId(2),
                Record::new().with("id", 2u64).with("name", "ada"),
            )
            .unwrap_err();
        assert!(matches!(
            err,
            adabt_core::error::Error::UniqueViolation { .. }
        ));
    }

    #[test]
    fn a_batch_with_a_conflicting_pair_inserts_nothing() {
        let t = Tmp::new("uniq-batch-internal");
        let mut db = open(t.path());
        db.add_unique_constraint("users", "name").unwrap();
        let recs = vec![
            (
                RecordId(1),
                Record::new().with("id", 1u64).with("name", "ada"),
            ),
            (
                RecordId(2),
                Record::new().with("id", 2u64).with("name", "grace"),
            ),
            // Duplicate of record 1, within the same batch.
            (
                RecordId(3),
                Record::new().with("id", 3u64).with("name", "ada"),
            ),
        ];
        let err = db.insert_batch("users", recs).unwrap_err();
        assert!(matches!(
            err,
            adabt_core::error::Error::UniqueViolation { .. }
        ));
        assert_eq!(db.count("users").unwrap(), 0, "a partial batch was written");
    }

    #[test]
    fn a_batch_conflicting_with_existing_data_inserts_nothing() {
        let t = Tmp::new("uniq-batch-existing");
        let mut db = open(t.path());
        db.add_unique_constraint("users", "name").unwrap();
        db.insert(
            "users",
            RecordId(0),
            Record::new().with("id", 0u64).with("name", "ada"),
        )
        .unwrap();
        let recs = vec![
            (
                RecordId(1),
                Record::new().with("id", 1u64).with("name", "grace"),
            ),
            (
                RecordId(2),
                Record::new().with("id", 2u64).with("name", "ada"),
            ),
        ];
        assert!(db.insert_batch("users", recs).is_err());
        assert_eq!(db.count("users").unwrap(), 1, "the batch partially landed");
    }

    #[test]
    fn a_valid_batch_still_enforces_the_constraint_going_forward() {
        let t = Tmp::new("uniq-batch-valid");
        let mut db = open(t.path());
        db.add_unique_constraint("users", "name").unwrap();
        let recs = vec![
            (
                RecordId(1),
                Record::new().with("id", 1u64).with("name", "ada"),
            ),
            (
                RecordId(2),
                Record::new().with("id", 2u64).with("name", "grace"),
            ),
        ];
        assert_eq!(db.insert_batch("users", recs).unwrap(), 2);
        let err = db
            .insert(
                "users",
                RecordId(3),
                Record::new().with("id", 3u64).with("name", "grace"),
            )
            .unwrap_err();
        assert!(matches!(
            err,
            adabt_core::error::Error::UniqueViolation { .. }
        ));
    }

    #[test]
    fn the_backing_index_is_never_retracted_while_the_constraint_stands() {
        // The property M16's pinned-scope rule exists to guarantee: dropping the
        // index would not merely slow queries down, it would let the very
        // violation the constraint exists to prevent happen silently.
        let t = Tmp::new("uniq-pinned");
        let mut db = Database::open(
            t.path(),
            Policy {
                mode: adabt_core::policy::Mode::Adaptive,
                priority: adabt_core::policy::Priorities {
                    speed: 1,
                    resources: 10,
                    freedom: 5,
                },
                ..Policy::conventional()
            },
        )
        .unwrap();
        db.create_collection("users", schema()).unwrap();
        db.add_unique_constraint("users", "name").unwrap();
        assert_eq!(db.index_specs().len(), 1);

        // Drive many optimization cycles under a resource-hungry priority, which
        // is exactly the pressure that would otherwise retract an index nobody
        // is querying through.
        for i in 0..2_000u64 {
            db.insert(
                "users",
                RecordId(i),
                Record::new().with("id", i).with("name", format!("n{i}")),
            )
            .unwrap();
            if i % 50 == 0 {
                db.optimize().unwrap();
            }
        }
        assert_eq!(
            db.index_specs().len(),
            1,
            "the constraint's backing index was retracted"
        );
    }

    #[test]
    fn dropping_the_constraint_leaves_the_index_but_stops_enforcing() {
        let t = Tmp::new("uniq-drop");
        let mut db = open(t.path());
        db.add_unique_constraint("users", "name").unwrap();
        assert!(db.drop_unique_constraint("users", "name").unwrap());
        assert!(!db.has_unique_constraint("users", "name"));
        assert_eq!(db.index_specs().len(), 1, "the backing index was removed");

        db.insert(
            "users",
            RecordId(1),
            Record::new().with("id", 1u64).with("name", "ada"),
        )
        .unwrap();
        db.insert(
            "users",
            RecordId(2),
            Record::new().with("id", 2u64).with("name", "ada"),
        )
        .unwrap();
        assert_eq!(db.count("users").unwrap(), 2);
    }
}

mod expression_completeness {
    use super::*;

    fn open(dir: &Path) -> Database {
        let mut db = Database::open(dir, Policy::conventional()).unwrap();
        db.create_collection("users", schema()).unwrap();
        for i in 0..20u64 {
            db.insert(
                "users",
                RecordId(i),
                Record::new()
                    .with("id", i)
                    .with("country", COUNTRIES[(i % 4) as usize])
                    .with("age", (20 + i) as i64)
                    .with("name", format!("user-{i}")),
            )
            .unwrap();
        }
        db
    }

    #[test]
    fn arithmetic_filters_a_real_query() {
        let t = Tmp::new("expr-arith");
        let mut db = open(t.path());
        // age + 10 > 35  <=>  age > 25  <=>  i > 5 (age = 20 + i)
        let plan =
            LogicalPlan::new(LogicalOp::scan("users").filter(
                (Expr::field("age") + Expr::lit(10i64)).compare(CmpOp::Gt, Expr::lit(35i64)),
            ));
        let rows = db.query(&plan).unwrap();
        assert_eq!(rows.len(), 14, "expected ages 26..40 to match (i=6..19)");
    }

    #[test]
    fn in_filters_a_real_query() {
        let t = Tmp::new("expr-in");
        let mut db = open(t.path());
        let plan = LogicalPlan::new(
            LogicalOp::scan("users").filter(Expr::field("country").in_values(["NO", "SE"])),
        );
        let rows = db.query(&plan).unwrap();
        assert_eq!(rows.len(), 10);
    }

    #[test]
    fn like_filters_a_real_query() {
        let t = Tmp::new("expr-like");
        let mut db = open(t.path());
        let plan =
            LogicalPlan::new(LogicalOp::scan("users").filter(Expr::field("name").like("user-1_")));
        let rows = db.query(&plan).unwrap();
        // user-10 .. user-19
        assert_eq!(rows.len(), 10);
    }

    // Join execution itself — including the "refuses" cases that belong to
    // this milestone rather than expression evaluation — now has its own
    // dedicated suite: see `tests/joins.rs`. A single `Join` reaching
    // `Database::query` was refused outright through M20-M22; M23 is where
    // that stopped being true, so the tests asserting refusal moved rather
    // than staying here asserting something no longer correct.
}

/// `Database::alter_schema` and `ShardedDatabase::alter_schema`: the public
/// entry point an application uses to evolve its own schema, as opposed to
/// `freeze_schema`'s auto-inferred tightening. `adabt-storage`'s own test
/// suite (`schema_evolution.rs`, `codec::in_place_eligibility`) is the
/// byte-level evidence for which changes are free; this only has to show the
/// engine layer wires that up correctly — in particular, that anything
/// derived from the old layout is not left stale.
mod schema_evolution {
    use super::*;

    fn fixed_schema() -> Schema {
        Schema::new(
            SchemaMode::Fixed,
            vec![
                FieldDef::new("id", FieldType::U64).required(),
                FieldDef::new("age", FieldType::I64).required(),
            ],
        )
        .unwrap()
    }

    #[test]
    fn an_eligible_append_is_visible_immediately_through_the_public_api() {
        let t = Tmp::new("evo-append");
        let mut db = Database::open(t.path(), Policy::conventional()).unwrap();
        db.create_collection("users", fixed_schema()).unwrap();
        for i in 0..20u64 {
            db.insert(
                "users",
                RecordId(i),
                Record::new().with("id", i).with("age", i as i64),
            )
            .unwrap();
        }

        let widened = Schema::new(
            SchemaMode::Fixed,
            vec![
                FieldDef::new("id", FieldType::U64).required(),
                FieldDef::new("age", FieldType::I64).required(),
                FieldDef::new("score", FieldType::I64),
            ],
        )
        .unwrap();
        let rewritten = db.alter_schema("users", widened).unwrap();
        assert_eq!(rewritten, 0, "an eligible append should rewrite no rows");
        assert_eq!(
            db.get("users", RecordId(5)).unwrap().unwrap().get("score"),
            None
        );

        db.insert(
            "users",
            RecordId(20),
            Record::new()
                .with("id", 20u64)
                .with("age", 20i64)
                .with("score", 7i64),
        )
        .unwrap();
        assert_eq!(
            db.get("users", RecordId(20)).unwrap().unwrap().get("score"),
            Some(&Value::I64(7))
        );
    }

    #[test]
    fn a_hash_index_stays_correct_across_an_in_place_alter() {
        // The index was built against the old codec's shape; if
        // `alter_schema` did not invalidate it, this would either return
        // stale results or fail outright once the layout no longer matches
        // what the index remembers.
        let t = Tmp::new("evo-index");
        let mut db = Database::open(t.path(), Policy::conventional()).unwrap();
        db.create_collection("users", fixed_schema()).unwrap();
        for i in 0..30u64 {
            db.insert(
                "users",
                RecordId(i),
                Record::new().with("id", i).with("age", (i % 5) as i64),
            )
            .unwrap();
        }
        db.create_index("users", "age", IndexKind::Hash).unwrap();
        let q = LogicalPlan::new(LogicalOp::scan("users").filter(Expr::eq("age", 3i64)));
        let before = db.query(&q).unwrap().len();
        assert!(before > 0);

        let widened = Schema::new(
            SchemaMode::Fixed,
            vec![
                FieldDef::new("id", FieldType::U64).required(),
                FieldDef::new("age", FieldType::I64).required(),
                FieldDef::new("score", FieldType::I64),
            ],
        )
        .unwrap();
        db.alter_schema("users", widened).unwrap();

        assert_eq!(db.query(&q).unwrap().len(), before);
        db.insert(
            "users",
            RecordId(30),
            Record::new().with("id", 30u64).with("age", 3i64),
        )
        .unwrap();
        assert_eq!(db.query(&q).unwrap().len(), before + 1);
    }

    #[test]
    fn an_ineligible_change_still_succeeds_by_copying() {
        let t = Tmp::new("evo-copy");
        let mut db = Database::open(t.path(), Policy::conventional()).unwrap();
        db.create_collection("users", schema()).unwrap();
        for i in 0..15u64 {
            db.insert(
                "users",
                RecordId(i),
                Record::new()
                    .with("id", i)
                    .with("country", "NO")
                    .with("age", i as i64)
                    .with("name", format!("user-{i}")),
            )
            .unwrap();
        }
        // Drops "country" from the middle, keeping "name" trailing — the
        // eligibility rule only ever looks at the *last* field, so this must
        // copy-and-swap regardless of mode.
        let narrowed = Schema::new(
            SchemaMode::Strict,
            vec![
                FieldDef::new("id", FieldType::U64).required(),
                FieldDef::new("age", FieldType::I64),
                FieldDef::new("name", FieldType::Str { max_len: Some(32) }),
            ],
        )
        .unwrap();
        let rewritten = db.alter_schema("users", narrowed).unwrap();
        assert_eq!(rewritten, 15, "dropping a middle field must copy-and-swap");
        for i in 0..15u64 {
            let got = db.get("users", RecordId(i)).unwrap().unwrap();
            assert_eq!(got.get("country"), None);
            assert_eq!(got.get("name"), Some(&Value::Str(format!("user-{i}"))));
        }
    }

    #[test]
    fn sharded_database_applies_the_change_to_every_shard() {
        use adabt_engine::sharded::ShardedDatabase;

        let t = Tmp::new("evo-sharded");
        let sdb = ShardedDatabase::open(t.path(), 4, Policy::conventional()).unwrap();
        sdb.create_collection("users", fixed_schema()).unwrap();
        for i in 0..40u64 {
            sdb.insert(
                "users",
                RecordId(i),
                Record::new().with("id", i).with("age", i as i64),
            )
            .unwrap();
        }

        let widened = Schema::new(
            SchemaMode::Fixed,
            vec![
                FieldDef::new("id", FieldType::U64).required(),
                FieldDef::new("age", FieldType::I64).required(),
                FieldDef::new("score", FieldType::I64),
            ],
        )
        .unwrap();
        sdb.alter_schema("users", widened).unwrap();

        for i in 0..40u64 {
            let got = sdb.get("users", RecordId(i)).unwrap().unwrap();
            assert_eq!(got.get("score"), None);
        }
        sdb.insert(
            "users",
            RecordId(40),
            Record::new()
                .with("id", 40u64)
                .with("age", 40i64)
                .with("score", 1i64),
        )
        .unwrap();
        assert_eq!(
            sdb.get("users", RecordId(40))
                .unwrap()
                .unwrap()
                .get("score"),
            Some(&Value::I64(1))
        );
    }
}
