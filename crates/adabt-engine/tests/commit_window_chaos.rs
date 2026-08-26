//! The commit-window chaos matrix.
//!
//! `Database::commit` applies a transaction's writes one frame at a time,
//! in sorted-key order — there is no atomic commit record yet (the module
//! docs say plainly what providing one would take). So the honest contract
//! for a crash *inside* the window is not all-or-nothing; it is this:
//!
//! 1. **What survives is exactly a prefix of the sorted write-set** — the
//!    first k writes, never an interleaving, never a gap. This is what
//!    "visited in sorted order, for reproducibility" buys under a kill.
//! 2. **Every survivor is exact**: a half-updated field or a row wearing
//!    another row's values fails here before anything else runs.
//! 3. **The recovered database is consistent and idempotent** — same three
//!    contracts as `crash_consistency.rs`, demanded of every truncation
//!    point through the commit's frames.
//!
//! The test also asserts the matrix *bites*: at least one cut must land
//! inside the window (some but not all of the transaction present), or it
//! was measuring truncation of nothing.

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
            "adabt-cwc-{tag}-{}-{:?}",
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

/// The transaction under the knife, in the order commit actually applies
/// them — sorted by real write key. The delete's key is ("b", RecordId(3)),
/// which sorts *before* every b2xx insert; its entry here carries the marker
/// id 205 purely so the fixture can name it.
const TXN_KEYS: [(&str, u64); 12] = [
    ("a", 100),
    ("a", 101),
    ("a", 102),
    ("a", 103),
    ("a", 104),
    ("a", 105),
    ("b", 205), // the delete of b/3, sorting as ("b", 3)
    ("b", 200),
    ("b", 201),
    ("b", 202),
    ("b", 203),
    ("b", 204),
];

/// Pre-existing state the transaction updates and deletes: ids 0..10 in both
/// collections, values derived from the id so any survivor checks exactly.
fn seeded(dir: &Path) -> Database {
    let mut db = Database::open(dir, Policy::manual(4)).unwrap();
    db.create_collection("a", Schema::dynamic()).unwrap();
    db.create_collection("b", Schema::dynamic()).unwrap();
    for i in 0..10u64 {
        for c in ["a", "b"] {
            db.insert(c, RecordId(i), old_row(i)).unwrap();
        }
    }
    db.checkpoint().unwrap();

    let mut txn = db.begin();
    for &(c, i) in &TXN_KEYS {
        if i == 205 {
            // The delete: last in sort order, so the deepest cut that still
            // counts as "inside" removes it.
            txn.delete(&mut db, c, RecordId(3)).unwrap();
        } else {
            txn.insert(&mut db, c, RecordId(i), new_row(i)).unwrap();
        }
    }
    db.commit(txn).unwrap();
    db
}

fn old_row(i: u64) -> Record {
    Record::new().with("v", -(i as i64)).with("gen", "old")
}

/// What write `(c, i)` puts: collection-tagged so a row landing in the wrong
/// collection cannot pass.
fn new_row(i: u64) -> Record {
    Record::new().with("v", (i as i64) * 7).with("gen", "txn")
}

/// The database state implied by the first `applied` writes of the sorted
/// key list having survived.
fn expected(applied: usize) -> Vec<(&'static str, u64, &'static str)> {
    let mut rows = Vec::new();
    let deleted_3 = TXN_KEYS
        .iter()
        .take(applied)
        .any(|&(c, i)| c == "b" && i == 205);

    for c in ["a", "b"] {
        for i in 0..10u64 {
            if c == "b" && i == 3 && deleted_3 {
                continue; // deleted inside the window
            }
            rows.push((c, i, "old"));
        }
        // The transaction's own inserts, present iff their write survived.
        for &(kc, ki) in TXN_KEYS.iter().take(applied) {
            if kc == c && ki != 205 {
                rows.push((c, ki, "txn"));
            }
        }
    }
    rows.sort();
    rows
}

