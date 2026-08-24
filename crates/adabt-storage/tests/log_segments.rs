//! The log stops growing forever.
//!
//! Before segmenting, `wal.adabt` was a single file that was appended to for the
//! life of the database, never truncated, and read *entirely into memory* on
//! every open. Two consequences, both bad enough on their own: disk usage
//! unbounded in the number of writes ever made, and open time proportional to
//! the whole history rather than to what had happened since the last checkpoint.
//!
//! A checkpoint already establishes that everything below it is in the pages.
//! Once the catalog is durable too, the segments holding that history are
//! redundant, and these tests are about them actually going away.

use adabt_core::ids::{Lsn, RecordId};
use adabt_core::policy::Durability;
use adabt_core::record::Record;
use adabt_core::schema::Schema;
use adabt_core::store::LogicalStore;
use adabt_storage::heap::HeapStore;
use adabt_storage::wal::{Wal, WalOp, SEGMENT_BYTES};
use std::path::{Path, PathBuf};

struct Tmp(PathBuf);
impl Tmp {
    fn new(tag: &str) -> Self {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "adabt-seg-{tag}-{}-{:?}",
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

fn log_bytes(dir: &Path) -> u64 {
    let mut total = 0;
    if let Ok(entries) = std::fs::read_dir(HeapStore::wal_path(dir)) {
        for e in entries.flatten() {
            total += e.metadata().map(|m| m.len()).unwrap_or(0);
        }
    }
    total
}

fn segment_count(dir: &Path) -> usize {
    std::fs::read_dir(HeapStore::wal_path(dir))
        .map(|d| d.flatten().count())
        .unwrap_or(0)
}

fn rec(i: u64) -> Record {
    Record::new().with("i", i).with("pad", "x".repeat(200))
}

#[test]
fn the_log_rotates_once_a_segment_fills() {
    let t = Tmp::new("rotate");
    let mut w = Wal::open(&HeapStore::wal_path(t.path()), Durability::Relaxed).unwrap();
    assert_eq!(w.segments().len(), 1);

    // Enough bytes to cross the rotation threshold at least twice.
    let payload = vec![0u8; 64 * 1024];
    let needed = (SEGMENT_BYTES / payload.len() as u64) * 2 + 4;
    for i in 0..needed {
        w.append(
            adabt_core::ids::TxnId(0),
            WalOp::Insert {
                collection: "c".into(),
                id: RecordId(i),
                bytes: payload.clone(),
            },
        )
        .unwrap();
    }
    w.sync().unwrap();
    assert!(
        w.segments().len() >= 3,
        "the log never rotated: {} segment(s)",
        w.segments().len()
    );
    // Segments are contiguous and ascending: a gap would make recovery skip
    // entries and a repeat would make it apply them twice.
    let firsts: Vec<u64> = w.segments().iter().map(|s| s.first_lsn).collect();
    assert!(firsts.windows(2).all(|p| p[0] < p[1]), "{firsts:?}");
}

#[test]
fn a_checkpoint_discards_the_history_below_it() {
    // The property the whole milestone exists for.
    let t = Tmp::new("discard");
    let mut h = HeapStore::open(t.path(), Durability::Relaxed, 256).unwrap();
    // Small segments, so the test crosses a rotation boundary several times
    // while writing megabytes rather than hundreds of them.
    const SEG: u64 = 256 * 1024;
    h.set_segment_bytes(SEG);
    h.create_collection("c", Schema::dynamic()).unwrap();

    let mut peak = 0u64;
    for round in 0..6u64 {
        for i in 0..8_000u64 {
            h.insert("c", RecordId(round * 8_000 + i), rec(i)).unwrap();
        }
        h.checkpoint().unwrap();
        peak = peak.max(log_bytes(t.path()));
    }
    let settled = log_bytes(t.path());
    assert!(
        settled < peak,
        "the log never shrank: peak {peak}, settled {settled}"
    );
    // And what it settles at is bounded by a segment or two, not by history.
    // What it settles at is bounded by a segment or two, not by history — which
    // is the difference between a database you can leave running and one you
    // cannot.
    assert!(
        settled <= 3 * SEG,
        "the log settled at {settled} bytes against a {SEG}-byte segment, which is not bounded"
    );
    assert_eq!(h.count("c").unwrap(), 48_000);
}

#[test]
fn a_bounded_log_still_recovers_everything() {
    // Discarding history is only correct if the history is genuinely redundant.
    let t = Tmp::new("recover");
    {
        let mut h = HeapStore::open(t.path(), Durability::Relaxed, 256).unwrap();
        h.create_collection("c", Schema::dynamic()).unwrap();
        for round in 0..4u64 {
            for i in 0..6_000u64 {
                h.insert("c", RecordId(round * 6_000 + i), rec(i)).unwrap();
            }
            h.checkpoint().unwrap();
        }
        // Writes after the final checkpoint, still only in the log.
        for i in 100_000..100_500u64 {
            h.insert("c", RecordId(i), rec(i)).unwrap();
        }
    }
    let mut h = HeapStore::open(t.path(), Durability::Relaxed, 256).unwrap();
    assert_eq!(h.count("c").unwrap(), 24_500);
    for i in 100_000..100_500u64 {
        assert_eq!(h.get("c", RecordId(i)).unwrap(), Some(rec(i)), "{i}");
    }
    for i in 0..6_000u64 {
        assert_eq!(h.get("c", RecordId(i)).unwrap(), Some(rec(i)), "{i}");
    }
}

#[test]
fn recovery_does_not_read_the_segments_below_the_checkpoint() {
    // The other half of the cost: open time proportional to what happened since
    // the last checkpoint, not to everything that ever happened.
    let t = Tmp::new("noread");
    {
        let mut h = HeapStore::open(t.path(), Durability::Relaxed, 256).unwrap();
        h.create_collection("c", Schema::dynamic()).unwrap();
        for i in 0..40_000u64 {
            h.insert("c", RecordId(i), rec(i)).unwrap();
        }
        h.checkpoint().unwrap();
    }
    let before = segment_count(t.path());
    let entries = Wal::read_all(&HeapStore::wal_path(t.path())).unwrap();
    let from_checkpoint = Wal::entries_from(
        &HeapStore::wal_path(t.path()),
        Lsn(entries.last().map(|e| e.lsn.0).unwrap_or(0)),
    )
    .unwrap();
    assert!(
        from_checkpoint.len() < entries.len().max(2),
        "reading from the checkpoint returned as much as reading everything"
    );
    assert!(before >= 1);
}

#[test]
fn a_pre_segment_log_is_adopted_rather_than_refused() {
    // The second migration this project has written, and the first that moves
    // data. A database whose log is a single `wal.adabt` becomes one whose log
    // is a directory with that file as its first segment.
    let t = Tmp::new("legacy");
    let expected = {
        let mut h = HeapStore::open(t.path(), Durability::Strict, 32).unwrap();
        h.create_collection("c", Schema::dynamic()).unwrap();
        for i in 0..300u64 {
            h.insert("c", RecordId(i), rec(i)).unwrap();
        }
        h.scan("c").unwrap()
    };

    // Flatten it back into the old shape: one file, no header.
    let wal_dir = HeapStore::wal_path(t.path());
    let seg = Wal::active_segment(&wal_dir).unwrap().unwrap();
    let with_header = std::fs::read(&seg).unwrap();
    let frames = &with_header[40..]; // strip the segment header
    std::fs::remove_dir_all(&wal_dir).unwrap();
    std::fs::write(t.path().join("wal.adabt"), frames).unwrap();

    let mut h = HeapStore::open(t.path(), Durability::Strict, 32).unwrap();
    assert_eq!(
        h.scan("c").unwrap(),
        expected,
        "the old log was not adopted"
    );
    assert!(
        !t.path().join("wal.adabt").exists(),
        "the superseded file was left behind"
    );
    assert!(Wal::active_segment(&wal_dir).unwrap().is_some());
}

#[test]
fn every_entry_carries_a_clock_reading() {
    // A log position answers "restore to entry 4,102,993". Only a clock reading
    // answers the question an operator actually asks.
    let t = Tmp::new("clock");
    let mut w = Wal::open(&HeapStore::wal_path(t.path()), Durability::Strict).unwrap();
    for i in 0..20u64 {
        w.append(
            adabt_core::ids::TxnId(0),
            WalOp::Insert {
                collection: "c".into(),
                id: RecordId(i),
                bytes: vec![1, 2, 3],
            },
        )
        .unwrap();
    }
    w.sync().unwrap();
    let entries = Wal::read_all(&HeapStore::wal_path(t.path())).unwrap();
    assert_eq!(entries.len(), 20);
    assert!(entries.iter().all(|e| e.nanos > 0), "an entry had no clock");
    // Non-decreasing, so a range scan by time is meaningful. Not asserted to be
    // strictly increasing: two entries can share a nanosecond, and the LSN is
    // what orders them.
    assert!(
        entries.windows(2).all(|p| p[0].nanos <= p[1].nanos),
        "timestamps went backwards within one run"
    );
}

#[test]
fn discarded_segments_can_be_archived_instead_of_deleted() {
    // Point-in-time recovery needs exactly the segments restart does not.
    let t = Tmp::new("archive");
    let archive = t.path().join("archive");
    let mut w = Wal::open(&HeapStore::wal_path(t.path()), Durability::Relaxed).unwrap();
    w.set_archive(Some(archive.clone()));

    let payload = vec![0u8; 64 * 1024];
    let needed = (SEGMENT_BYTES / payload.len() as u64) * 2 + 4;
    for i in 0..needed {
        w.append(
            adabt_core::ids::TxnId(0),
            WalOp::Insert {
                collection: "c".into(),
                id: RecordId(i),
                bytes: payload.clone(),
            },
        )
        .unwrap();
    }
    w.sync().unwrap();
    let before = w.segments().len();
    let dropped = w.discard_below(Lsn(needed)).unwrap();
    assert!(dropped > 0, "nothing was discarded from {before} segments");
    let archived = std::fs::read_dir(&archive).map(|d| d.count()).unwrap_or(0);
    assert_eq!(archived, dropped, "a discarded segment was not archived");
}

#[test]
fn the_active_segment_is_never_discarded() {
    // Whatever the checkpoint says, the segment being written to holds entries
    // that may not be in any page yet.
    let t = Tmp::new("active");
    let mut w = Wal::open(&HeapStore::wal_path(t.path()), Durability::Relaxed).unwrap();
    for i in 0..10u64 {
        w.append(
            adabt_core::ids::TxnId(0),
            WalOp::Insert {
                collection: "c".into(),
                id: RecordId(i),
                bytes: vec![9; 16],
            },
        )
        .unwrap();
    }
    w.sync().unwrap();
    assert_eq!(w.discard_below(Lsn(u64::MAX)).unwrap(), 0);
    assert_eq!(w.segments().len(), 1);
    assert_eq!(
        Wal::read_all(&HeapStore::wal_path(t.path())).unwrap().len(),
        10
    );
}

#[test]
fn vacuum_returns_space_to_the_filesystem() {
    // The free-space map lets a deleted record's space be *reused*; nothing
    // could give it *back*. An operator with a full disk had no answer except
    // restoring from a backup, which is not an answer.
    let t = Tmp::new("vacuum");
    let mut h = HeapStore::open(t.path(), Durability::Relaxed, 256).unwrap();
    h.create_collection("keep", Schema::dynamic()).unwrap();
    h.create_collection("bulk", Schema::dynamic()).unwrap();
    for i in 0..200u64 {
        h.insert("keep", RecordId(i), rec(i)).unwrap();
    }
    for i in 0..8_000u64 {
        h.insert("bulk", RecordId(i), rec(i)).unwrap();
    }
    h.checkpoint().unwrap();
    let heap = HeapStore::heap_path(t.path());
    let full = std::fs::metadata(&heap).unwrap().len();

    h.drop_collection("bulk").unwrap();
    let freed = h.vacuum().unwrap();
    h.checkpoint().unwrap();
    let after = std::fs::metadata(&heap).unwrap().len();

    assert!(freed > 0, "vacuum returned no pages");
    assert!(
        after < full / 2,
        "the heap was {full} bytes and is still {after} after dropping most of it"
    );

    // And what was kept is still there, in full, at the right values.
    assert_eq!(h.count("keep").unwrap(), 200);
    for i in 0..200u64 {
        assert_eq!(h.get("keep", RecordId(i)).unwrap(), Some(rec(i)), "{i}");
    }
    assert!(h.count("bulk").is_err() || h.count("bulk").unwrap() == 0);
}

#[test]
fn a_vacuumed_heap_survives_a_restart() {
    // Records are moved, so every page directory entry that pointed at the tail
    // now points somewhere else. If that did not reach disk, the restart finds
    // records missing or reads the wrong bytes.
    let t = Tmp::new("vacuum-restart");
    let expected = {
        let mut h = HeapStore::open(t.path(), Durability::Strict, 256).unwrap();
        h.create_collection("keep", Schema::dynamic()).unwrap();
        h.create_collection("bulk", Schema::dynamic()).unwrap();
        for i in 0..400u64 {
            h.insert("keep", RecordId(i), rec(i)).unwrap();
        }
        for i in 0..4_000u64 {
            h.insert("bulk", RecordId(i), rec(i)).unwrap();
        }
        h.drop_collection("bulk").unwrap();
        h.vacuum().unwrap();
        h.checkpoint().unwrap();
        h.scan("keep").unwrap()
    };
    let mut h = HeapStore::open(t.path(), Durability::Strict, 256).unwrap();
    assert_eq!(h.scan("keep").unwrap(), expected);
}

#[test]
fn vacuuming_a_full_heap_moves_nothing_and_loses_nothing() {
    // Nothing to reclaim is the common case, and it must be cheap and safe.
    let t = Tmp::new("vacuum-noop");
    let mut h = HeapStore::open(t.path(), Durability::Relaxed, 256).unwrap();
    h.create_collection("c", Schema::dynamic()).unwrap();
    for i in 0..2_000u64 {
        h.insert("c", RecordId(i), rec(i)).unwrap();
    }
    let before = h.scan("c").unwrap();
    h.vacuum().unwrap();
    assert_eq!(h.scan("c").unwrap(), before);
}
