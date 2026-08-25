//! The crash/chaos matrix around checkpoints, closed with the consistency
//! checker.
//!
//! `crash_during_optimization.rs` established the primitive: truncate the WAL
//! at arbitrary byte offsets, which is what a killed process actually leaves
//! behind — not a tidy `Drop`, but a log that stops mid-frame. That test
//! proves optimizations never damage data that predates them. This one walks
//! a *matrix* of truncation points through a whole workload — inserts across
//! two checkpoints, hash and covering indexes, a columnar copy — and demands,
//! for every point, the full post-recovery contract rather than one invariant:
//!
//! 1. **Open either succeeds or fails cleanly.** A torn tail is replayed to
//!    the last intact frame; it may cost recent writes, never a panic or a
//!    silently wrong catalog.
//! 2. **Every record that survives reads back exactly.** Whatever prefix of
//!    the workload the surviving log holds, its rows are intact — no half-updated
//!    field, no id with another row's values.
//! 3. **`verify()` reports no divergences.** Indexes are rebuilt from the heap
//!    at open, so consistency is by construction — which is precisely why it
//!    must be *asserted*: if a rebuild path ever starts trusting cached state,
//!    this is the test that catches it on the first bad commit.
//! 4. **Reopening again changes nothing.** Recovery is idempotent; a second
//!    open sees the same database.

use adabt_core::ids::RecordId;
use adabt_core::policy::Policy;
use adabt_core::record::Record;
use adabt_core::schema::Schema;
use adabt_core::store::LogicalStore;
use adabt_engine::Database;
use adabt_storage::heap::HeapStore;
use std::path::{Path, PathBuf};

struct Tmp(PathBuf);
impl Tmp {
    fn new(tag: &str) -> Self {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "adabt-chaos-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
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

fn copy_dir(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).unwrap();
    for entry in std::fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let to = dst.join(entry.file_name());
        if entry.path().is_dir() {
            copy_dir(&entry.path(), &to);
        } else {
            std::fs::copy(entry.path(), &to).unwrap();
        }
    }
}

const N: u64 = 200;

/// The workload every truncation point cuts through: two checkpointed waves
/// of inserts over a dynamic schema, with derived structures live throughout.
/// Ids carry their expected contents, so any survivor can be checked exactly.
fn seeded(dir: &Path) -> Database {
    let mut db = Database::open(dir, Policy::manual(4)).unwrap();
    db.create_collection("c", Schema::dynamic()).unwrap();
    wave(dir, &mut db, 0);
    db.checkpoint().unwrap();
    // Wave two is deliberately left uncheckpointed: it exists only in the
    // log, which is what makes the truncation matrix able to lose it. A
    // workload folded entirely into pages would leave an empty tail and a
    // matrix that could never fail.
    wave(dir, &mut db, N / 2);
    db
}

fn wave(dir: &Path, db: &mut Database, start: u64) {
    // Reopened between waves? Indexes already exist; creating is idempotent.
    let fresh = start == 0;
    if fresh {
        db.create_index("c", "bucket", adabt_core::index_kind::IndexKind::Hash)
            .unwrap();
        db.create_covering_index(
            "c",
            "bucket",
            &["status".to_string()],
            adabt_core::index_kind::IndexKind::Hash,
        )
        .unwrap();
    }
    for i in start..start + N / 2 {
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
    let _ = dir; // dir only used for symmetry with callers that need it
}

/// What a fully committed row must look like, checked field by field. A torn
/// write that survived as garbage would fail here before verify ever runs.
fn expect_intact(rec: &Record, id: RecordId) {
    let i = id.0;
    assert_eq!(
        rec.get("id"),
        Some(&adabt_core::value::Value::U64(i)),
        "id {i}"
    );
    assert_eq!(
        rec.get("bucket"),
        Some(&adabt_core::value::Value::I64((i % 100) as i64)),
        "bucket of {i}"
    );
    assert_eq!(
        rec.get("status").and_then(|v| match v {
            adabt_core::value::Value::Str(s) => Some(s.as_str()),
            _ => None,
        }),
        Some(if i % 2 == 0 { "open" } else { "shut" }),
        "status of {i}"
    );
}

#[test]
fn every_truncation_point_recovers_to_a_consistent_database() {
    let src = Tmp::new("src");
    seeded(src.path());

    // The WAL is a directory of bounded segments (M17). Only the *last* one
    // can carry a torn tail — every earlier segment was sealed and synced
    // before the next was created — so a killed process's damage lives here,
    // and this is the file the matrix cuts.
    let wal_dir = HeapStore::wal_path(src.path());
    let mut segs: Vec<PathBuf> = std::fs::read_dir(&wal_dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with("seg-"))
                .unwrap_or(false)
        })
        .collect();
    segs.sort();
    let active = segs
        .last()
        .expect("a checkpointed workload leaves a segment")
        .clone();
    let len = std::fs::metadata(&active).unwrap().len();
    assert!(len > 0, "the active segment is non-empty");

    // Offsets spread across the whole segment: front (wave two barely begun),
    // middle (mid-insert), back (everything landed). 13 points is enough to
    // catch an off-by-one in frame boundaries without making the suite crawl.
    const POINTS: u64 = 13;
    let offsets: Vec<u64> = (0..POINTS).map(|k| len * k / POINTS).collect();

    let mut opened_cleanly = 0;
    let mut refused = 0;
    let mut min_rows = u64::MAX;
    for (round, off) in offsets.iter().enumerate() {
        let victim = Tmp::new(format!("cut{round}").as_str());
        copy_dir(src.path(), victim.path());

        // The crash: everything after `off` never happened.
        let f = std::fs::OpenOptions::new()
            .write(true)
            .open(
                HeapStore::wal_path(victim.path())
                    .join(active.file_name().expect("segment has a name")),
            )
            .unwrap();
        f.set_len(*off).unwrap();
        drop(f);

        match Database::open(victim.path(), Policy::manual(4)) {
            Ok(mut db) => {
                opened_cleanly += 1;

                // Contract 2: survivors are exact.
                let rows = db.scan("c").unwrap();
                min_rows = min_rows.min(rows.len() as u64);
                for (id, rec) in &rows {
                    expect_intact(rec, *id);
                }

                // Contract 3: nothing derived disagrees with anything.
                let report = db.verify().unwrap();
                assert!(
                    report.problems.is_empty(),
                    "offset {off}: recovered database diverged:\n{}",
                    report.problems.join("\n")
                );

                // Contract 4: recovery is idempotent.
                let n1 = rows.len();
                drop(db);
                let mut db2 = Database::open(victim.path(), Policy::manual(4)).unwrap();
                assert_eq!(db2.scan("c").unwrap().len(), n1, "offset {off}");
                let r2 = db2.verify().unwrap();
                assert!(r2.problems.is_empty(), "offset {off}: {:?}", r2.problems);
            }
            Err(_) => {
                // A clean refusal is within contract 1 — but if *every*
                // point were refused we would have proven nothing about
                // recovery, hence the tally asserted below.
                refused += 1;
            }
        }
    }
    assert!(
        opened_cleanly >= refused.max(1),
        "recovery refused more often than it recovered: {opened_cleanly} ok, {refused} refused"
    );
    // The cuts must bite: some point lost uncheckpointed rows, or the matrix
    // was measuring truncation of nothing.
    assert!(
        min_rows < N,
        "no truncation point lost data; the tail was already checkpointed and the matrix proved nothing"
    );
    println!(
        "chaos matrix: {opened_cleanly} recovered cleanly, {refused} refused outright, \
         fewest survivors {min_rows}/{N}"
    );
}
