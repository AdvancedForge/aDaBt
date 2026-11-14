//! The commit-window chaos matrix.
//!
//! `Database::commit` logs a transaction's whole write-set under one commit
//! record before any of it reaches a page, and recovery applies a transaction
//! only once it finds that record. So the contract for a crash *inside* the
//! window is all-or-nothing:
//!
//! 1. **Every cut recovers all of the transaction or none of it** — none for
//!    every cut short of the end of the commit record, all for every cut at or
//!    past it. Never a prefix, never a mixture.
//! 2. **Every survivor is exact**: a half-updated field or a row wearing
//!    another row's values fails here before anything else runs.
//! 3. **The recovered database is consistent and idempotent** — same three
//!    contracts as `crash_consistency.rs`, demanded of every cut.
//!
//! A crash image pairs a cut log with the heap file a crash at that point
//! could leave. Before the commit record nothing has touched a page, so that
//! is the heap from before the commit. After it, the pages the transaction
//! dirtied may or may not have reached the file — and the fixture shrinks the
//! buffer pool so the pool is forced to write some of them out mid-commit — so
//! both that heap and the final one must recover the whole transaction.
//!
//! A point-in-time restore is the same question asked deliberately: a target
//! anywhere inside the transaction must not expose part of it either. That
//! test keeps every page of the transaction out of the heap file, because a
//! restore to an earlier point over a heap file that already holds later
//! pages is unsound for any write, transactional or not (`docs/limitations.md`).

use adabt_core::ids::{RecordId, TxnId};
use adabt_core::policy::Policy;
use adabt_core::record::Record;
use adabt_core::schema::Schema;
use adabt_core::store::LogicalStore;
use adabt_engine::Database;
use adabt_storage::heap::{HeapStore, RecoverTarget};
use adabt_storage::wal::{Wal, WalOp};
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

/// The transaction under the knife. The delete's key is ("b", RecordId(3));
/// its entry here carries the marker id 205 purely so the fixture can name it.
const TXN_KEYS: [(&str, u64); 12] = [
    ("a", 100),
    ("a", 101),
    ("a", 102),
    ("a", 103),
    ("a", 104),
    ("a", 105),
    ("b", 205), // the delete of b/3
    ("b", 200),
    ("b", 201),
    ("b", 202),
    ("b", 203),
    ("b", 204),
];

/// Ids 0..10 in both collections, values derived from the id so any survivor
/// checks exactly.
fn seed(db: &mut Database) {
    db.create_collection("a", Schema::dynamic()).unwrap();
    db.create_collection("b", Schema::dynamic()).unwrap();
    for i in 0..10u64 {
        for c in ["a", "b"] {
            db.insert(c, RecordId(i), old_row(i)).unwrap();
        }
    }
    db.checkpoint().unwrap();
}

/// Commit the transaction. With `steal`, under a two-page pool, so that
/// applying it forces the pool to write dirty pages to the heap file
/// mid-commit.
fn commit_txn(db: &mut Database, steal: bool) {
    if steal {
        db.set_pool_capacity(2).unwrap();
    }
    let evicted = db.buffer_stats().evictions;
    let mut txn = db.begin();
    for &(c, i) in &TXN_KEYS {
        if i == 205 {
            txn.delete(db, c, RecordId(3)).unwrap();
        } else {
            txn.insert(db, c, RecordId(i), new_row(i)).unwrap();
        }
    }
    db.commit(txn).unwrap();
    assert_eq!(
        db.buffer_stats().evictions > evicted,
        steal,
        "the fixture must steal pages during the commit exactly when asked to"
    );
}

fn old_row(i: u64) -> Record {
    Record::new().with("v", -(i as i64)).with("gen", "old")
}

/// Padded so the transaction spans several pages.
fn new_row(i: u64) -> Record {
    Record::new()
        .with("v", (i as i64) * 7)
        .with("gen", "txn")
        .with("pad", "p".repeat(1_500))
}

/// The database state with the transaction applied, or not.
fn expected(committed: bool) -> Vec<(&'static str, u64, &'static str)> {
    let mut rows = Vec::new();
    for c in ["a", "b"] {
        for i in 0..10u64 {
            if !(committed && c == "b" && i == 3) {
                rows.push((c, i, "old"));
            }
        }
        if committed {
            for &(kc, ki) in &TXN_KEYS {
                if kc == c && ki != 205 {
                    rows.push((c, ki, "txn"));
                }
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
            let gen = match rec.get("gen") {
                Some(adabt_core::value::Value::Str(s)) => s.clone(),
                _ => panic!("row {c}/{id} has no generation marker"),
            };
            let exact = match gen.as_str() {
                "old" => rec == old_row(id.0),
                "txn" => rec == new_row(id.0),
                other => panic!("row {c}/{id} has generation {other:?}"),
            };
            assert!(exact, "row {c}/{id} is not exactly what was written");
            let c: &'static str = if c == "a" { "a" } else { "b" };
            rows.push((c, id.0, if gen == "old" { "old" } else { "txn" }));
        }
    }
    rows.sort();
    rows
}

/// The log's only segment, so its frames line up with [`Wal::read_all`].
fn only_segment(dir: &Path) -> PathBuf {
    let segs: Vec<PathBuf> = std::fs::read_dir(HeapStore::wal_path(dir))
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("adabt"))
        .collect();
    assert_eq!(segs.len(), 1, "fixture expects a single log segment");
    segs[0].clone()
}

