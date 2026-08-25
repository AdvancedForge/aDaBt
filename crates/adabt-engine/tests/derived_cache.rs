//! Persisting derived representations across a restart.
//!
//! Indexes are rebuildable from the primary, which is why they can be dropped
//! and recreated at will — and why, until now, every restart rebuilt them by
//! decoding every record in the heap. The cache removes that cost when it can.
//!
//! The tests here are almost all about the cases where it *cannot*. A cache that
//! is merely fast is worth little; a cache that is fast and can be wrong is
//! worth less than nothing, because the wrongness shows up as missing rows in
//! query results rather than as an error. So the load-bearing assertions are the
//! ones where the cache is stale, damaged or absent and the database is expected
//! to notice and rebuild.

use adabt_core::ids::RecordId;
use adabt_core::index_kind::IndexKind;
use adabt_core::policy::Policy;
use adabt_core::record::Record;
use adabt_core::schema::{FieldDef, FieldType, Schema, SchemaMode};
use adabt_core::store::LogicalStore;
use adabt_engine::Database;
use adabt_ir::plan::{LogicalOp, LogicalPlan};
use adabt_ir::{CmpOp, Expr};
use std::path::{Path, PathBuf};

struct Tmp(PathBuf);
impl Tmp {
    fn new(tag: &str) -> Self {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "adabt-cache-{tag}-{}-{:?}",
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
const N: u64 = 2_000;

fn schema() -> Schema {
    Schema::new(
        SchemaMode::Fixed,
        vec![
            FieldDef::new("id", FieldType::U64).required(),
            FieldDef::new("country", FieldType::Char(8)),
            FieldDef::new("age", FieldType::I64),
        ],
    )
    .unwrap()
}

fn rec(i: u64) -> Record {
    Record::new()
        .with("id", i)
        .with("country", COUNTRIES[(i % 4) as usize])
        .with("age", (i % 70) as i64)
}

/// A database with two indexes of different kinds, checkpointed.
fn prepared(dir: &Path) -> Database {
    let mut db = Database::open(dir, Policy::manual(0)).unwrap();
    db.create_collection("users", schema()).unwrap();
    for i in 0..N {
        db.insert("users", RecordId(i), rec(i)).unwrap();
    }
    db.create_index("users", "country", IndexKind::Hash)
        .unwrap();
    db.create_index("users", "age", IndexKind::BTree).unwrap();
    db.checkpoint().unwrap();
    db
}

fn queries() -> Vec<LogicalPlan> {
    let mut v: Vec<LogicalPlan> = COUNTRIES
        .iter()
        .map(|c| LogicalPlan::new(LogicalOp::scan("users").filter(Expr::eq("country", *c))))
        .collect();
    for bound in [10i64, 40, 69] {
        v.push(LogicalPlan::new(
            LogicalOp::scan("users").filter(Expr::cmp("age", CmpOp::Gt, bound)),
        ));
        v.push(LogicalPlan::new(
            LogicalOp::scan("users").filter(Expr::cmp("age", CmpOp::Lt, bound)),
        ));
    }
    v
}

/// Every query's answer, sorted, as a fingerprint.
fn answers(db: &mut Database) -> Vec<Vec<(RecordId, Record)>> {
    queries()
        .iter()
        .map(|q| {
            let mut rows = db.query(q).unwrap();
            rows.sort_by_key(|(id, _)| id.0);
            rows
        })
        .collect()
}

fn cache_path(dir: &Path) -> PathBuf {
    adabt_storage::derived::path(dir)
}

#[test]
fn a_restored_index_answers_exactly_as_a_rebuilt_one() {
    // The only claim that matters. Everything else about the cache is a
    // performance argument; this is the correctness one.
    let t = Tmp::new("identical");
    let mut db = prepared(t.path());
    let expected = answers(&mut db);
    drop(db);
    assert!(cache_path(t.path()).exists(), "no cache was written");

    let mut restored = Database::open(t.path(), Policy::manual(0)).unwrap();
    assert_eq!(restored.index_specs().len(), 2);
    assert_eq!(answers(&mut restored), expected);
    drop(restored);

    // And with the cache removed, so the same database rebuilds from the heap.
    std::fs::remove_file(cache_path(t.path())).unwrap();
    let mut rebuilt = Database::open(t.path(), Policy::manual(0)).unwrap();
    assert_eq!(rebuilt.index_specs().len(), 2);
    assert_eq!(
        answers(&mut rebuilt),
        expected,
        "restoring and rebuilding disagree"
    );
}

#[test]
fn writes_after_the_checkpoint_invalidate_the_cache() {
    // The cache describes the state it was written from. Anything after that
    // makes it a description of the past, and using it would leave the new
    // records unindexed and invisible to any query the planner routes through
    // an index.
    let t = Tmp::new("stale");
    let mut db = prepared(t.path());
    for i in N..N + 50 {
        db.insert("users", RecordId(i), rec(i)).unwrap();
    }
    drop(db); // no second checkpoint: the cache on disk is now behind

    let mut db = Database::open(t.path(), Policy::manual(0)).unwrap();
    assert_eq!(db.count("users").unwrap(), (N + 50) as usize);
    let rows = db
        .query(&LogicalPlan::new(
            LogicalOp::scan("users").filter(Expr::eq("country", COUNTRIES[0])),
        ))
        .unwrap();
    assert_eq!(
        rows.len(),
        ((N + 50) / 4) as usize + 1,
        "records written after the checkpoint are missing from the index"
    );
}

#[test]
fn a_damaged_cache_costs_a_rebuild_and_nothing_else() {
    let t = Tmp::new("damaged");
    let mut db = prepared(t.path());
    let expected = answers(&mut db);
    drop(db);

    let good = std::fs::read(cache_path(t.path())).unwrap();
    // A flip in the header, one in the middle, one in the checksum itself.
    for at in [4, good.len() / 2, good.len() - 3] {
        let mut damaged = good.clone();
        damaged[at] ^= 0xff;
        std::fs::write(cache_path(t.path()), &damaged).unwrap();
        let mut db = Database::open(t.path(), Policy::manual(0)).unwrap();
        assert_eq!(db.index_specs().len(), 2, "damage at {at} lost an index");
        assert_eq!(answers(&mut db), expected, "damage at {at} changed answers");
    }
}

#[test]
fn a_cache_from_a_different_database_is_not_used() {
    // The stamp exists for exactly this: two directories whose logs may have
    // reached the same position while holding entirely different data.
    let (a, b) = (Tmp::new("orig-a"), Tmp::new("orig-b"));
    let mut da = prepared(a.path());
    let expected = answers(&mut da);
    drop(da);

    let mut db = Database::open(b.path(), Policy::manual(0)).unwrap();
    db.create_collection("users", schema()).unwrap();
    for i in 0..N {
        // The same shape of data, shifted, so a wrongly-adopted index would
        // return plausible rows rather than obviously broken ones.
        db.insert("users", RecordId(i), rec(i + 1)).unwrap();
    }
    db.create_index("users", "country", IndexKind::Hash)
        .unwrap();
    db.create_index("users", "age", IndexKind::BTree).unwrap();
    db.checkpoint().unwrap();
    drop(db);

    std::fs::copy(cache_path(b.path()), cache_path(a.path())).unwrap();
    let mut da = Database::open(a.path(), Policy::manual(0)).unwrap();
    assert_eq!(
        answers(&mut da),
        expected,
        "an index built from another database's records was adopted"
    );
}

#[test]
fn dropping_an_index_does_not_resurrect_it_from_the_cache() {
    let t = Tmp::new("dropped");
    let mut db = prepared(t.path());
    assert!(db.drop_index("users", "country", IndexKind::Hash));
    db.checkpoint().unwrap();
    drop(db);

    let db = Database::open(t.path(), Policy::manual(0)).unwrap();
    let fields: Vec<String> = db.index_specs().iter().map(|s| s.field.clone()).collect();
    assert_eq!(fields, vec!["age".to_string()], "{fields:?}");
}

#[test]
fn an_index_added_after_the_last_checkpoint_is_still_there_after_a_restart() {
    // The definition is in the log even though the contents never reached the
    // cache, so the index must come back — rebuilt, which is the slow path
    // working as designed rather than a failure.
    let t = Tmp::new("uncached-index");
    let mut db = prepared(t.path());
    db.create_index("users", "id", IndexKind::BTree).unwrap();
    let expected = answers(&mut db);
    drop(db);

    let mut db = Database::open(t.path(), Policy::manual(0)).unwrap();
    assert_eq!(db.index_specs().len(), 3);
    assert_eq!(answers(&mut db), expected);
}

// -- the instrument ------------------------------------------------------
//
// A local copy of the counting allocator from `allocations.rs` — each test
// file is its own binary, so a global allocator cannot be shared, and this
// test needs it: page reads turned out to be identical between the two
// paths (records live decoded in RAM after recovery, so rebuilding pays no
// pool reads at all), while the decode-and-clone work of a rebuild shows up
// plainly in allocation counts.
static ALLOCS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static COUNTING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

struct CountingAlloc;

unsafe impl std::alloc::GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, l: std::alloc::Layout) -> *mut u8 {
        if COUNTING.load(std::sync::atomic::Ordering::Relaxed) {
            ALLOCS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        unsafe { std::alloc::System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: std::alloc::Layout) {
        unsafe { std::alloc::System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: std::alloc::Layout, n: usize) -> *mut u8 {
        if COUNTING.load(std::sync::atomic::Ordering::Relaxed) {
            ALLOCS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        unsafe { std::alloc::System.realloc(p, l, n) }
    }
}

#[global_allocator]
static A: CountingAlloc = CountingAlloc;

fn allocations_during<T>(f: impl FnOnce() -> T) -> u64 {
    use std::sync::atomic::Ordering;
    ALLOCS.store(0, Ordering::Relaxed);
    COUNTING.store(true, Ordering::Relaxed);
    let out = f();
    COUNTING.store(false, Ordering::Relaxed);
    drop(out);
    ALLOCS.load(Ordering::Relaxed)
}

#[test]
fn restoring_costs_far_fewer_allocations_than_rebuilding() {
    // Not a benchmark, a floor — and deliberately NOT a timing assertion.
    // An earlier version of this test compared elapsed wall-clock time, and
    // flaked on a loaded machine: 69.5ms against 67.4ms is noise, not a
    // result. Page reads were tried next and measure nothing here: records
    // live decoded in RAM after recovery, so both opens touch the same
    // pages and the counters come back equal. What rebuilding actually
    // spends is per-record decode-and-clone work, which shows up as
    // allocations — deterministic on every machine.
    //
    // Restoring reads keys back from the cache file; rebuilding decodes
    // every record in the heap twice over (once per index). If restoring
    // ever costs as much as rebuilding, the cache is doing nothing and
    // should be deleted rather than maintained.
    const BIG: u64 = 60_000;
    let t = Tmp::new("allocs-big");
    {
        let mut db = Database::open(t.path(), Policy::manual(0)).unwrap();
        db.create_collection("users", schema()).unwrap();
        for i in 0..BIG {
            db.insert("users", RecordId(i), rec(i)).unwrap();
        }
        db.create_index("users", "country", IndexKind::Hash)
            .unwrap();
        db.create_index("users", "age", IndexKind::BTree).unwrap();
        db.checkpoint().unwrap();
    }
    let cached = std::fs::read(cache_path(t.path())).unwrap();

    let open_counting = |dir: &Path| {
        allocations_during(|| {
            let mut db = Database::open(dir, Policy::manual(0)).unwrap();
            assert_eq!(db.count("users").unwrap(), BIG.try_into().unwrap());
        })
    };

    std::fs::write(cache_path(t.path()), &cached).unwrap();
    let restored = open_counting(t.path());
    std::fs::remove_file(cache_path(t.path())).unwrap();
    let rebuilt = open_counting(t.path());

    // The first calibration run measured 492k against 731k on this fixture.
    // The bulk of an open is shared — WAL recovery, heap load — so the cache
    // cannot and should not claim a multiple; what it claims is its own
    // margin, and the floor is set at one avoided allocation per record
    // (the measured delta was roughly four, across two indexes).
    assert!(
        restored < rebuilt && rebuilt - restored >= BIG,
        "restoring cost {restored} allocations against rebuilding's {rebuilt}; \
         the saved margin fell below one allocation per record ({BIG})"
    );
}
