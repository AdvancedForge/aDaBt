//! Crash-atomic groups of writes at the storage layer: `commit_writes` and
//! `insert_batch` survive any crash entirely or not at all.
//!
//! A crash is simulated the way the log experiences one: the active segment is
//! cut short — at a frame boundary, or part-way through a frame's header or
//! payload — and the store reopened. Every cut through a group's frames must
//! recover the state before the group or the state after it, and nothing in
//! between.

use adabt_core::error::Error;
use adabt_core::ids::{RecordId, TxnId};
use adabt_core::policy::Durability;
use adabt_core::record::Record;
use adabt_core::schema::Schema;
use adabt_core::store::LogicalStore;
use adabt_storage::heap::{HeapStore, RecoverTarget, TxnWrite};
use adabt_storage::wal::{Wal, WalEntry, WalOp};
use std::path::{Path, PathBuf};

struct Tmp(PathBuf);
impl Tmp {
    fn new(tag: &str) -> Self {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "adabt-atomic-{tag}-{}-{:?}",
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

fn rec(i: u64, gen: &str) -> Record {
    Record::new().with("i", i).with("gen", gen)
}

/// Bytes before the first frame of a segment.
const SEGMENT_HEADER: u64 = 40;

/// The only segment of the log in `dir`, asserting there is just one so that
/// its frames line up one-to-one with [`Wal::read_all`].
fn only_segment(dir: &Path) -> PathBuf {
    let segs: Vec<PathBuf> = std::fs::read_dir(HeapStore::wal_path(dir))
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("adabt"))
        .collect();
    assert_eq!(segs.len(), 1, "fixture expects a single log segment");
    segs[0].clone()
}

/// `(start, end)` byte offsets of every frame in a segment, in order.
fn frame_spans(segment: &Path) -> Vec<(u64, u64)> {
    let bytes = std::fs::read(segment).unwrap();
    let mut spans = Vec::new();
    let mut pos = SEGMENT_HEADER as usize;
    while pos + 8 <= bytes.len() {
        let len = u32::from_le_bytes(bytes[pos..pos + 4].try_into().unwrap()) as usize;
        let end = pos + 8 + len;
        spans.push((pos as u64, end as u64));
        pos = end;
    }
    spans
}

/// Every place a crash could cut the log from `first` (a frame index) to the
/// end: at each frame boundary, inside each frame's header, and inside each
/// frame's payload.
fn cuts_from(segment: &Path, first: usize) -> Vec<u64> {
    let mut cuts = Vec::new();
    for &(start, end) in &frame_spans(segment)[first..] {
        cuts.extend([start, start + 5, start + 8 + (end - start - 8) / 2]);
    }
    cuts.push(std::fs::metadata(segment).unwrap().len());
    cuts
}

/// The disk a crash could leave behind with the log cut at `at` bytes: the
/// heap file and catalog from `heap_from`, the log from `log_from`.
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

fn state(h: &mut HeapStore) -> Vec<(RecordId, Record)> {
    h.scan("c").unwrap()
}

/// Index of the first frame belonging to a transaction (non-zero txn tag).
fn first_tagged(entries: &[WalEntry]) -> usize {
    entries
        .iter()
        .position(|e| e.txn != TxnId(0))
        .expect("the group must be logged with a transaction id")
}

/// Run `group` against a store `seed` has filled, then demand that every cut
/// through the group's frames recovers exactly the state before it or exactly
/// the state after it — before, for every cut short of the end of its commit
/// record, and after, for every cut beyond.
///
/// The heap file in each image is chosen to be the worst one the protocol
/// allows at that point. Until the commit record is written nothing touches a
/// page, so the heap is the one from before the group. Once it is written,
/// any page the group dirtied may or may not have reached the file, so both
/// that heap and the final one — every page the two-page pool was forced to
/// steal while the group was applied — must recover the whole group.
fn assert_every_cut_is_all_or_none(
    tag: &str,
    seed: impl FnOnce(&mut HeapStore),
    group: impl FnOnce(&mut HeapStore),
) {
    let live = Tmp::new(&format!("{tag}-live"));
    let mut h = HeapStore::open(live.path(), Durability::Strict, 2).unwrap();
    h.create_collection("c", Schema::dynamic()).unwrap();
    seed(&mut h);
    let before = state(&mut h);
    let pre = Tmp::new(&format!("{tag}-pre"));
    copy_dir(live.path(), pre.path());
    let evicted = h.pool_stats().evictions;
    group(&mut h);
    assert!(
        h.pool_stats().evictions > evicted,
        "the fixture must make the pool steal pages while the group is applied"
    );
    let after = state(&mut h);
    assert_ne!(before, after);
    drop(h);

    let entries = Wal::read_all(&HeapStore::wal_path(live.path())).unwrap();
    assert!(matches!(entries.last().unwrap().op, WalOp::Commit { .. }));
    let seg = only_segment(live.path());
    let committed_at = frame_spans(&seg).last().unwrap().1;
    let mut cuts = 0;
    for (n, at) in cuts_from(&seg, first_tagged(&entries))
        .into_iter()
        .enumerate()
    {
        let (want, heaps) = if at >= committed_at {
            (&after, vec![pre.path(), live.path()])
        } else {
            (&before, vec![pre.path()])
        };
        for (k, heap) in heaps.into_iter().enumerate() {
            let image = crash_image(heap, live.path(), at, &format!("{tag}-cut{n}-{k}"));
            let mut h = HeapStore::open(image.path(), Durability::Strict, 2).unwrap();
            assert_eq!(&state(&mut h), want, "cut at byte {at}, heap image {k}");
            drop(h);
            // Recovery leaves nothing behind that changes a second open.
            let mut again = HeapStore::open(image.path(), Durability::Strict, 2).unwrap();
            assert_eq!(
                &state(&mut again),
                want,
                "cut at byte {at}: reopen changed it"
            );
            cuts += 1;
        }
    }
    assert!(cuts > 3, "the matrix must actually cut through the group");
}

#[test]
fn a_group_is_logged_as_tagged_frames_closed_by_one_commit_record() {
    let t = Tmp::new("shape");
    let mut h = HeapStore::open(t.path(), Durability::Strict, 64).unwrap();
    h.create_collection("c", Schema::dynamic()).unwrap();
    h.insert("c", RecordId(0), rec(0, "old")).unwrap();
    let writes = vec![
        TxnWrite::Put {
            collection: "c".into(),
            id: RecordId(1),
            record: rec(1, "txn"),
        },
        TxnWrite::Delete {
            collection: "c".into(),
            id: RecordId(0),
        },
        // Absent already: changes nothing, so logs nothing.
        TxnWrite::Delete {
            collection: "c".into(),
            id: RecordId(99),
        },
    ];
    assert_eq!(h.commit_writes(writes).unwrap(), vec![false, true, false]);
    drop(h);

    let entries = Wal::read_all(&HeapStore::wal_path(t.path())).unwrap();
    let first = first_tagged(&entries);
    let group = &entries[first..];
    let txn = TxnId(group[0].lsn.0);
    assert_eq!(group.len(), 3, "two frames and a commit: {group:?}");
    assert!(group.iter().all(|e| e.txn == txn), "{group:?}");
    assert!(matches!(group[0].op, WalOp::Update { .. }));
    assert!(matches!(group[1].op, WalOp::Delete { .. }));
    assert_eq!(group[2].op, WalOp::Commit { txn });
    // Everything before the group is still autocommit.
    assert!(entries[..first].iter().all(|e| e.txn == TxnId(0)));
}

#[test]
fn a_hundred_write_group_under_strict_costs_one_fsync() {
    let t = Tmp::new("fsync");
    let mut h = HeapStore::open(t.path(), Durability::Strict, 64).unwrap();
    h.create_collection("c", Schema::dynamic()).unwrap();
    let writes = (0..100u64)
        .map(|i| TxnWrite::Put {
            collection: "c".into(),
            id: RecordId(i),
            record: rec(i, "txn"),
        })
        .collect();
    let before = h.sync_count();
    h.commit_writes(writes).unwrap();
    assert_eq!(h.sync_count() - before, 1);
}

#[test]
fn every_cut_through_a_transaction_recovers_all_of_it_or_none() {
    let padded = |i: u64| rec(i, "txn").with("pad", "p".repeat(600));
    assert_every_cut_is_all_or_none(
        "txn",
        |h| {
            for i in 0..20u64 {
                h.insert("c", RecordId(i), rec(i, "old")).unwrap();
            }
        },
        |h| {
            // Updates, deletes and fresh inserts.
            let mut writes = Vec::new();
            for i in 0..20u64 {
                writes.push(if i % 3 == 0 {
                    TxnWrite::Delete {
                        collection: "c".into(),
                        id: RecordId(i),
                    }
                } else {
                    TxnWrite::Put {
                        collection: "c".into(),
                        id: RecordId(i),
                        record: padded(i),
                    }
                });
            }
            for i in 100..140u64 {
                writes.push(TxnWrite::Put {
                    collection: "c".into(),
                    id: RecordId(i),
                    record: padded(i),
                });
            }
            h.commit_writes(writes).unwrap();
        },
    );
}

#[test]
fn every_cut_through_a_batch_insert_recovers_all_of_it_or_none() {
    assert_every_cut_is_all_or_none(
        "batch",
        |h| {
            for i in 0..5u64 {
                h.insert("c", RecordId(i), rec(i, "old")).unwrap();
            }
        },
        |h| {
            let batch = (10..60u64)
                .map(|i| (RecordId(i), rec(i, "batch").with("pad", "b".repeat(400))))
                .collect();
            h.insert_batch("c", batch).unwrap();
        },
    );
}

#[test]
fn a_log_written_before_transactional_frames_replays_unchanged() {
    // The encoded records the old write paths would have logged, taken from
    // a real store so the bytes are exactly what its codec produces.
    let scratch = Tmp::new("old-log-bytes");
    let mut h = HeapStore::open(scratch.path(), Durability::Strict, 64).unwrap();
    h.create_collection("c", Schema::dynamic()).unwrap();
    for i in 0..6u64 {
        h.insert("c", RecordId(i), rec(i, "v1")).unwrap();
    }
    h.update("c", RecordId(1), rec(1, "v2")).unwrap();
    drop(h);
    let mut inserted = Vec::new();
    let mut updated = Vec::new();
    for e in Wal::read_all(&HeapStore::wal_path(scratch.path())).unwrap() {
        match e.op {
            WalOp::Insert { bytes, .. } => inserted.push(bytes),
            WalOp::Update { bytes, .. } => updated = bytes,
            _ => {}
        }
    }

    // Now a log written frame for frame as the old code paths wrote it: every
    // frame autocommit (`TxnId(0)`), no `Commit` anywhere.
    let t = Tmp::new("old-log");
    let mut h = HeapStore::open(t.path(), Durability::Strict, 64).unwrap();
    h.create_collection("c", Schema::dynamic()).unwrap();
    drop(h);
    let insert = |i: usize| WalOp::Insert {
        collection: "c".into(),
        id: RecordId(i as u64),
        bytes: inserted[i].clone(),
    };
    let mut wal = Wal::open(&HeapStore::wal_path(t.path()), Durability::Strict).unwrap();
    // An old batch insert: untagged frames under one fsync.
    for i in 0..4 {
        wal.append(TxnId(0), insert(i)).unwrap();
    }
    wal.commit().unwrap();
    // An old transaction commit: an update and a delete, each synced alone.
    let update = WalOp::Update {
        collection: "c".into(),
        id: RecordId(1),
        bytes: updated,
    };
    wal.append(TxnId(0), update).unwrap();
    wal.commit().unwrap();
    let delete = WalOp::Delete {
        collection: "c".into(),
        id: RecordId(2),
    };
    wal.append(TxnId(0), delete).unwrap();
    wal.commit().unwrap();
    // The tail of an old batch that a crash cut short. With no commit record
    // to wait for, untagged frames replay as the prefix they always did.
    for i in 4..6 {
        wal.append(TxnId(0), insert(i)).unwrap();
    }
    wal.sync().unwrap();
    drop(wal);

    let want = vec![
        (RecordId(0), rec(0, "v1")),
        (RecordId(1), rec(1, "v2")),
        (RecordId(3), rec(3, "v1")),
        (RecordId(4), rec(4, "v1")),
        (RecordId(5), rec(5, "v1")),
    ];
    let mut h = HeapStore::open(t.path(), Durability::Strict, 64).unwrap();
    assert_eq!(state(&mut h), want);
    drop(h);
    let mut again = HeapStore::open(t.path(), Durability::Strict, 64).unwrap();
    assert_eq!(state(&mut again), want, "replay is idempotent");
}

#[test]
fn a_restore_target_inside_a_transaction_excludes_all_of_it() {
    let t = Tmp::new("pitr");
    let backup = Tmp::new("pitr-backup");
    let mut h = HeapStore::open(t.path(), Durability::Strict, 64).unwrap();
    h.create_collection("c", Schema::dynamic()).unwrap();
    h.insert("c", RecordId(0), rec(0, "old")).unwrap();
    // The base every restore below starts from.
    h.backup_to(backup.path()).unwrap();
    let writes = (1..6u64)
        .map(|i| TxnWrite::Put {
            collection: "c".into(),
            id: RecordId(i),
            record: rec(i, "txn"),
        })
        .collect();
    h.commit_writes(writes).unwrap();
    h.insert("c", RecordId(9), rec(9, "after")).unwrap();
    drop(h);

    let entries = Wal::read_all(&HeapStore::wal_path(t.path())).unwrap();
    let first = first_tagged(&entries);
    let commit = entries
        .iter()
        .position(|e| matches!(e.op, WalOp::Commit { .. }))
        .unwrap();

    // Each target is restored from the backup into its own directory, given
    // the live log: a restore ends the log it restored at its target.
    let restore = |target: &WalEntry| {
        let copy = Tmp::new(&format!("pitr-{}", target.lsn.0));
        HeapStore::restore_from(backup.path(), copy.path()).unwrap();
        copy_dir(
            &HeapStore::wal_path(t.path()),
            &HeapStore::wal_path(copy.path()),
        );
        let r = HeapStore::open_at(
            copy.path(),
            Durability::Strict,
            64,
            RecoverTarget::Lsn(target.lsn),
        )
        .unwrap();
        (copy, r)
    };

    // Every target from the group's first frame to the one before its commit
    // record restores the state before the transaction.
    for target in &entries[first..commit] {
        let (_copy, mut r) = restore(target);
        assert_eq!(
            state(&mut r),
            vec![(RecordId(0), rec(0, "old"))],
            "target lsn {} exposed part of the transaction",
            target.lsn.0
        );
    }
    // The commit record itself is the moment the whole transaction appears.
    let (_copy, mut r) = restore(&entries[commit]);
    assert_eq!(r.count("c").unwrap(), 6);
    assert_eq!(r.get("c", RecordId(9)).unwrap(), None);
}

#[test]
fn a_commit_record_whose_frames_are_missing_is_corruption_not_a_partial_apply() {
    let t = Tmp::new("gap");
    let mut h = HeapStore::open(t.path(), Durability::Strict, 64).unwrap();
    h.create_collection("c", Schema::dynamic()).unwrap();
    h.insert("c", RecordId(0), rec(0, "old")).unwrap();
    drop(h);
    let bytes = match &Wal::read_all(&HeapStore::wal_path(t.path()))
        .unwrap()
        .last()
        .unwrap()
        .op
    {
        WalOp::Insert { bytes, .. } => bytes.clone(),
        other => panic!("unexpected {other:?}"),
    };

    // A transaction that claims to start one frame before its first surviving
    // frame: what the log would hold if a frame had vanished from its middle.
    let mut wal = Wal::open(&HeapStore::wal_path(t.path()), Durability::Strict).unwrap();
    let txn = TxnId(wal.next_lsn().0 - 1);
    wal.append(
        txn,
        WalOp::Update {
            collection: "c".into(),
            id: RecordId(0),
            bytes,
        },
    )
    .unwrap();
    wal.append(txn, WalOp::Commit { txn }).unwrap();
    wal.sync().unwrap();
    drop(wal);

    match HeapStore::open(t.path(), Durability::Strict, 64) {
        Err(Error::Corruption(msg)) => assert!(msg.contains("transaction"), "{msg}"),
        Err(e) => panic!("wrong error: {e}"),
        Ok(_) => panic!("a transaction missing a frame must not be applied"),
    }
}

#[test]
fn a_record_too_large_for_a_page_is_refused_before_it_is_logged() {
    let t = Tmp::new("too-large");
    let mut h = HeapStore::open(t.path(), Durability::Strict, 64).unwrap();
    h.create_collection("c", Schema::dynamic()).unwrap();
    let huge = Record::new().with("blob", "z".repeat(9_000));
    let logged = Wal::read_all(&HeapStore::wal_path(t.path())).unwrap().len();

    assert!(h.insert("c", RecordId(1), huge.clone()).is_err());
    assert!(h.update("c", RecordId(1), huge.clone()).is_err());
    assert!(h
        .insert_batch(
            "c",
            vec![(RecordId(2), rec(2, "ok")), (RecordId(3), huge.clone())]
        )
        .is_err());
    let writes = vec![
        TxnWrite::Put {
            collection: "c".into(),
            id: RecordId(4),
            record: rec(4, "ok"),
        },
        TxnWrite::Put {
            collection: "c".into(),
            id: RecordId(5),
            record: huge,
        },
    ];
    assert!(h.commit_writes(writes).is_err());
    drop(h);

    assert_eq!(
        Wal::read_all(&HeapStore::wal_path(t.path())).unwrap().len(),
        logged,
        "a refused write must leave nothing in the log"
    );
    let mut h = HeapStore::open(t.path(), Durability::Strict, 64).unwrap();
    assert_eq!(h.count("c").unwrap(), 0);
}

#[test]
fn transaction_ids_never_collide_with_an_abandoned_group() {
    // A group cut off before its commit record, then a new group after
    // reopening: the new one's id is its own first LSN, so the abandoned
    // frames can never be mistaken for part of it.
    let src = Tmp::new("abandoned");
    let mut h = HeapStore::open(src.path(), Durability::Strict, 64).unwrap();
    h.create_collection("c", Schema::dynamic()).unwrap();
    h.insert_batch(
        "c",
        (0..5u64).map(|i| (RecordId(i), rec(i, "lost"))).collect(),
    )
    .unwrap();
    drop(h);
    let seg = only_segment(src.path());
    let spans = frame_spans(&seg);
    // Keep every frame of the batch but its commit record.
    let (commit_start, _) = *spans.last().unwrap();
    let image = crash_image(src.path(), src.path(), commit_start, "abandoned-cut");
    let mut h = HeapStore::open(image.path(), Durability::Strict, 64).unwrap();
    assert_eq!(h.count("c").unwrap(), 0);
    h.insert_batch("c", vec![(RecordId(7), rec(7, "kept"))])
        .unwrap();
    drop(h);
    let entries = Wal::read_all(&HeapStore::wal_path(image.path())).unwrap();
    let tags: std::collections::BTreeSet<u64> = entries
        .iter()
        .filter(|e| e.txn != TxnId(0))
        .map(|e| e.txn.0)
        .collect();
    assert_eq!(tags.len(), 2, "two distinct groups: {tags:?}");
    let mut h = HeapStore::open(image.path(), Durability::Strict, 64).unwrap();
    assert_eq!(state(&mut h), vec![(RecordId(7), rec(7, "kept"))]);
}
