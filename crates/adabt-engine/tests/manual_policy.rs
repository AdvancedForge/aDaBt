//! `Mode::Manual`'s `overrides`, now that they can name a scope and carry
//! params rather than only toggling something globally.
//!
//! The property under test throughout: a scoped override reaches exactly the
//! place it named, an unscoped one still expands the way it always did, and
//! an explicit param — an index kind — is honored rather than silently
//! recomputed from telemetry the caller already knew and overrode.

use adabt_core::ids::RecordId;
use adabt_core::index_kind::IndexKind;
use adabt_core::policy::{Mode, Override, Policy};
use adabt_core::record::Record;
use adabt_core::schema::{FieldDef, FieldType, Schema, SchemaMode};
use adabt_core::store::LogicalStore;
use adabt_engine::Database;
use adabt_exec::physical::PhysicalOp;
use adabt_ir::plan::{LogicalOp, LogicalPlan};
use adabt_ir::{CmpOp, Expr};
use std::path::{Path, PathBuf};

struct Tmp(PathBuf);
impl Tmp {
    fn new(tag: &str) -> Self {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "adabt-manual-policy-{tag}-{}-{:?}",
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
        ],
    )
    .unwrap()
}

fn manual(level: u8, overrides: Vec<Override>) -> Policy {
    Policy {
        mode: Mode::Manual { level, overrides },
        ..Policy::conventional()
    }
}

/// Enough rows and enough repeated filtering on both `country` and `age`
/// to satisfy `auto_index`'s applicability gate (≥1000 rows, ≥8 filters) on
/// *both* fields — so a test can prove a scoped override reaches only the
/// field it named, not every field that happens to qualify.
fn seed_and_query(db: &mut Database) {
    db.create_collection("users", schema()).unwrap();
    for i in 0..1200u64 {
        db.insert(
            "users",
            RecordId(i),
            Record::new()
                .with("id", i)
                .with("country", if i % 2 == 0 { "NO" } else { "SE" })
                .with("age", (i % 60) as i64),
        )
        .unwrap();
    }
    for _ in 0..10 {
        db.query(&LogicalPlan::new(
            LogicalOp::scan("users").filter(Expr::eq("country", "NO")),
        ))
        .unwrap();
        db.query(&LogicalPlan::new(
            LogicalOp::scan("users").filter(Expr::cmp("age", CmpOp::Gt, 30i64)),
        ))
        .unwrap();
    }
}

fn access_path_kind(db: &Database, field: &str) -> Option<IndexKind> {
    let plan = LogicalPlan::new(LogicalOp::scan("users").filter(Expr::eq(field, "NO")));
    match db.plan(&plan).root.access_path() {
        PhysicalOp::IndexLookup { kind, .. } => Some(*kind),
        _ => None,
    }
}

#[test]
fn an_unscoped_toggle_still_expands_to_every_qualifying_field() {
    // The pre-existing behaviour, unchanged: `Override::toggle` defaults to
    // scope "global", which for a `PerField` optimization still means "every
    // field that currently qualifies" — exactly what a bare `(name, true)`
    // tuple meant before `scope` existed.
    let t = Tmp::new("unscoped");
    let mut db = Database::open(
        t.path(),
        manual(2, vec![Override::toggle("auto_index", true)]),
    )
    .unwrap();
    seed_and_query(&mut db);
    db.optimize().unwrap();

    assert!(
        access_path_kind(&db, "country").is_some(),
        "country should be indexed"
    );
    assert!(
        access_path_kind(&db, "age").is_some(),
        "age should be indexed too"
    );
}

#[test]
fn a_scoped_override_targets_exactly_the_named_field() {
    let t = Tmp::new("scoped");
    let mut db = Database::open(
        t.path(),
        manual(
            2,
            vec![Override::scoped("auto_index", "users.country", true)],
        ),
    )
    .unwrap();
    seed_and_query(&mut db);
    db.optimize().unwrap();

    assert!(
        access_path_kind(&db, "country").is_some(),
        "the named field should be indexed"
    );
    assert!(
        access_path_kind(&db, "age").is_none(),
        "a field the override did not name should not be indexed, even though it also qualifies"
    );
}

