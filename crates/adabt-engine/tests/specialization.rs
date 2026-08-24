//! Physical self-specialization: a schemaless collection becoming a directly
//! addressable one, without its API changing.

use adabt_core::ids::RecordId;
use adabt_core::policy::Policy;
use adabt_core::record::Record;
use adabt_core::schema::{Schema, SchemaMode};
use adabt_core::store::LogicalStore;
use adabt_core::value::Value;
use adabt_engine::Database;
use adabt_ir::plan::{LogicalOp, LogicalPlan};
use adabt_ir::Expr;
use std::path::{Path, PathBuf};

struct Tmp(PathBuf);
impl Tmp {
    fn new(tag: &str) -> Self {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "adabt-spec-{tag}-{}-{:?}",
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

/// A collection that starts schemaless but is used with a consistent shape.
fn loose(dir: &Path, n: u64) -> Database {
    let mut db = Database::open(dir, Policy::manual(0)).unwrap();
    db.create_collection("events", Schema::dynamic()).unwrap();
    for i in 0..n {
        db.insert(
            "events",
            RecordId(i),
            Record::new()
                .with("id", i)
                .with("kind", if i % 2 == 0 { "click" } else { "view" })
                .with("weight", (i % 100) as i64),
        )
        .unwrap();
    }
    db
}

#[test]
fn a_schemaless_collection_that_settled_becomes_directly_addressable() {
    // The transition the whole design exists to make possible.
    let t = Tmp::new("freeze");
    let mut db = loose(t.path(), 3_000);

    assert_eq!(db.schema_of("events").unwrap().mode(), SchemaMode::Dynamic);
    assert!(!db.has_direct_array("events"));

    // The API used before and after is identical.
    let q = LogicalPlan::new(LogicalOp::scan("events").filter(Expr::eq("kind", "click")));
    let before_query = db.query(&q).unwrap();
    let before_get: Vec<_> = (0..200u64)
        .map(|i| db.get("events", RecordId(i)).unwrap())
        .collect();

    let inferred = db.freeze_schema("events").unwrap();
    assert!(inferred.is_fixed(), "{:?}", inferred.rejected);
    assert_eq!(db.schema_of("events").unwrap().mode(), SchemaMode::Fixed);

    // Same calls, same answers.
    assert_eq!(
        db.query(&q).unwrap(),
        before_query,
        "freezing changed a query result"
    );
    let after_get: Vec<_> = (0..200u64)
        .map(|i| db.get("events", RecordId(i)).unwrap())
        .collect();
    assert_eq!(after_get, before_get, "freezing changed a record");

    // And the physical endpoint is now reachable.
    db.set_level(10).unwrap();
    assert!(
        db.has_direct_array("events"),
        "a frozen collection did not become directly addressable: {}",
        db.config().describe()
    );
    let addressed: Vec<_> = (0..200u64)
        .map(|i| db.get("events", RecordId(i)).unwrap())
        .collect();
    assert_eq!(addressed, before_get, "direct addressing changed a record");
}

#[test]
fn freezing_reports_what_the_collection_gives_up() {
    let t = Tmp::new("cost");
    let mut db = loose(t.path(), 1_500);
    let inferred = db.freeze_schema("events").unwrap();
    let cost = inferred.describe_cost();
    assert!(cost.contains("restricted to 3 field"), "{cost}");

    // The constraint is real: a record with a new field is now refused.
    let extra = Record::new()
        .with("id", 9_999u64)
        .with("kind", "click")
        .with("weight", 1i64)
        .with("surprise", 1i64);
    assert!(
        db.insert("events", RecordId(9_999), extra).is_err(),
        "a frozen collection accepted an undeclared field"
    );
}

#[test]
fn a_collection_with_a_mixed_field_is_not_frozen_to_fixed() {
    let t = Tmp::new("mixed");
    let mut db = Database::open(t.path(), Policy::manual(0)).unwrap();
    db.create_collection("events", Schema::dynamic()).unwrap();
    for i in 0..2_000u64 {
        let v: Value = if i % 2 == 0 {
            Value::I64(i as i64)
        } else {
            Value::Str(format!("s{i}"))
        };
        db.insert(
            "events",
            RecordId(i),
            Record::new().with("id", i).with("v", v),
        )
        .unwrap();
    }
    let inferred = db.freeze_schema("events").unwrap();
    assert!(!inferred.is_fixed());
    assert_eq!(db.schema_of("events").unwrap().mode(), SchemaMode::Declared);
    // Every existing record still reads back.
    for i in 0..2_000u64 {
        assert!(
            db.get("events", RecordId(i)).unwrap().is_some(),
            "record {i} lost"
        );
    }
}

#[test]
fn freezing_an_already_rigid_collection_is_refused() {
    let t = Tmp::new("already");
    let mut db = loose(t.path(), 1_200);
    db.freeze_schema("events").unwrap();
    let again = db.freeze_schema("events");
    assert!(
        again.is_err(),
        "freezing a Fixed collection again should be refused rather than churn"
    );
}

#[test]
fn a_frozen_schema_survives_a_restart() {
    let t = Tmp::new("restart");
    {
        let mut db = loose(t.path(), 1_500);
        db.freeze_schema("events").unwrap();
        db.checkpoint().unwrap();
    }
    let mut db = Database::open(t.path(), Policy::manual(0)).unwrap();
    assert_eq!(
        db.schema_of("events").unwrap().mode(),
        SchemaMode::Fixed,
        "the frozen schema did not survive"
    );
    assert_eq!(db.count("events").unwrap(), 1_500);
    for i in [0u64, 700, 1_499] {
        let r = db.get("events", RecordId(i)).unwrap().expect("record lost");
        assert_eq!(r.get("id"), Some(&Value::U64(i)));
    }
}

#[test]
fn a_failed_freeze_leaves_the_collection_untouched() {
    // Validation happens before anything is written: a partial freeze would
    // leave a collection whose own schema rejects its stored records, which no
    // later operation could repair.
    let t = Tmp::new("atomic");
    let mut db = loose(t.path(), 1_200);
    db.freeze_schema("events").unwrap();
    let before: Vec<_> = (0..50u64)
        .map(|i| db.get("events", RecordId(i)).unwrap())
        .collect();

    assert!(db.freeze_schema("events").is_err());
    let after: Vec<_> = (0..50u64)
        .map(|i| db.get("events", RecordId(i)).unwrap())
        .collect();
    assert_eq!(before, after, "a refused freeze modified the collection");
}

#[test]
fn freezing_is_never_done_automatically() {
    // It is irreversible, so it is not the optimizer's decision. A human
    // choosing level 8 is; the adaptive driver is not.
    let t = Tmp::new("manual-only");
    let mut db = Database::open(
        t.path(),
        Policy {
            mode: adabt_core::policy::Mode::Adaptive,
            priority: adabt_core::policy::Priorities {
                speed: 10,
                resources: 10,
                freedom: 0,
            },
            ..Policy::conventional()
        },
    )
    .unwrap();
    db.create_collection("events", Schema::dynamic()).unwrap();
    for i in 0..3_000u64 {
        db.insert(
            "events",
            RecordId(i),
            Record::new().with("id", i).with("weight", (i % 50) as i64),
        )
        .unwrap();
    }
    let q = LogicalPlan::new(LogicalOp::scan("events").filter(Expr::eq("weight", 5i64)));
    for _ in 0..10 {
        for _ in 0..60 {
            db.query(&q).unwrap();
        }
        db.optimize().unwrap();
    }
    assert_eq!(
        db.schema_of("events").unwrap().mode(),
        SchemaMode::Dynamic,
        "the driver froze a schema on its own:\n{}",
        db.explain_optimizations()
    );
}

#[test]
fn a_hot_identity_lookup_gets_specialised_and_still_answers_identically() {
    let t = Tmp::new("compiled");
    let mut db = loose(t.path(), 2_000);
    db.freeze_schema("events").unwrap();
    db.set_level(10).unwrap();
    assert!(db.has_direct_array("events"));

    let plans: Vec<LogicalPlan> = (0..500u64)
        .map(|i| LogicalPlan::new(LogicalOp::get("events", RecordId(i))))
        .collect();

    // Answers before specialisation kicks in.
    let general: Vec<_> = plans.iter().map(|p| db.query(p).unwrap()).collect();
    assert!(
        db.compiled_paths() > 0,
        "a shape called 500 times was never specialised"
    );

    // And after. Same API, same answers, different machinery.
    let specialised: Vec<_> = plans.iter().map(|p| db.query(p).unwrap()).collect();
    assert_eq!(specialised, general, "the compiled path changed an answer");
    assert!(db.compiled_hits() > 0, "the compiled path was never taken");
    for (i, rows) in specialised.iter().enumerate() {
        assert_eq!(rows.len(), 1, "record {i} lost on the compiled path");
        assert_eq!(rows[0].0, RecordId(i as u64));
    }
}

#[test]
fn changing_the_layout_invalidates_compiled_paths() {
    // A compiled path encodes what exists. If the direct array goes away, a
    // path still reaching for it is wrong rather than merely stale.
    let t = Tmp::new("invalidate");
    let mut db = loose(t.path(), 2_000);
    db.freeze_schema("events").unwrap();
    db.set_level(10).unwrap();

    let q = LogicalPlan::new(LogicalOp::get("events", RecordId(7)));
    for _ in 0..400 {
        db.query(&q).unwrap();
    }
    assert!(db.compiled_paths() > 0);
    let before = db.query(&q).unwrap();

    db.set_level(1).unwrap();
    assert!(!db.has_direct_array("events"));
    assert_eq!(
        db.compiled_paths(),
        0,
        "a specialisation outlived its layout"
    );
    assert_eq!(
        db.query(&q).unwrap(),
        before,
        "invalidation changed an answer"
    );
}

#[test]
fn only_shapes_worth_specialising_are_specialised() {
    let t = Tmp::new("selective");
    let mut db = loose(t.path(), 2_000);
    // A scan still has real work to do; skipping the general path would mean
    // reimplementing it.
    let q = LogicalPlan::new(LogicalOp::scan("events").filter(Expr::eq("kind", "click")));
    for _ in 0..600 {
        db.query(&q).unwrap();
    }
    assert_eq!(
        db.compiled_paths(),
        0,
        "a filtered scan was specialised, which would mean duplicating the executor"
    );
}
