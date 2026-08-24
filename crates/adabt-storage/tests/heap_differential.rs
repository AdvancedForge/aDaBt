//! The heap store, checked against the reference model, and its crash behaviour.

use adabt_core::ids::RecordId;
use adabt_core::policy::Durability;
use adabt_core::record::Record;
use adabt_core::schema::{FieldDef, FieldType, Schema, SchemaMode};
use adabt_core::store::LogicalStore;
use adabt_storage::heap::HeapStore;
use adabt_testkit::differential::{compare, run, seeds};
use adabt_testkit::generator::{GenConfig, Generator};
use adabt_testkit::ops::{apply, Op};
use adabt_testkit::reference::ReferenceStore;
use std::path::{Path, PathBuf};

struct Tmp(PathBuf);
impl Tmp {
    fn new(tag: &str) -> Self {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "adabt-heap-{tag}-{}-{:?}",
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

fn config() -> GenConfig {
    GenConfig::with_collections(vec![
        (
            "fixed".into(),
            Schema::new(
                SchemaMode::Fixed,
                vec![
                    FieldDef::new("id", FieldType::U64).required(),
                    FieldDef::new("balance", FieldType::I64),
                    FieldDef::new("active", FieldType::Bool),
                    FieldDef::new("name", FieldType::Char(24)),
                ],
            )
            .unwrap(),
        ),
        (
            "strict".into(),
            Schema::new(
                SchemaMode::Strict,
                vec![
                    FieldDef::new("id", FieldType::U64).required(),
                    // Long enough to force records across pages and to make
                    // updates change size, which exercises relocation.
                    FieldDef::new(
                        "bio",
                        FieldType::Str {
                            max_len: Some(2000),
                        },
                    ),
                    FieldDef::new("score", FieldType::F64),
                ],
            )
            .unwrap(),
        ),
        (
            "declared".into(),
            Schema::new(
                SchemaMode::Declared,
                vec![FieldDef::new("id", FieldType::U64).required()],
            )
            .unwrap(),
        ),
        ("dynamic".into(), Schema::dynamic()),
    ])
}

fn seed_stores(cfg: &GenConfig, dir: &Path) -> (ReferenceStore, HeapStore) {
    let mut a = ReferenceStore::new();
    let mut b = HeapStore::open(dir, Durability::Strict, 16).unwrap();
    for (name, schema) in &cfg.collections {
        a.create_collection(name, schema.clone()).unwrap();
        b.create_collection(name, schema.clone()).unwrap();
    }
    (a, b)
}

#[test]
fn heap_matches_the_reference_model() {
    let cfg = config();
    for (i, seed) in seeds(0x4EA9, 12).into_iter().enumerate() {
        let t = Tmp::new(&format!("diff{i}"));
        let (mut a, mut b) = seed_stores(&cfg, t.path());
        run(&mut a, &mut b, "reference", "heap", &cfg, seed, 500).unwrap_or_else(|d| panic!("{d}"));
    }
}

#[test]
fn heap_matches_the_reference_model_over_a_long_run() {
    let t = Tmp::new("long");
    let cfg = config();
    let (mut a, mut b) = seed_stores(&cfg, t.path());
    run(&mut a, &mut b, "reference", "heap", &cfg, 0x10_1010, 8_000)
        .unwrap_or_else(|d| panic!("{d}"));
}

/// Reopen the store between every batch of operations, so the run is really a
/// long sequence of crash-and-recover cycles.
#[test]
fn state_survives_repeated_reopening() {
    let t = Tmp::new("reopen");
    let cfg = config();
    let mut reference = ReferenceStore::new();
    {
        let mut h = HeapStore::open(t.path(), Durability::Strict, 8).unwrap();
        for (name, schema) in &cfg.collections {
            reference.create_collection(name, schema.clone()).unwrap();
            h.create_collection(name, schema.clone()).unwrap();
        }
    }
    let mut gen = Generator::new(&cfg, 0xBEEF);
    for round in 0..25 {
        let ops = gen.take(60);
        let mut h = HeapStore::open(t.path(), Durability::Strict, 8).unwrap();
        compare(&mut reference, &mut h, "reference", "heap", &ops, 0xBEEF)
            .unwrap_or_else(|d| panic!("round {round}: {d}"));
    }
}

/// Kill the store without a checkpoint. Everything acknowledged under strict
/// durability must come back.
#[test]
fn an_uncheckpointed_crash_loses_nothing_under_strict_durability() {
    let t = Tmp::new("crash");
    let schema = Schema::new(
        SchemaMode::Strict,
        vec![
            FieldDef::new("id", FieldType::U64).required(),
            FieldDef::new("payload", FieldType::Str { max_len: Some(500) }),
        ],
    )
    .unwrap();

    let n = 400u64;
    {
        let mut h = HeapStore::open(t.path(), Durability::Strict, 4).unwrap();
        h.create_collection("c", schema.clone()).unwrap();
        for i in 0..n {
            let rec = Record::new()
                .with("id", i)
                .with("payload", "p".repeat((i % 400) as usize));
            h.insert("c", RecordId(i), rec).unwrap();
        }
        // No checkpoint, no clean shutdown: just drop it, as a kill -9 would.
    }

    let mut h = HeapStore::open(t.path(), Durability::Strict, 4).unwrap();
    assert_eq!(
        h.count("c").unwrap(),
        n as usize,
        "records lost across a crash"
    );
    for i in 0..n {
        let got = h.get("c", RecordId(i)).unwrap().expect("record missing");
        assert_eq!(got.get("id"), Some(&adabt_core::value::Value::U64(i)));
        assert_eq!(
            got.get("payload"),
            Some(&adabt_core::value::Value::Str(
                "p".repeat((i % 400) as usize)
            ))
        );
    }
}

#[test]
fn a_checkpoint_lets_replay_start_later_without_changing_the_result() {
    let t = Tmp::new("checkpoint");
    let schema = Schema::new(
        SchemaMode::Fixed,
        vec![
            FieldDef::new("id", FieldType::U64).required(),
            FieldDef::new("v", FieldType::I64).required(),
        ],
    )
    .unwrap();
    {
        let mut h = HeapStore::open(t.path(), Durability::Strict, 8).unwrap();
        h.create_collection("c", schema).unwrap();
        for i in 0..200u64 {
            h.insert(
                "c",
                RecordId(i),
                Record::new().with("id", i).with("v", i as i64),
            )
            .unwrap();
        }
        h.checkpoint().unwrap();
        // Changes after the checkpoint must still be replayed.
        for i in 200..300u64 {
            h.insert(
                "c",
                RecordId(i),
                Record::new().with("id", i).with("v", i as i64),
            )
            .unwrap();
        }
        for i in 0..50u64 {
            h.delete("c", RecordId(i)).unwrap();
        }
    }
    let mut h = HeapStore::open(t.path(), Durability::Strict, 8).unwrap();
    assert_eq!(h.count("c").unwrap(), 250);
    assert!(
        h.get("c", RecordId(0)).unwrap().is_none(),
        "deleted record returned"
    );
    assert!(
        h.get("c", RecordId(299)).unwrap().is_some(),
        "post-checkpoint insert lost"
    );
}

#[test]
fn updates_that_outgrow_their_slot_relocate_correctly() {
    let t = Tmp::new("relocate");
    let schema = Schema::new(
        SchemaMode::Strict,
        vec![
            FieldDef::new("id", FieldType::U64).required(),
            FieldDef::new(
                "b",
                FieldType::Str {
                    max_len: Some(4000),
                },
            ),
        ],
    )
    .unwrap();
    let mut h = HeapStore::open(t.path(), Durability::Strict, 8).unwrap();
    h.create_collection("c", schema).unwrap();
    for i in 0..60u64 {
        h.insert("c", RecordId(i), Record::new().with("id", i).with("b", "x"))
            .unwrap();
    }
    // Grow every record far past its original footprint, forcing relocation.
    for i in 0..60u64 {
        h.update(
            "c",
            RecordId(i),
            Record::new().with("id", i).with("b", "y".repeat(3000)),
        )
        .unwrap();
    }
    for i in 0..60u64 {
        let got = h
            .get("c", RecordId(i))
            .unwrap()
            .expect("record vanished on relocation");
        assert_eq!(
            got.get("b"),
            Some(&adabt_core::value::Value::Str("y".repeat(3000)))
        );
    }
    assert_eq!(h.count("c").unwrap(), 60);
}

#[test]
fn space_from_deleted_records_is_reused() {
    let t = Tmp::new("reuse");
    let schema = Schema::new(
        SchemaMode::Strict,
        vec![
            FieldDef::new("id", FieldType::U64).required(),
            FieldDef::new(
                "b",
                FieldType::Str {
                    max_len: Some(3000),
                },
            ),
        ],
    )
    .unwrap();
    let mut h = HeapStore::open(t.path(), Durability::Strict, 32).unwrap();
    h.create_collection("c", schema).unwrap();
    let big = "z".repeat(2000);
    for i in 0..200u64 {
        h.insert(
            "c",
            RecordId(i),
            Record::new().with("id", i).with("b", &big[..]),
        )
        .unwrap();
    }
    let pages_after_fill = h.page_count();
    for i in 0..200u64 {
        h.delete("c", RecordId(i)).unwrap();
    }
    for i in 1000..1200u64 {
        h.insert(
            "c",
            RecordId(i),
            Record::new().with("id", i).with("b", &big[..]),
        )
        .unwrap();
    }
    assert!(
        h.page_count() <= pages_after_fill * 2,
        "freed space was not reused: {} pages grew to {}",
        pages_after_fill,
        h.page_count()
    );
    assert_eq!(h.count("c").unwrap(), 200);
}

#[test]
fn relaxed_durability_avoids_fsync_while_strict_does_not() {
    let cfg_schema = || {
        Schema::new(
            SchemaMode::Fixed,
            vec![FieldDef::new("id", FieldType::U64).required()],
        )
        .unwrap()
    };
    let write = |durability, dir: &Path| {
        let mut h = HeapStore::open(dir, durability, 8).unwrap();
        h.create_collection("c", cfg_schema()).unwrap();
        for i in 0..300u64 {
            h.insert("c", RecordId(i), Record::new().with("id", i))
                .unwrap();
        }
        h.wal_syncs()
    };
    let ts = Tmp::new("strict-sync");
    let tr = Tmp::new("relaxed-sync");
    let strict = write(Durability::Strict, ts.path());
    let relaxed = write(Durability::Relaxed, tr.path());
    assert!(
        strict >= 300,
        "strict durability must fsync each write, got {strict}"
    );
    assert_eq!(relaxed, 0, "relaxed durability must not fsync");
}

#[test]
fn dropping_a_collection_frees_its_records_and_survives_reopen() {
    let t = Tmp::new("drop");
    let schema = Schema::new(
        SchemaMode::Fixed,
        vec![FieldDef::new("id", FieldType::U64).required()],
    )
    .unwrap();
    {
        let mut h = HeapStore::open(t.path(), Durability::Strict, 8).unwrap();
        h.create_collection("a", schema.clone()).unwrap();
        h.create_collection("b", schema.clone()).unwrap();
        for i in 0..100u64 {
            h.insert("a", RecordId(i), Record::new().with("id", i))
                .unwrap();
            h.insert("b", RecordId(i), Record::new().with("id", i))
                .unwrap();
        }
        h.drop_collection("a").unwrap();
    }
    let mut h = HeapStore::open(t.path(), Durability::Strict, 8).unwrap();
    assert_eq!(h.collection_names(), vec!["b".to_string()]);
    assert!(
        h.get("a", RecordId(0)).is_err(),
        "dropped collection came back"
    );
    assert_eq!(
        h.count("b").unwrap(),
        100,
        "the surviving collection was damaged"
    );
}

#[test]
fn a_small_buffer_pool_gives_the_same_answers_as_a_large_one() {
    let cfg = config();
    let (t_small, t_big) = (Tmp::new("pool-small"), Tmp::new("pool-big"));
    let ops = Generator::new(&cfg, 0x9999).take(1_500);

    let mut results = Vec::new();
    for (dir, pages) in [(t_small.path(), 2usize), (t_big.path(), 256)] {
        let mut h = HeapStore::open(dir, Durability::Strict, pages).unwrap();
        for (name, schema) in &cfg.collections {
            h.create_collection(name, schema.clone()).unwrap();
        }
        let outcomes: Vec<_> = ops.iter().map(|op| apply(&mut h, op)).collect();
        results.push(outcomes);
    }
    assert_eq!(
        results[0], results[1],
        "buffer pool size changed the answers, which means eviction loses data"
    );
}

#[test]
fn a_record_too_large_for_a_page_is_rejected_cleanly() {
    let t = Tmp::new("toobig");
    let schema = Schema::new(
        SchemaMode::Strict,
        vec![
            FieldDef::new("id", FieldType::U64).required(),
            FieldDef::new("b", FieldType::Bytes { max_len: None }),
        ],
    )
    .unwrap();
    let mut h = HeapStore::open(t.path(), Durability::Strict, 8).unwrap();
    h.create_collection("c", schema).unwrap();
    let huge = Record::new().with("id", 1u64).with("b", vec![0u8; 32_000]);
    assert!(h.insert("c", RecordId(1), huge).is_err());
    // The store stays usable afterwards.
    h.insert(
        "c",
        RecordId(2),
        Record::new().with("id", 2u64).with("b", vec![1u8; 10]),
    )
    .unwrap();
    assert_eq!(h.count("c").unwrap(), 1);
}

#[test]
fn scan_returns_record_id_order_regardless_of_physical_placement() {
    let t = Tmp::new("scanorder");
    let schema = Schema::new(
        SchemaMode::Fixed,
        vec![FieldDef::new("id", FieldType::U64).required()],
    )
    .unwrap();
    let mut h = HeapStore::open(t.path(), Durability::Strict, 4).unwrap();
    h.create_collection("c", schema).unwrap();
    // Insert in a deliberately scrambled order.
    for i in [50u64, 3, 99, 1, 42, 7, 88, 0] {
        h.insert("c", RecordId(i), Record::new().with("id", i))
            .unwrap();
    }
    let ids: Vec<u64> = h.scan("c").unwrap().iter().map(|(i, _)| i.0).collect();
    let mut sorted = ids.clone();
    sorted.sort_unstable();
    assert_eq!(ids, sorted, "scan order is part of the logical contract");
}

/// The op sequence is fixed; only the durability setting changes. Weakening
/// durability trades crash-safety for speed and must not change what a running
/// store returns.
#[test]
fn durability_setting_does_not_change_logical_results() {
    let cfg = config();
    let ops: Vec<Op> = Generator::new(&cfg, 0x5AFE).take(1_200);
    let mut results = Vec::new();
    for (i, d) in [
        Durability::Strict,
        Durability::GroupCommit,
        Durability::Relaxed,
    ]
    .into_iter()
    .enumerate()
    {
        let t = Tmp::new(&format!("dur{i}"));
        let mut h = HeapStore::open(t.path(), d, 16).unwrap();
        for (name, schema) in &cfg.collections {
            h.create_collection(name, schema.clone()).unwrap();
        }
        results.push(ops.iter().map(|op| apply(&mut h, op)).collect::<Vec<_>>());
    }
    assert_eq!(results[0], results[1]);
    assert_eq!(results[1], results[2]);
}
