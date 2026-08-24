//! Opening a database without reading every page.
//!
//! The directory is derived: it can always be rebuilt by scanning the heap, and
//! before this cache existed it always was. The cache replaces that scan when it
//! can prove it describes the same checkpoint.
//!
//! Which makes the interesting tests the ones where it cannot prove it. A stale
//! directory does not produce an error or a crash — it produces a database that
//! looks fine and has lost records, or points at the wrong bytes. So most of
//! what follows arranges for the cache to be wrong and checks that it is
//! ignored.

use adabt_core::ids::RecordId;
use adabt_core::policy::Durability;
use adabt_core::record::Record;
use adabt_core::schema::Schema;
use adabt_core::store::LogicalStore;
use adabt_storage::heap::HeapStore;
use std::path::{Path, PathBuf};

struct Tmp(PathBuf);
impl Tmp {
    fn new(tag: &str) -> Self {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "adabt-dir-{tag}-{}-{:?}",
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

const N: u64 = 3_000;

fn rec(i: u64) -> Record {
    Record::new()
        .with("id", i)
        .with("name", format!("customer-{i}"))
        .with("balance", (i * 37 % 100_000) as i64)
}

fn cache(dir: &Path) -> PathBuf {
    adabt_storage::directory::path(dir)
}

fn filled(dir: &Path) -> HeapStore {
    let mut h = HeapStore::open(dir, Durability::Strict, 64).unwrap();
    h.create_collection("c", Schema::dynamic()).unwrap();
    for i in 0..N {
        h.insert("c", RecordId(i), rec(i)).unwrap();
    }
    h.checkpoint().unwrap();
    h
}

/// Everything the store holds, as a fingerprint.
fn contents(h: &mut HeapStore) -> Vec<(RecordId, Record)> {
    h.scan("c").unwrap()
}

#[test]
fn a_loaded_directory_and_a_scanned_one_describe_the_same_database() {
    let t = Tmp::new("same");
    let expected = {
        let mut h = filled(t.path());
        contents(&mut h)
    };
    assert!(
        cache(t.path()).exists(),
        "the checkpoint wrote no directory"
    );

    let mut loaded = HeapStore::open(t.path(), Durability::Strict, 64).unwrap();
    assert_eq!(contents(&mut loaded), expected);
    drop(loaded);

    std::fs::remove_file(cache(t.path())).unwrap();
    let mut scanned = HeapStore::open(t.path(), Durability::Strict, 64).unwrap();
    assert_eq!(
        contents(&mut scanned),
        expected,
        "loading and scanning disagree"
    );
}

#[test]
fn writes_after_the_checkpoint_are_recovered_on_top_of_the_cache() {
    // The cache describes the checkpoint, not the present. Everything after it
    // is in the log, and replay has to carry the directory the rest of the way.
    let t = Tmp::new("after");
    {
        let mut h = filled(t.path());
        for i in N..N + 400 {
            h.insert("c", RecordId(i), rec(i)).unwrap();
        }
        for i in 0..200 {
            h.delete("c", RecordId(i)).unwrap();
        }
        for i in 200..400 {
            h.update("c", RecordId(i), rec(i).with("balance", -1i64))
                .unwrap();
        }
        // No second checkpoint: the cache on disk is now behind by 800 writes.
    }
    let mut h = HeapStore::open(t.path(), Durability::Strict, 64).unwrap();
    assert_eq!(h.count("c").unwrap(), (N + 400 - 200) as usize);
    for i in 0..200 {
        assert_eq!(
            h.get("c", RecordId(i)).unwrap(),
            None,
            "deleted {i} is back"
        );
    }
    for i in 200..400u64 {
        assert_eq!(
            h.get("c", RecordId(i)).unwrap(),
            Some(rec(i).with("balance", -1i64)),
            "update to {i} was lost"
        );
    }
    for i in N..N + 400 {
        assert_eq!(h.get("c", RecordId(i)).unwrap(), Some(rec(i)), "{i}");
    }
}

#[test]
fn a_damaged_directory_costs_a_scan_and_nothing_else() {
    let t = Tmp::new("damaged");
    let expected = {
        let mut h = filled(t.path());
        contents(&mut h)
    };
    let good = std::fs::read(cache(t.path())).unwrap();
    for at in [4, 20, good.len() / 2, good.len() - 3] {
        let mut damaged = good.clone();
        damaged[at] ^= 0xff;
        std::fs::write(cache(t.path()), &damaged).unwrap();
        let mut h = HeapStore::open(t.path(), Durability::Strict, 64).unwrap();
        assert_eq!(
            contents(&mut h),
            expected,
            "damage at {at} changed the data"
        );
    }
}

#[test]
fn a_directory_from_another_database_is_not_adopted() {
    // Two stores built by identical operations reach identical log positions and
    // identical heap sizes. Only the database identity separates them, and if it
    // did not, one would silently adopt the other's map of where records live.
    let (a, b) = (Tmp::new("ident-a"), Tmp::new("ident-b"));
    let expected = {
        let mut h = filled(a.path());
        contents(&mut h)
    };
    {
        let mut h = HeapStore::open(b.path(), Durability::Strict, 64).unwrap();
        h.create_collection("c", Schema::dynamic()).unwrap();
        for i in 0..N {
            // Same shape, different values, written in a different order — so a
            // wrongly adopted directory points at real records that are not the
            // ones asked for.
            h.insert("c", RecordId(N - 1 - i), rec(i + 7)).unwrap();
        }
        h.checkpoint().unwrap();
    }
    std::fs::copy(cache(b.path()), cache(a.path())).unwrap();
    let mut h = HeapStore::open(a.path(), Durability::Strict, 64).unwrap();
    assert_eq!(
        contents(&mut h),
        expected,
        "another database's directory was adopted"
    );
}

#[test]
fn a_truncated_log_is_never_recovered_against_a_newer_directory() {
    // The dangerous combination: the directory describes a checkpoint that the
    // log no longer records. Cutting the log back must send recovery to the
    // scan, because the cache is now describing a future that did not happen.
    let t = Tmp::new("truncated-log");
    {
        let mut h = filled(t.path());
        for i in N..N + 300 {
            h.insert("c", RecordId(i), rec(i)).unwrap();
        }
        h.checkpoint().unwrap();
    }
    let wal = adabt_storage::wal::Wal::active_segment(&HeapStore::wal_path(t.path()))
        .unwrap()
        .expect("no log segment");
    let full = std::fs::metadata(&wal).unwrap().len();
    for cut in [full / 4, full / 2, full * 3 / 4, full - 1] {
        let f = std::fs::OpenOptions::new().write(true).open(&wal).unwrap();
        f.set_len(cut).unwrap();
        f.sync_all().unwrap();
        let mut h = match HeapStore::open(t.path(), Durability::Strict, 64) {
            Ok(h) => h,
            Err(e) => panic!("cutting the log to {cut} made the store unopenable: {e}"),
        };
        // Whatever survived must be internally consistent: every record the
        // store believes exists must decode and read back.
        let rows = h.scan("c").unwrap();
        assert_eq!(rows.len(), h.count("c").unwrap());
        for (id, got) in rows {
            assert_eq!(
                h.get("c", id).unwrap(),
                Some(got),
                "record {id} disagrees with itself after a cut at {cut}"
            );
        }
    }
}

#[test]
fn an_empty_database_round_trips_through_the_cache() {
    let t = Tmp::new("empty");
    {
        let mut h = HeapStore::open(t.path(), Durability::Strict, 8).unwrap();
        h.create_collection("c", Schema::dynamic()).unwrap();
        h.checkpoint().unwrap();
    }
    let mut h = HeapStore::open(t.path(), Durability::Strict, 8).unwrap();
    assert_eq!(h.count("c").unwrap(), 0);
    assert_eq!(h.collection_names(), vec!["c".to_string()]);
}

#[test]
fn a_collection_dropped_after_the_checkpoint_stays_dropped() {
    // The cache still lists it. The log says it is gone, and the log wins.
    let t = Tmp::new("dropped");
    {
        let mut h = filled(t.path());
        h.drop_collection("c").unwrap();
    }
    let h = HeapStore::open(t.path(), Durability::Strict, 64).unwrap();
    assert!(
        h.collection_names().is_empty(),
        "{:?}",
        h.collection_names()
    );
}

#[test]
fn loading_a_directory_reads_no_pages_at_all() {
    // The cache exists to avoid reading every page, so that is what is measured
    // — not elapsed time. A wall-clock assertion here was flaky under parallel
    // test load and, worse, was measuring a gap that narrowed the moment the
    // catalog removed the log walk that used to sit alongside the page scan. The
    // page count is the thing the cache actually changes, and it changes it to
    // zero.
    let t = Tmp::new("reads");
    drop(filled(t.path()));
    let saved = std::fs::read(cache(t.path())).unwrap();

    let reads_on_open = |dir: &Path| {
        let h = HeapStore::open(dir, Durability::Strict, 64).unwrap();
        h.pool_stats().reads
    };

    std::fs::write(cache(t.path()), &saved).unwrap();
    let loaded = reads_on_open(t.path());
    std::fs::remove_file(cache(t.path())).unwrap();
    let scanned = reads_on_open(t.path());

    assert_eq!(
        loaded, 0,
        "recovery read {loaded} pages despite a valid directory"
    );
    assert!(
        scanned > 10,
        "the scan path read only {scanned} pages, so this proves nothing"
    );
}

#[test]
fn a_migrated_collection_survives_a_restart_through_the_cache() {
    // The regression the first version of this cache caused, and the reason the
    // directory is keyed by collection id rather than by name.
    //
    // Freezing a schema hands one collection's records to another: the new
    // encoding is built beside the old one under a private name and adopted in a
    // single log entry, after which the *name* `c` refers to a collection with a
    // different *id*. A record's slot prefix carries the id, so that is what a
    // page scan recovers and what the cache has to reproduce.
    //
    // Keyed by name, the cache put every record back under the pre-migration id.
    // Recovery then re-ran the adoption, which frees the old collection's pages
    // — by then the pages holding all the data — and the collection came back
    // empty, with no error anywhere.
    use adabt_core::schema::{FieldDef, FieldType, SchemaMode};

    let t = Tmp::new("migrated");
    let frozen = Schema::new(
        SchemaMode::Fixed,
        vec![
            FieldDef::new("id", FieldType::U64).required(),
            FieldDef::new("name", FieldType::Char(32)).required(),
            FieldDef::new("balance", FieldType::I64).required(),
        ],
    )
    .unwrap();
    {
        let mut h = filled(t.path());
        h.alter_schema("c", frozen.clone()).unwrap();
        h.checkpoint().unwrap();
    }
    assert!(cache(t.path()).exists());

    let mut h = HeapStore::open(t.path(), Durability::Strict, 64).unwrap();
    assert_eq!(
        h.count("c").unwrap(),
        N as usize,
        "the migrated collection came back empty"
    );
    assert_eq!(h.schema_of("c").unwrap().mode(), SchemaMode::Fixed);
    for i in 0..N {
        assert_eq!(h.get("c", RecordId(i)).unwrap(), Some(rec(i)), "record {i}");
    }
    // And the staging collection is not visible under any name.
    assert_eq!(h.collection_names(), vec!["c".to_string()]);
}