#[test]
fn an_explicit_index_kind_is_honored_over_the_telemetry_guess() {
    // `country` is filtered by equality only, which `index_kind_for` alone
    // would resolve to `Hash` — the point is that the override's own `kind`
    // param wins anyway.
    let t = Tmp::new("kind");
    let mut db = Database::open(
        t.path(),
        manual(
            2,
            vec![Override::scoped("auto_index", "users.country", true)
                .with_param("kind", IndexKind::BTree.as_ordinal())],
        ),
    )
    .unwrap();
    seed_and_query(&mut db);
    db.optimize().unwrap();

    assert_eq!(access_path_kind(&db, "country"), Some(IndexKind::BTree));
}

#[test]
fn with_no_kind_param_the_telemetry_guess_still_applies() {
    let t = Tmp::new("default-kind");
    let mut db = Database::open(
        t.path(),
        manual(
            2,
            vec![Override::scoped("auto_index", "users.country", true)],
        ),
    )
    .unwrap();
    seed_and_query(&mut db);
    db.optimize().unwrap();

    // Equality-only filtering on `country` — the telemetry-driven default.
    assert_eq!(access_path_kind(&db, "country"), Some(IndexKind::Hash));
}

#[test]
fn an_override_naming_an_unregistered_optimization_is_reported_at_open() {
    let t = Tmp::new("typo");
    match Database::open(
        t.path(),
        manual(0, vec![Override::toggle("auto_indx", true)]),
    ) {
        Err(e) => {
            assert!(
                matches!(e, adabt_core::error::Error::InvalidOptimization(_)),
                "{e}"
            );
            assert!(e.to_string().contains("auto_indx"), "{e}");
        }
        Ok(_) => panic!("a typo'd optimization name should be reported at open"),
    }
}

#[test]
fn compile_identity_lookups_installs_the_path_before_the_threshold() {
    let t = Tmp::new("compile-now");
    let mut db = Database::open(t.path(), Policy::manual(0)).unwrap();
    db.create_collection("users", schema()).unwrap();
    db.insert(
        "users",
        RecordId(1),
        Record::new()
            .with("id", 1u64)
            .with("country", "NO")
            .with("age", 30i64),
    )
    .unwrap();
    assert_eq!(db.compiled_paths(), 0);

    db.compile_identity_lookups("users").unwrap();
    assert_eq!(
        db.compiled_paths(),
        1,
        "forcing must not wait for HOT_THRESHOLD calls"
    );

    // The compiled path is consulted by `query`, not by the direct `get()`
    // convenience method — `get` is already the minimal path and has nothing
    // to specialise away.
    let hits_before = db.compiled_hits();
    db.query(&LogicalPlan::new(LogicalOp::get("users", RecordId(1))))
        .unwrap();
    assert_eq!(
        db.compiled_hits(),
        hits_before + 1,
        "the forced path should actually be used, not just recorded"
    );
}

#[test]
fn compile_identity_lookups_rejects_an_unknown_collection() {
    let t = Tmp::new("compile-missing");
    let mut db = Database::open(t.path(), Policy::manual(0)).unwrap();
    assert!(db.compile_identity_lookups("ghosts").is_err());
}

#[test]
fn a_disabled_scoped_override_still_leaves_the_database_usable() {
    let t = Tmp::new("disabled");
    let mut db = Database::open(
        t.path(),
        manual(
            0,
            vec![Override::scoped("auto_index", "users.country", false)],
        ),
    )
    .unwrap();
    seed_and_query(&mut db);
    db.optimize().unwrap();
    assert_eq!(db.count("users").unwrap(), 1200);
}
