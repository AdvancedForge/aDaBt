//! Compression at the heap level: does it save space, and does it still return
//! exactly the same records?

use adabt_core::ids::RecordId;
use adabt_core::policy::Durability;
use adabt_core::record::Record;
use adabt_core::schema::{FieldDef, FieldType, Schema, SchemaMode};
use adabt_core::store::LogicalStore;
use adabt_storage::heap::HeapStore;
use adabt_testkit::differential::{run, seeds};
use adabt_testkit::generator::GenConfig;
use adabt_testkit::reference::ReferenceStore;
use std::path::{Path, PathBuf};

struct Tmp(PathBuf);
impl Tmp {
    fn new(tag: &str) -> Self {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "adabt-compress-{tag}-{}-{:?}",
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

/// Wide fixed-width fields, so records carry the zero padding that a real
/// fixed-layout schema produces.
fn padded_schema() -> Schema {
    Schema::new(
        SchemaMode::Fixed,
        vec![
            FieldDef::new("id", FieldType::U64).required(),
            FieldDef::new("balance", FieldType::I64).required(),
            FieldDef::new("name", FieldType::Char(64)),
            FieldDef::new("notes", FieldType::Char(128)),
        ],
    )
    .unwrap()
}

fn rec(i: u64) -> Record {
    Record::new()
        .with("id", i)
        .with("balance", (i * 37 % 100_000) as i64)
        .with("name", format!("customer-{i}"))
        .with("notes", format!("acct {i}"))
}

fn fill(h: &mut HeapStore, n: u64) {
    h.create_collection("c", padded_schema()).unwrap();
    for i in 0..n {
        h.insert("c", RecordId(i), rec(i)).unwrap();
    }
}

#[test]
fn compression_reduces_stored_bytes_substantially() {
    let (a, b) = (Tmp::new("off"), Tmp::new("on"));
    let n = 2_000u64;

    let mut plain = HeapStore::open(a.path(), Durability::Relaxed, 256).unwrap();
    fill(&mut plain, n);
    let raw = plain.stored_bytes().unwrap();

    let mut packed = HeapStore::open(b.path(), Durability::Relaxed, 256).unwrap();
    packed.set_compression(true);
    fill(&mut packed, n);
    let compressed = packed.stored_bytes().unwrap();

    assert!(
        compressed * 2 < raw,
        "expected at least a halving: {compressed} against {raw}"
    );
}

#[test]
fn compression_uses_fewer_pages_for_the_same_data() {
    // The point of compressing records rather than pages: smaller records mean
    // more per page, so the saving reaches I/O and buffer residency too.
    let (a, b) = (Tmp::new("pages-off"), Tmp::new("pages-on"));
    let mut plain = HeapStore::open(a.path(), Durability::Relaxed, 256).unwrap();
    fill(&mut plain, 3_000);

    let mut packed = HeapStore::open(b.path(), Durability::Relaxed, 256).unwrap();
    packed.set_compression(true);
    fill(&mut packed, 3_000);

    assert!(
        packed.page_count() < plain.page_count(),
        "compressed heap used {} pages, uncompressed used {}",
        packed.page_count(),
        plain.page_count()
    );
}

#[test]
fn every_record_comes_back_identical() {
    let t = Tmp::new("identity");
    let mut h = HeapStore::open(t.path(), Durability::Relaxed, 64).unwrap();
    h.set_compression(true);
    fill(&mut h, 1_500);
    for i in 0..1_500u64 {
        assert_eq!(h.get("c", RecordId(i)).unwrap(), Some(rec(i)), "record {i}");
    }
}

#[test]
fn compressed_and_raw_records_coexist() {
    // Enabling compression must not require a migration: existing records keep
    // their encoding, and both must read back correctly.
    let t = Tmp::new("mixed");
    let mut h = HeapStore::open(t.path(), Durability::Relaxed, 64).unwrap();
    h.create_collection("c", padded_schema()).unwrap();
    for i in 0..500u64 {
        h.insert("c", RecordId(i), rec(i)).unwrap();
    }
    h.set_compression(true);
    for i in 500..1_000u64 {
        h.insert("c", RecordId(i), rec(i)).unwrap();
    }
    h.set_compression(false);
    for i in 1_000..1_500u64 {
        h.insert("c", RecordId(i), rec(i)).unwrap();
    }
    for i in 0..1_500u64 {
        assert_eq!(h.get("c", RecordId(i)).unwrap(), Some(rec(i)), "record {i}");
    }
}

#[test]
fn recompressing_an_existing_store_saves_space_and_keeps_every_record() {
    let t = Tmp::new("recompress");
    let mut h = HeapStore::open(t.path(), Durability::Relaxed, 256).unwrap();
    fill(&mut h, 2_000);
    let before = h.stored_bytes().unwrap();

    h.set_compression(true);
    let delta = h.recompress_all().unwrap();

    assert!(delta < 0, "recompression did not save space: {delta:+}");
    let after = h.stored_bytes().unwrap();
    assert_eq!(after as i64, before as i64 + delta);
    assert!(after * 2 < before, "{after} against {before}");

    for i in 0..2_000u64 {
        assert_eq!(h.get("c", RecordId(i)).unwrap(), Some(rec(i)), "record {i}");
    }
}

#[test]
fn compressed_records_survive_a_crash_and_reopen() {
    let t = Tmp::new("crash");
    {
        let mut h = HeapStore::open(t.path(), Durability::Strict, 32).unwrap();
        h.set_compression(true);
        fill(&mut h, 800);
        // No checkpoint, no clean shutdown.
    }
    let mut h = HeapStore::open(t.path(), Durability::Strict, 32).unwrap();
    assert_eq!(h.count("c").unwrap(), 800);
    for i in 0..800u64 {
        assert_eq!(h.get("c", RecordId(i)).unwrap(), Some(rec(i)), "record {i}");
    }
}

#[test]
fn recompression_is_replayed_correctly_after_a_crash() {
    // recompress_all goes through the WAL, so a crash part-way through must
    // replay to a consistent state rather than losing records.
    let t = Tmp::new("recompress-crash");
    {
        let mut h = HeapStore::open(t.path(), Durability::Strict, 32).unwrap();
        fill(&mut h, 600);
        h.set_compression(true);
        h.recompress_all().unwrap();
    }
    let mut h = HeapStore::open(t.path(), Durability::Strict, 32).unwrap();
    assert_eq!(h.count("c").unwrap(), 600);
    for i in 0..600u64 {
        assert_eq!(h.get("c", RecordId(i)).unwrap(), Some(rec(i)), "record {i}");
    }
}

#[test]
fn a_compressed_store_matches_the_reference_model() {
    let cfg = GenConfig::with_collections(vec![
        ("fixed".into(), padded_schema()),
        (
            "strict".into(),
            Schema::new(
                SchemaMode::Strict,
                vec![
                    FieldDef::new("id", FieldType::U64).required(),
                    FieldDef::new(
                        "bio",
                        FieldType::Str {
                            max_len: Some(2000),
                        },
                    ),
                ],
            )
            .unwrap(),
        ),
        ("dynamic".into(), Schema::dynamic()),
    ]);
    for (i, seed) in seeds(0xC0FFEE, 6).into_iter().enumerate() {
        let t = Tmp::new(&format!("diff{i}"));
        let mut a = ReferenceStore::new();
        let mut b = HeapStore::open(t.path(), Durability::Relaxed, 32).unwrap();
        b.set_compression(true);
        for (name, schema) in &cfg.collections {
            a.create_collection(name, schema.clone()).unwrap();
            b.create_collection(name, schema.clone()).unwrap();
        }
        run(&mut a, &mut b, "reference", "compressed", &cfg, seed, 600)
            .unwrap_or_else(|d| panic!("{d}"));
    }
}

#[test]
fn compression_costs_cpu_which_is_the_trade_being_made() {
    // Not a performance assertion — a demonstration that the trade is real and
    // measurable in the direction claimed, so the axis effects are honest.
    let (a, b) = (Tmp::new("cpu-off"), Tmp::new("cpu-on"));
    let n = 4_000u64;

    let t0 = std::time::Instant::now();
    let mut plain = HeapStore::open(a.path(), Durability::Relaxed, 512).unwrap();
    fill(&mut plain, n);
    let plain_secs = t0.elapsed().as_secs_f64();

    let t1 = std::time::Instant::now();
    let mut packed = HeapStore::open(b.path(), Durability::Relaxed, 512).unwrap();
    packed.set_compression(true);
    fill(&mut packed, n);
    let packed_secs = t1.elapsed().as_secs_f64();

    let saved = plain.stored_bytes().unwrap() as f64 - packed.stored_bytes().unwrap() as f64;
    assert!(
        saved > 0.0,
        "no space was saved, so there is no trade to make"
    );
    // Writing is not expected to be faster; the storage saving is the point.
    println!(
        "compression: {:.0} bytes saved, write time {plain_secs:.3}s -> {packed_secs:.3}s",
        saved
    );
}