/// `(start, end)` byte offsets of every frame in a segment, after its
/// 40-byte header.
fn frame_spans(segment: &Path) -> Vec<(u64, u64)> {
    let bytes = std::fs::read(segment).unwrap();
    let mut spans = Vec::new();
    let mut pos = 40usize;
    while pos + 8 <= bytes.len() {
        let len = u32::from_le_bytes(bytes[pos..pos + 4].try_into().unwrap()) as usize;
        spans.push((pos as u64, (pos + 8 + len) as u64));
        pos += 8 + len;
    }
    spans
}

/// A crash image: everything from `heap_from` except the log, which is
/// `log_from`'s cut at `at` bytes.
fn crash_image(heap_from: &Path, log_from: &Path, at: u64, tag: &str) -> Tmp {
    let image = Tmp::new(tag);
    copy_dir(heap_from, image.path());
    let wal = HeapStore::wal_path(image.path());
    std::fs::remove_dir_all(&wal).unwrap();
    copy_dir(&HeapStore::wal_path(log_from), &wal);
    let f = std::fs::OpenOptions::new()
        .write(true)
        .open(only_segment(image.path()))
        .unwrap();
    f.set_len(at).unwrap();
    image
}

#[test]
fn every_cut_through_the_commit_window_recovers_all_or_none() {
    let live = Tmp::new("live");
    let pre = Tmp::new("pre");
    let mut db = Database::open(live.path(), Policy::manual(4)).unwrap();
    seed(&mut db);
    copy_dir(live.path(), pre.path());
    commit_txn(&mut db, true);
    drop(db);
    assert_eq!(
        observed(&mut Database::open(live.path(), Policy::manual(4)).unwrap()),
        expected(true)
    );

    let entries = Wal::read_all(&HeapStore::wal_path(live.path())).unwrap();
    let first = entries
        .iter()
        .position(|e| e.txn != TxnId(0))
        .expect("the commit must be logged as a transaction");
    let seg = only_segment(live.path());
    let spans = frame_spans(&seg);
    assert_eq!(spans.len(), entries.len());
    let committed_at = spans.last().unwrap().1;
    assert!(matches!(entries.last().unwrap().op, WalOp::Commit { .. }));

    // Every frame boundary, a cut inside every frame's header and one inside
    // every frame's payload, from the transaction's first frame to the end.
    let mut cuts: Vec<u64> = spans[first..]
        .iter()
        .flat_map(|&(start, end)| [start, start + 5, (start + 8 + end) / 2])
        .collect();
    cuts.push(committed_at);

    let (mut saw_none, mut saw_all) = (false, false);
    for (n, &at) in cuts.iter().enumerate() {
        let committed = at >= committed_at;
        let heaps = if committed {
            vec![pre.path(), live.path()]
        } else {
            vec![pre.path()]
        };
        for (k, heap) in heaps.into_iter().enumerate() {
            let image = crash_image(heap, live.path(), at, &format!("cut{n}-{k}"));
            let mut db = Database::open(image.path(), Policy::manual(4))
                .unwrap_or_else(|e| panic!("cut at {at}, heap {k}: refused to open: {e}"));

            // Contracts 1 + 2: all or none, exact rows.
            let got = observed(&mut db);
            assert_eq!(
                got,
                expected(committed),
                "cut at {at}, heap {k}: not {} of the transaction",
                if committed { "all" } else { "none" }
            );
            if committed {
                saw_all = true;
            } else {
                saw_none = true;
            }

            // Contract 3: consistency, then idempotence.
            let report = db.verify().unwrap();
            assert!(
                report.problems.is_empty(),
                "cut at {at}, heap {k}: {}",
                report.problems.join("\n")
            );
            drop(db);
            let mut db2 = Database::open(image.path(), Policy::manual(4)).unwrap();
            assert_eq!(
                observed(&mut db2),
                got,
                "cut at {at}, heap {k}: reopening changed the outcome"
            );
            let r2 = db2.verify().unwrap();
            assert!(r2.problems.is_empty(), "cut at {at}: {:?}", r2.problems);
        }
    }
    assert!(
        saw_none && saw_all,
        "the cuts must land on both sides of the commit record"
    );
}

#[test]
fn a_restore_target_inside_a_transaction_exposes_none_of_it() {
    let t = Tmp::new("pitr");
    let mut db = Database::open(t.path(), Policy::manual(4)).unwrap();
    seed(&mut db);
    commit_txn(&mut db, false);
    drop(db);

    let entries = Wal::read_all(&HeapStore::wal_path(t.path())).unwrap();
    let first = entries.iter().position(|e| e.txn != TxnId(0)).unwrap();
    let commit = entries.len() - 1;
    assert!(matches!(entries[commit].op, WalOp::Commit { .. }));

    // Each target is restored into its own copy: a restore checkpoints what it
    // recovered, and the next target must start from the original log.
    for (n, e) in entries.iter().enumerate().skip(first) {
        let copy = Tmp::new(&format!("pitr-{n}"));
        copy_dir(t.path(), copy.path());
        let mut db =
            Database::open_at(copy.path(), Policy::manual(4), RecoverTarget::Lsn(e.lsn)).unwrap();
        assert_eq!(
            observed(&mut db),
            expected(n == commit),
            "restore to lsn {} (frame {n} of the log)",
            e.lsn.0
        );
        assert!(db.verify().unwrap().problems.is_empty());
    }
}
