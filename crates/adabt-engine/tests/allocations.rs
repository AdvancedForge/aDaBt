//! How many heap allocations a query costs, per row.
//!
//! Every performance regression this project has actually shipped was invisible
//! to the test suite, because the suite compares *answers* and these bugs did
//! not change answers. A scan read the collection twice; a query cloned its
//! entire result set for a cache that was switched off; a sort duplicated its
//! output on the way out. All three returned exactly the right rows.
//!
//! Wall-clock tests cannot close that gap — they are noisy on a loaded machine
//! and they encode a threshold that means something different on every box.
//! Allocation counts can: they are deterministic, they are identical on any
//! machine, and doing work twice shows up as doing it twice.
//!
//! So this installs a counting allocator and asserts a *budget per row*. The
//! budgets are deliberately loose. They are not targets and beating them is not
//! the point — they are tripwires set above where the path sits today, so that
//! a change which doubles the work fails here instead of being discovered in a
//! benchmark six milestones later.

use adabt_core::ids::RecordId;
use adabt_core::policy::Policy;
use adabt_core::record::Record;
use adabt_core::schema::{FieldDef, FieldType, Schema, SchemaMode};
use adabt_core::store::LogicalStore;
use adabt_engine::Database;
use adabt_ir::plan::{LogicalOp, LogicalPlan, SortKey};
use std::alloc::{GlobalAlloc, Layout, System};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;

// -- the instrument ------------------------------------------------------

static ALLOCS: AtomicU64 = AtomicU64::new(0);
static COUNTING: AtomicBool = AtomicBool::new(false);

struct Counting;

// Counting only, never allocating: the counter is a static atomic, so the
// allocator cannot recurse into itself.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
        }
        unsafe { System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
        }
        unsafe { System.realloc(p, l, new) }
    }
}

#[global_allocator]
static A: Counting = Counting;

/// The counter is global to the whole test binary, and tests in one binary run
/// on several threads. Serializing only the measurement is not enough: another
/// test allocating on another thread lands in the count. So every test here
/// holds this for its entire body, which makes the file effectively
/// single-threaded — the price of a global instrument.
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

fn exclusive() -> std::sync::MutexGuard<'static, ()> {
    ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner())
}

/// Allocations made while `f` runs. The caller must already hold `exclusive()`.
fn allocations_during<T>(f: impl FnOnce() -> T) -> u64 {
    ALLOCS.store(0, Ordering::Relaxed);
    COUNTING.store(true, Ordering::Relaxed);
    let out = f();
    COUNTING.store(false, Ordering::Relaxed);
    drop(out);
    ALLOCS.load(Ordering::Relaxed)
}

// -- fixtures ------------------------------------------------------------

