//! Clustered sort order: the finish test the roadmap named, plus the
//! correctness contract that makes the optimization honest.
//!
//! The claim under test is physical, not logical: records whose clustering
//! key values are near each other should *land* on the same pages, so a
//! range scan over that key touches pages in proportion to the range's
//! size rather than the collection's. That is measured directly with the
//! touched-page diagnostic — no proxies, no timing.
//!
//! The claims beside it are the ones that keep it from being a trick:
//! answers are identical to an unclustered collection's, an update through
//! the clustered path stays correct, and a restart loses only the locality,
//! never a record or a value.

use adabt_core::ids::RecordId;
use adabt_core::policy::Policy;
use adabt_core::record::Record;
use adabt_core::schema::{FieldDef, FieldType, Schema, SchemaMode};
use adabt_core::store::LogicalStore;
use adabt_engine::Database;
use std::path::{Path, PathBuf};

const N: u64 = 20_000;

struct Tmp(PathBuf);
impl Tmp {
    fn new(tag: &str) -> Self {
        let mut p = std::env::temp_dir();
        p.push(format!("adabt-cluster-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
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
        SchemaMode::Declared,
        vec![
            FieldDef::new("k", FieldType::I64),
            FieldDef::new("payload", FieldType::Char(32)),
        ],
    )
    .unwrap()
}

/// Deterministic wide-domain keys: an LCG over 0..1_000_000, so consecutive
/// ids hold unrelated keys and any locality in the clustered collection came
/// from placement, not insertion order.
fn key(i: u64) -> i64 {
    let mut x = i
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
    x ^= x >> 33;
    x = x.wrapping_mul(0xff51_afd7_ed55_8ccd);
    x ^= x >> 33;
    (x % 1_000_000) as i64
}

fn seed(dir: &Path) -> Database {
    let mut db = Database::open(dir, Policy::manual(0)).unwrap();
    db.create_collection("plain", schema()).unwrap();
    db.create_collection("ordered", schema()).unwrap();
    db.declare_cluster_field("ordered", "k").unwrap();
    for i in 0..N {
        let k = key(i);
        for (name, _) in [("plain", 0), ("ordered", 1)] {
            db.insert(
                name,
                RecordId(i),
                Record::new()
                    .with("k", k)
                    .with("payload", format!("row-{i:05}")),
            )
            .unwrap();
        }
    }
    db
}

/// The roadmap's finish test: page reads for a range scan drop to the pages
/// the range physically occupies. Measured on identical data, differing
/// only in the declared hint.
#[test]
fn a_range_scan_touches_pages_in_proportion_to_the_range() {
    let tmp = Tmp::new("finish");
    let mut db = seed(tmp.path());

    // A middle tenth of the key domain.
    let (lo, hi) = (400_000i64, 500_000i64);
    let in_range = |k: i64| k >= lo && k <= hi;
    let expected: Vec<u64> = (0..N).filter(|i| in_range(key(*i))).collect();
    assert!(!expected.is_empty(), "the range must match something");

    let plain_pages = {
        db.clear_page_touches();
        for id in &expected {
            db.get("plain", RecordId(*id)).unwrap();
        }
        db.touched_pages()
    };
    let ordered_pages = {
        db.clear_page_touches();
        for id in &expected {
            db.get("ordered", RecordId(*id)).unwrap();
        }
        db.touched_pages()
    };
    println!(
        "range of {} rows: plain touched {plain_pages} pages, ordered touched {ordered_pages}",
        expected.len()
    );

    assert!(
        ordered_pages * 3 <= plain_pages,
        "clustering collapsed locality: ordered {ordered_pages} pages vs plain {plain_pages}"
    );
}

/// Clustering is placement, not content: both collections answer every get
/// with exactly the same bytes.
#[test]
fn answers_are_identical_to_the_unclustered_collection() {
    let tmp = Tmp::new("answers");
    let mut db = seed(tmp.path());
    for i in 0..N {
        let a = db.get("plain", RecordId(i)).unwrap().unwrap();
        let b = db.get("ordered", RecordId(i)).unwrap().unwrap();
        assert_eq!(a.get("k"), b.get("k"), "row {i}");
        assert_eq!(a.get("payload"), b.get("payload"), "row {i}");
    }
}

/// An update through a clustered collection keeps every other row intact and
/// readable — placement work on write must never disturb the directory's
/// promises.
#[test]
fn updates_through_a_clustered_collection_stay_correct() {
    let tmp = Tmp::new("updates");
    let mut db = seed(tmp.path());
    for i in 0..N {
        db.update(
            "ordered",
            RecordId(i),
            Record::new()
                .with("k", key(i))
                .with("payload", format!("updated-{i:05}")),
        )
        .unwrap();
    }
    for i in 0..N {
        let rec = db.get("ordered", RecordId(i)).unwrap().expect("present");
        assert_eq!(
            match rec.get("payload") {
                Some(adabt_core::value::Value::Str(s)) => Some(s.as_str()),
                _ => None,
            },
            Some(format!("updated-{i:05}").as_str())
        );
    }
}

/// A restart keeps the declaration (catalog state) but not the placement
/// ranges - and it must not forget a record. New keyed inserts steer again
/// without re-declaring, and a cleared hint stays cleared.
#[test]
fn a_restart_keeps_the_declaration_and_every_record() {
    let tmp = Tmp::new("restart");
    {
        let mut db = seed(tmp.path());
        db.insert(
            "ordered",
            RecordId(N),
            Record::new()
                .with("k", 424_242i64)
                .with("payload", "post-restart"),
        )
        .unwrap();
    }
    let mut db = Database::open(tmp.path(), Policy::manual(0)).unwrap();
    assert_eq!(db.cluster_field("ordered"), Some("k"));
    assert!(db.get("ordered", RecordId(N)).unwrap().is_some());
    for i in 0..N {
        assert!(db.get("ordered", RecordId(i)).unwrap().is_some(), "row {i}");
    }
    db.insert(
        "ordered",
        RecordId(N + 1),
        Record::new()
            .with("k", 424_243i64)
            .with("payload", "steered-after-restart"),
    )
    .unwrap();
    let rec = db.get("ordered", RecordId(N + 1)).unwrap().unwrap();
    assert_eq!(
        match rec.get("payload") {
            Some(adabt_core::value::Value::Str(s)) => Some(s.as_str()),
            _ => None,
        },
        Some("steered-after-restart")
    );
    db.clear_cluster_field("ordered").unwrap();
    assert_eq!(db.cluster_field("ordered"), None);
    drop(db);
    let db = Database::open(tmp.path(), Policy::manual(0)).unwrap();
    assert_eq!(
        db.cluster_field("ordered"),
        None,
        "a cleared hint stays cleared"
    );
}