fn observed(db: &mut Database) -> Vec<(&'static str, u64, &'static str)> {
    let mut rows = Vec::new();
    for c in ["a", "b"] {
        for (id, rec) in db.scan(c).unwrap() {
            let gen = match rec.get("gen").and_then(|v| match v {
                adabt_core::value::Value::Str(s) => Some(s.as_str()),
                _ => None,
            }) {
                Some(g) => g.to_string(),
                None => panic!("row {c}/{id} has no generation marker"),
            };
            rows.push((if c == "a" { "a" } else { "b" }, id.0, leak(gen)));
        }
    }
    rows.sort();
    rows
}

fn leak(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

#[test]
fn every_cut_through_the_commit_window_recovers_a_prefix_never_a_mixture() {
    let src = Tmp::new("src");
    seeded(src.path());
    assert_eq!(
        observed(&mut Database::open(src.path(), Policy::manual(4)).unwrap()),
        expected(TXN_KEYS.len())
    );

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
    let active = segs.last().expect("post-commit segment").clone();
    let len = std::fs::metadata(&active).unwrap().len();
    assert!(len > 0);

    const POINTS: u64 = 17;
    let offsets: Vec<u64> = (1..=POINTS).map(|k| len * k / POINTS).collect();

    let mut bit_inside_window = false;
    let mut opened_cleanly = 0;
    let mut refused = 0;

    for (round, &off) in offsets.iter().enumerate() {
        let victim = Tmp::new(format!("cut{round}").as_str());
        copy_dir(src.path(), victim.path());

        let f = std::fs::OpenOptions::new()
            .write(true)
            .open(HeapStore::wal_path(victim.path()).join(active.file_name().unwrap()))
            .unwrap();
        f.set_len(off).unwrap();
        drop(f);

        match Database::open(victim.path(), Policy::manual(4)) {
            Ok(mut db) => {
                opened_cleanly += 1;

                // Contract 1 + 2: exactly a prefix of the write-set, exact rows.
                let got = observed(&mut db);
                let applied = expected_applied_of(&got);
                assert_eq!(
                    got,
                    expected(applied),
                    "cut at {off}: survivors are not the first {applied} writes"
                );
                if applied > 0 && applied < TXN_KEYS.len() {
                    bit_inside_window = true;
                }

                // Contract 3: consistency, then idempotence.
                let report = db.verify().unwrap();
                assert!(
                    report.problems.is_empty(),
                    "cut at {off}: {:?}",
                    report.problems.join("\n")
                );
                drop(db);
                let mut db2 = Database::open(victim.path(), Policy::manual(4)).unwrap();
                assert_eq!(
                    observed(&mut db2),
                    got,
                    "cut at {off}: reopening changed the outcome"
                );
                let r2 = db2.verify().unwrap();
                assert!(r2.problems.is_empty(), "cut at {off}: {:?}", r2.problems);
            }
            Err(_) => refused += 1,
        }
    }

    assert!(
        opened_cleanly >= refused.max(1),
        "recovery refused more than it recovered: {opened_cleanly} ok, {refused} refused"
    );
    assert!(
        bit_inside_window,
        "no cut landed inside the commit window; the matrix proved nothing about partial commits"
    );
}

/// How many of the transaction's writes are present in an observed state.
/// The prefix property itself is asserted by comparing against `expected`;
/// this only locates where the cut landed.
fn expected_applied_of(got: &[(&'static str, u64, &'static str)]) -> usize {
    let mut applied = 0usize;
    for (idx, &(c, i)) in TXN_KEYS.iter().enumerate() {
        let present = got
            .iter()
            .any(|&(gc, gi, gen)| gc == c && gi == i && gen == "txn");
        let deleted = c == "b" && i == 205;
        if present || (deleted && !got.iter().any(|&(gc, gi, _)| gc == "b" && gi == 3)) {
            applied = idx + 1;
        }
    }
    applied
}