struct Tmp(PathBuf);
impl Tmp {
    fn new(tag: &str) -> Self {
        let mut p = std::env::temp_dir();
        p.push(format!("adabt-allocations-{tag}-{}", std::process::id()));
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

const N: u64 = 5_000;

fn seeded(dir: &Path) -> Database {
    let mut db = Database::open(dir, Policy::manual(0)).unwrap();
    db.create_collection("c", Schema::dynamic()).unwrap();
    for i in 0..N {
        db.insert(
            "c",
            RecordId(i),
            Record::new()
                .with("id", i)
                .with("bucket", (i % 100) as i64)
                .with("status", if i % 2 == 0 { "open" } else { "shut" }),
        )
        .unwrap();
    }
    db
}

fn scan() -> LogicalPlan {
    LogicalPlan::new(LogicalOp::Scan {
        collection: "c".into(),
    })
}

// -- the budgets ---------------------------------------------------------

/// A three-field record with one string field costs two allocations to deliver:
/// one for the record's own field vector, one for the string. That is the
/// floor, and both schema modes sit on it today.
///
/// The budget is set at twice the floor. Not a target — a tripwire. It is
/// there to catch the multiples this project has actually shipped: a second
/// decode of the collection, a clone of the whole result set for a disabled
/// cache, a copy of the output on the way out of a sort. Each of those doubled
/// the count, so a 2x ceiling catches every one of them while leaving room for
/// an honest change that costs one more allocation per row.
const SCAN_BUDGET: u64 = 4;

/// The columnar path's floor is one allocation per row — the record's own
/// vector. Field names are interned in the store (`ColumnStore::arcs`), so
/// handing them to each row is a refcount bump; integer cells are copied, not
/// allocated; only string cells allocate, and this fixture has one per row.
///
/// Before interning, `project` built every name with `to_string()` plus the
/// `Arc` conversion — two extra allocations per CELL per row, so six per row
/// on this fixture instead of two. That is what this budget exists to keep
/// from coming back.
const COLUMNAR_BUDGET: u64 = 3;

#[test]
fn a_columnar_scan_allocates_a_bounded_amount_per_row() {
    let _exclusive = exclusive();
    let dir = Tmp::new("columnar");
    seeded(dir.path());
    // Two fields projected out of three, both non-string where possible: id
    // and bucket are integers, status stays behind. The plan must actually be
    // served columnarly or the test proves nothing — asserted, not assumed,
    // exactly like the column-store tests do it.
    let plan = LogicalPlan::new(
        LogicalOp::Scan {
            collection: "c".into(),
        }
        .project(vec!["id".into(), "bucket".into()]),
    );

    // A level whose preset includes the column store, then prove it engaged.
    let mut tuned = Database::open(dir.path(), Policy::manual(4)).unwrap();
    let explain = tuned.plan(&plan).explain();
    assert!(
        explain.contains("ColumnScan"),
        "the plan was not served columnarly:\n{explain}"
    );
    tuned.query(&plan).unwrap(); // warm

    let allocs = allocations_during(|| tuned.query(&plan).unwrap());
    let per_row = allocs / N;

    println!(
        "columnar scan: {per_row} allocations/row ({allocs} for {N}), budget {COLUMNAR_BUDGET}"
    );
    assert!(
        per_row <= COLUMNAR_BUDGET,
        "a columnar scan cost {per_row} allocations per row (budget {COLUMNAR_BUDGET}); \
         {allocs} for {N} rows"
    );
}

#[test]
fn a_scan_allocates_a_bounded_amount_per_row() {
    let _exclusive = exclusive();
    let dir = Tmp::new("scan");
    let mut db = seeded(dir.path());
    let plan = scan();

    // Warm: the first call installs plan-cache entries and grows buffers that
    // later calls reuse. Measuring it would measure the warm-up.
    db.query(&plan).unwrap();

    let allocs = allocations_during(|| db.query(&plan).unwrap());
    let per_row = allocs / N;

    // Reported, not just asserted: a budget test is only useful if the
    // headroom is visible. `cargo test -- --nocapture` shows it.
    println!("scan: {per_row} allocations/row ({allocs} for {N}), budget {SCAN_BUDGET}");
    assert!(
        per_row <= SCAN_BUDGET,
        "a scan cost {per_row} allocations per row (budget {SCAN_BUDGET}); \
         {allocs} for {N} rows"
    );
}

/// Sorting is where the executor used to copy its whole output: it collected
/// every row, sorted them, then rebuilt batches with `chunks(..).to_vec()`,
/// cloning every record it already owned. A sort should cost a scan plus the
/// sort — not a scan plus a second copy of the scan.
#[test]
fn sorting_does_not_cost_a_second_copy_of_the_output() {
    let _exclusive = exclusive();
    let dir = Tmp::new("sort");
    let mut db = seeded(dir.path());
    let plan = scan();
    let sorted = LogicalPlan::new(LogicalOp::scan("c").sort(vec![SortKey {
        field: "bucket".into(),
        descending: false,
    }]));

    db.query(&plan).unwrap();
    db.query(&sorted).unwrap();

    let plain = allocations_during(|| db.query(&plan).unwrap());
    let with_sort = allocations_during(|| db.query(&sorted).unwrap());

    // A full extra copy of the output would be at least one allocation per
    // record for the record itself and one per string field — comfortably more
    // than the whole scan costs. Half a scan is a generous ceiling that still
    // catches it.

    println!("sort: {with_sort} allocations against {plain} unsorted");
    assert!(
        with_sort < plain + plain / 2,
        "sorting cost {with_sort} allocations against {plain} for the same scan \
         unsorted — the difference is a duplicated result set, not a sort"
    );
}

/// The result cache is disabled at level 0, and a disabled cache must cost
/// nothing. It used to cost a clone of every row returned, because the caller
/// built the clone before the cache could decline it.
#[test]
fn a_disabled_result_cache_costs_nothing_per_row() {
    let _exclusive = exclusive();
    let dir = Tmp::new("nocache");
    let mut db = seeded(dir.path());
    assert_eq!(
        db.result_cache_stats().hits + db.result_cache_stats().misses,
        0,
        "this test is about the cache being off; it must not have been probed yet"
    );

    let plan = scan();
    db.query(&plan).unwrap();
    let allocs = allocations_during(|| db.query(&plan).unwrap());

    // With the cache off, a scan's cost is decode plus delivery. A clone of
    // every row on top of that would roughly double it, so the same budget the
    // scan test uses is exactly the right ceiling here.
    let per_row = allocs / N;
    assert!(
        per_row <= SCAN_BUDGET,
        "a scan with the result cache disabled cost {per_row} allocations per \
         row (budget {SCAN_BUDGET}) — the cache is being paid for while off"
    );
}

/// Enumerating ids reads no records, so it should allocate a bounded amount in
/// total rather than per row. This is the allocation-side statement of the
/// same fact `scan_cost.rs` asserts in page reads.
#[test]
fn enumerating_ids_does_not_allocate_per_record() {
    let _exclusive = exclusive();
    let dir = Tmp::new("ids");
    let mut db = seeded(dir.path());
    db.ids("c").unwrap();

    let allocs = allocations_during(|| db.ids("c").unwrap());

    // One `Vec` that grows by doubling: logarithmic in N, nowhere near linear.

    println!("ids: {allocs} allocations for {N} ids");
    assert!(
        allocs < 64,
        "enumerating {N} ids cost {allocs} allocations; it should cost a \
         handful — one growing Vec and nothing else"
    );
}

/// The same budget under a declared schema.
///
/// The two decode paths are genuinely different code: a `Dynamic` record
/// carries its own field names and a `Strict` one takes them from the schema.
/// Measuring only one is how a change that helps one and hurts the other gets
/// shipped — which is exactly what happened while writing this: sharing schema
/// names made `Strict` cheaper and `Dynamic` *more expensive*, and only the
/// `Dynamic` number was being watched.
#[test]
fn a_scan_under_a_declared_schema_has_the_same_budget() {
    let _exclusive = exclusive();
    let dir = Tmp::new("strict");

    let schema = Schema::new(
        SchemaMode::Strict,
        vec![
            FieldDef::new("id", FieldType::U64).required(),
            FieldDef::new("bucket", FieldType::I64),
            FieldDef::new("status", FieldType::Str { max_len: Some(8) }),
        ],
    )
    .unwrap();

    let mut db = Database::open(dir.path(), Policy::manual(0)).unwrap();
    db.create_collection("c", schema).unwrap();
    for i in 0..N {
        db.insert(
            "c",
            RecordId(i),
            Record::new()
                .with("id", i)
                .with("bucket", (i % 100) as i64)
                .with("status", if i % 2 == 0 { "open" } else { "shut" }),
        )
        .unwrap();
    }

    let plan = scan();
    db.query(&plan).unwrap();
    let allocs = allocations_during(|| db.query(&plan).unwrap());
    let per_row = allocs / N;

    println!("strict scan: {per_row} allocations/row ({allocs} for {N})");
    assert!(
        per_row <= SCAN_BUDGET,
        "a scan under a declared schema cost {per_row} allocations per row \
         (budget {SCAN_BUDGET})"
    );
}
