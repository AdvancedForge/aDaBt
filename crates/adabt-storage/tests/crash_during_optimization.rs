//! What happens if the process dies part-way through an optimization?
//!
//! Every other crash test in this repo drops the store without checkpointing,
//! which is not a crash: `Drop` still runs and the log is still flushed. These
//! truncate the write-ahead log at arbitrary byte offsets, which is what a
//! process actually killed mid-write leaves behind.
//!
//! The invariant under test is narrow and absolute: **an optimization must
//! never damage data that predates it.** An optimization may fail, may be half
//! applied, may leave work for the next startup to redo — but a record that was
//! durable before the change must still read back exactly, whatever the outcome.

use adabt_core::ids::RecordId;
use adabt_core::policy::Durability;
use adabt_core::record::Record;
use adabt_core::schema::{FieldDef, FieldType, Schema, SchemaMode};
use adabt_core::store::LogicalStore;
use adabt_storage::heap::HeapStore;
use std::path::{Path, PathBuf};

struct Tmp(PathBuf);
impl Tmp {
    fn new(tag: &str) -> Self {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "adabt-optcrash-{tag}-{}-{:?}",
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

const N: u64 = 200;

fn rec(i: u64) -> Record {
    Record::new()
        .with("id", i)
        .with("balance", (i * 37 % 100_000) as i64)
        .with("name", format!("customer-{i}"))
}

fn frozen_schema() -> Schema {
    Schema::new(
        SchemaMode::Fixed,
        vec![
            FieldDef::new("id", FieldType::U64).required(),
            FieldDef::new("balance", FieldType::I64).required(),
            FieldDef::new("name", FieldType::Char(32)).required(),
        ],
    )
    .unwrap()
}

/// Copy a store directory, so one prepared state can be crashed many ways.
///
/// Recursive, because the log is a directory of segments rather than a file.
fn copy_dir(from: &Path, to: &Path) {
    let _ = std::fs::remove_dir_all(to);
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        let dest = to.join(e.file_name());
        if e.file_type().unwrap().is_dir() {
            copy_dir(&e.path(), &dest);
        } else {
            std::fs::copy(e.path(), dest).unwrap();
        }
    }
}

/// The segment being appended to — the only one a crash can tear.
///
/// These tests never write the sixteen mebibytes it takes to rotate, so it is
/// also the only segment there is.
fn active_segment(dir: &Path) -> PathBuf {
    adabt_storage::wal::Wal::active_segment(&HeapStore::wal_path(dir))
        .unwrap()
        .expect("no log segment")
}

fn wal_len(dir: &Path) -> u64 {
    std::fs::metadata(active_segment(dir)).unwrap().len()
}

/// Cut the log at `bytes`, exactly as a process killed mid-append would.
fn truncate_wal(dir: &Path, bytes: u64) {
    let f = std::fs::OpenOptions::new()
        .write(true)
        .open(active_segment(dir))
        .unwrap();
    f.set_len(bytes).unwrap();
    f.sync_all().unwrap();
}

/// Every record that existed before the change must still read back exactly.
///
/// The schema may or may not have been raised — either outcome is acceptable,
/// because a half-finished optimization is allowed to be *unfinished*. What is
/// not acceptable is a record that comes back wrong, comes back missing, or
/// fails to decode.
///
/// Returns the schema mode in effect, so a caller can check that its cut points
/// actually straddle the change rather than all landing on one side of it.
fn assert_no_record_was_damaged(dir: &Path, at: u64) -> SchemaMode {
    let mut h = match HeapStore::open(dir, Durability::Strict, 32) {
        Ok(h) => h,
        Err(e) => panic!("truncating the log at {at} made the store unopenable: {e}"),
    };
    for i in 0..N {
        match h.get("c", RecordId(i)) {
            Ok(Some(got)) => assert_eq!(
                got,
                rec(i),
                "record {i} came back wrong after a crash at log byte {at}"
            ),
            Ok(None) => panic!("record {i} vanished after a crash at log byte {at}"),
            Err(e) => panic!("record {i} failed to decode after a crash at log byte {at}: {e}"),
        }
    }
    assert_eq!(h.count("c").unwrap(), N as usize, "count changed at {at}");
    // No staging area may outlive recovery, under any name.
    assert!(
        h.collection_names() == vec!["c".to_string()],
        "recovery at {at} left {:?} behind",
        h.collection_names()
    );
    h.schema_of("c").unwrap().mode()
}

/// Prepare a store with `N` durable records under a loose schema, checkpointed,
/// and return the log offset at which the optimization begins.
fn prepared(dir: &Path) -> u64 {
    let mut h = HeapStore::open(dir, Durability::Strict, 32).unwrap();
    h.create_collection("c", Schema::dynamic()).unwrap();
    for i in 0..N {
        h.insert("c", RecordId(i), rec(i)).unwrap();
    }
    h.checkpoint().unwrap();
    drop(h);
    wal_len(dir)
}

#[test]
fn a_crash_part_way_through_a_schema_freeze_leaves_every_record_readable() {
    let src = Tmp::new("freeze-src");
    let start = prepared(src.path());

    let mut h = HeapStore::open(src.path(), Durability::Strict, 32).unwrap();
    h.alter_schema("c", frozen_schema()).unwrap();
    drop(h);
    let end = wal_len(src.path());
    assert!(end > start, "the freeze wrote nothing to the log");

    // Twenty cut points spread across the rewrite, plus the two boundaries.
    let work = Tmp::new("freeze-work");
    let mut modes = Vec::new();
    for step in 0..=20u64 {
        let at = start + (end - start) * step / 20;
        copy_dir(src.path(), work.path());
        truncate_wal(work.path(), at);
        modes.push(assert_no_record_was_damaged(work.path(), at));
    }

    // The cut points have to straddle the change, or this test would pass just
    // as happily against an implementation that never froze anything.
    assert_eq!(
        modes.first(),
        Some(&SchemaMode::Dynamic),
        "cutting the log before the migration still froze the collection"
    );
    assert_eq!(
        modes.last(),
        Some(&SchemaMode::Fixed),
        "an untruncated log did not complete the freeze"
    );
    // And it is one flip, not a gradual slide: every prefix is either wholly
    // before the adoption or wholly after it.
    let flips = modes.windows(2).filter(|w| w[0] != w[1]).count();
    assert_eq!(flips, 1, "the schema changed in stages: {modes:?}");
}

#[test]
fn a_freeze_that_never_commits_leaves_no_staging_collection_and_no_leaked_pages() {
    // The staging copy roughly doubles the collection's footprint while it
    // exists. If recovery kept it, a crash would permanently cost the space of
    // a migration that never happened.
    let t = Tmp::new("freeze-abandoned");
    let _ = prepared(t.path());

    let settled = {
        let mut h = HeapStore::open(t.path(), Durability::Strict, 32).unwrap();
        h.checkpoint().unwrap();
        h.stored_bytes().unwrap()
    };

    let mut h = HeapStore::open(t.path(), Durability::Strict, 32).unwrap();
    h.alter_schema("c", frozen_schema()).unwrap();
    drop(h);
    let end = wal_len(t.path());

    // Cut just short of the adoption entry: the staging copy is fully built and
    // fully durable, and entirely worthless.
    truncate_wal(t.path(), end - 1);
    let mut h = HeapStore::open(t.path(), Durability::Strict, 32).unwrap();
    assert_eq!(h.collection_names(), vec!["c".to_string()]);
    assert_eq!(h.schema_of("c").unwrap().mode(), SchemaMode::Dynamic);
    assert_eq!(h.count("c").unwrap(), N as usize);
    assert_eq!(
        h.stored_bytes().unwrap(),
        settled,
        "the abandoned staging copy is still occupying space"
    );
}

#[test]
fn a_crash_part_way_through_recompression_leaves_every_record_readable() {
    let src = Tmp::new("recompress-src");
    let start = prepared(src.path());

    let mut h = HeapStore::open(src.path(), Durability::Strict, 32).unwrap();
    h.set_compression(true);
    h.recompress_all().unwrap();
    drop(h);
    let end = wal_len(src.path());
    assert!(end > start, "recompression wrote nothing to the log");

    let work = Tmp::new("recompress-work");
    for step in 0..=20u64 {
        let at = start + (end - start) * step / 20;
        copy_dir(src.path(), work.path());
        truncate_wal(work.path(), at);
        assert_no_record_was_damaged(work.path(), at);
    }
}

#[test]
fn a_crash_part_way_through_index_creation_leaves_every_record_readable() {
    let src = Tmp::new("index-src");
    let start = prepared(src.path());

    let mut h = HeapStore::open(src.path(), Durability::Strict, 32).unwrap();
    h.record_index("c", "balance", "btree").unwrap();
    drop(h);
    let end = wal_len(src.path());

    let work = Tmp::new("index-work");
    for step in 0..=4u64 {
        let at = start + (end - start) * step / 4;
        copy_dir(src.path(), work.path());
        truncate_wal(work.path(), at);
        assert_no_record_was_damaged(work.path(), at);
    }
}
