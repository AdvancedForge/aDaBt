//! Backup, restore, and point-in-time recovery.
//!
//! A backup is not a new file format: it is a database directory, produced by
//! `HeapStore::backup_to`, that opens with the exact same `HeapStore::open`
//! every other database directory does. Restoring is copying it back.
//! Point-in-time recovery is `open_at` handed a `RecoverTarget::Lsn` instead
//! of replaying everything present — the same recovery passes every other
//! open runs, fed a log this crate deliberately shortened instead of one a
//! crash happened to shorten.

use adabt_core::ids::{Lsn, RecordId};
use adabt_core::policy::Durability;
use adabt_core::record::Record;
use adabt_core::schema::Schema;
use adabt_core::store::LogicalStore;
use adabt_storage::heap::{HeapStore, RecoverTarget};
use adabt_storage::wal::{Wal, WalOp};
use std::path::{Path, PathBuf};
use std::time::Duration;

struct Tmp(PathBuf);
impl Tmp {
    fn new(tag: &str) -> Self {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "adabt-backup-{tag}-{}-{:?}",
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

fn rec(i: u64) -> Record {
    Record::new().with("i", i).with("pad", "x".repeat(20))
}

#[test]
fn a_backup_is_openable_and_matches_the_source() {
    let src = Tmp::new("src");
    let dest = Tmp::new("dest");
    let mut h = HeapStore::open(src.path(), Durability::Strict, 32).unwrap();
    h.create_collection("c", Schema::dynamic()).unwrap();
    for i in 0..100 {
        h.insert("c", RecordId(i), rec(i)).unwrap();
    }
    h.backup_to(dest.path()).unwrap();

    let mut restored = HeapStore::open(dest.path(), Durability::Strict, 32).unwrap();
    assert_eq!(restored.count("c").unwrap(), 100);
    for i in 0..100 {
        assert_eq!(restored.get("c", RecordId(i)).unwrap(), Some(rec(i)), "{i}");
    }

    // The two are independent from here: neither backup nor restore leaves
    // the copy aliased to the source's files.
    h.insert("c", RecordId(100), rec(100)).unwrap();
    assert_eq!(h.count("c").unwrap(), 101);
    assert_eq!(restored.count("c").unwrap(), 100);
}

#[test]
fn a_backup_is_a_base_image_and_omits_the_derived_cache() {
    let src = Tmp::new("nocache-src");
    let dest = Tmp::new("nocache-dest");
    let mut h = HeapStore::open(src.path(), Durability::Strict, 32).unwrap();
    h.create_collection("c", Schema::dynamic()).unwrap();
    for i in 0..20 {
        h.insert("c", RecordId(i), rec(i)).unwrap();
    }
    h.checkpoint().unwrap();
    std::fs::write(adabt_storage::derived::path(src.path()), b"cache").unwrap();
    h.backup_to(dest.path()).unwrap();

    assert!(!dest.path().join("derived.adabt").exists());
    assert!(dest.path().join("heap.adabt").exists());
    assert!(dest.path().join("catalog.adabt").exists());
    assert!(dest.path().join("superblock.adabt").exists());
    assert!(dest.path().join("wal").is_dir());
    // The page directory and the base-image marker are part of the backup:
    // together they are what lets a restore start from it.
    assert!(dest.path().join("directory.adabt").exists());
    assert!(adabt_storage::base_image::read(dest.path(), identity_of(src.path())).is_some());
}

/// The identity a directory's superblock gives its database.
fn identity_of(dir: &Path) -> u128 {
    adabt_storage::superblock::open_or_create(dir, adabt_storage::page::PAGE_SIZE as u32)
        .unwrap()
        .identity
}

#[test]
fn a_backup_does_not_bring_back_what_was_deleted_or_overwritten_before_it() {
    // A deleted record, or the old version of an updated one, keeps its slot
    // until version reclamation gets to it, so the backup's heap file holds
    // both. Reading that file by scanning its pages brought them back.
    let src = Tmp::new("dead-src");
    let backup = Tmp::new("dead-backup");
    let dest = Tmp::new("dead-dest");
    let mut h = HeapStore::open(src.path(), Durability::Strict, 32).unwrap();
    h.create_collection("c", Schema::dynamic()).unwrap();
    for i in 0..10 {
        h.insert("c", RecordId(i), rec(i)).unwrap();
    }
    h.delete("c", RecordId(3)).unwrap();
    h.update("c", RecordId(4), rec(400)).unwrap();
    h.backup_to(backup.path()).unwrap();

    let mut want: Vec<(RecordId, Record)> = (0..10)
        .filter(|&i| i != 3)
        .map(|i| (RecordId(i), if i == 4 { rec(400) } else { rec(i) }))
        .collect();
    want.sort_by_key(|(id, _)| *id);
    let mut opened = HeapStore::open(backup.path(), Durability::Strict, 32).unwrap();
    assert_eq!(opened.scan("c").unwrap(), want);

    HeapStore::restore_from(backup.path(), dest.path()).unwrap();
    let at_base = last_lsn(dest.path());
    let mut restored = HeapStore::open_at(
        dest.path(),
        Durability::Strict,
        32,
        RecoverTarget::Lsn(at_base),
    )
    .unwrap();
    assert_eq!(restored.scan("c").unwrap(), want);
}

#[test]
fn backup_refuses_a_non_empty_destination() {
    let src = Tmp::new("refuse-src");
    let dest = Tmp::new("refuse-dest");
    let mut h = HeapStore::open(src.path(), Durability::Strict, 32).unwrap();
    h.create_collection("c", Schema::dynamic()).unwrap();
    h.backup_to(dest.path()).unwrap();

    let err = h.backup_to(dest.path()).unwrap_err();
    assert!(matches!(err, adabt_core::error::Error::InvalidRestore(_)));
    // And the first backup is untouched.
    let restored = HeapStore::open(dest.path(), Durability::Strict, 32).unwrap();
    assert_eq!(restored.collection_names(), vec!["c".to_string()]);
}

#[test]
fn restore_refuses_a_directory_that_is_not_a_backup() {
    let not_a_backup = Tmp::new("notreal");
    std::fs::create_dir_all(not_a_backup.path()).unwrap();
    std::fs::write(not_a_backup.path().join("hello.txt"), b"nope").unwrap();

    let dest = Tmp::new("notreal-dest");
    let err = HeapStore::restore_from(not_a_backup.path(), dest.path()).unwrap_err();
    assert!(matches!(err, adabt_core::error::Error::InvalidRestore(_)));
}

#[test]
fn restore_refuses_a_non_empty_destination() {
    let src = Tmp::new("occ-src");
    let backup = Tmp::new("occ-backup");
    let dest = Tmp::new("occ-dest");
    let mut h = HeapStore::open(src.path(), Durability::Strict, 32).unwrap();
    h.create_collection("c", Schema::dynamic()).unwrap();
    h.backup_to(backup.path()).unwrap();

    std::fs::create_dir_all(dest.path()).unwrap();
    std::fs::write(dest.path().join("x"), b"occupied").unwrap();
    let err = HeapStore::restore_from(backup.path(), dest.path()).unwrap_err();
    assert!(matches!(err, adabt_core::error::Error::InvalidRestore(_)));
}

#[test]
fn restore_then_open_matches_backup_then_open() {
    let src = Tmp::new("equiv-src");
    let backup = Tmp::new("equiv-backup");
    let restored_dir = Tmp::new("equiv-restored");
    let mut h = HeapStore::open(src.path(), Durability::Strict, 32).unwrap();
    h.create_collection("c", Schema::dynamic()).unwrap();
    for i in 0..40 {
        h.insert("c", RecordId(i), rec(i)).unwrap();
    }
    h.backup_to(backup.path()).unwrap();
    HeapStore::restore_from(backup.path(), restored_dir.path()).unwrap();

    let mut restored = HeapStore::open(restored_dir.path(), Durability::Strict, 32).unwrap();
    assert_eq!(restored.count("c").unwrap(), 40);
    for i in 0..40 {
        assert_eq!(restored.get("c", RecordId(i)).unwrap(), Some(rec(i)));
    }
}

/// Restore `backup` into a fresh directory, hand it `live`'s log, and open it
/// at `target` — the whole supported procedure.
fn restore_to(backup: &Path, live: &Path, tag: &str, target: Lsn) -> (Tmp, HeapStore) {
    let dest = Tmp::new(tag);
    HeapStore::restore_from(backup, dest.path()).unwrap();
    supply_segments(&HeapStore::wal_path(live), dest.path());
    let h = HeapStore::open_at(
        dest.path(),
        Durability::Strict,
        32,
        RecoverTarget::Lsn(target),
    )
    .unwrap();
    (dest, h)
}

#[test]
fn open_at_a_target_lsn_replays_only_up_to_it() {
    let t = Tmp::new("pitr-lsn");
    let backup = Tmp::new("pitr-lsn-backup");
    let mut h = HeapStore::open(t.path(), Durability::Strict, 32).unwrap();
    h.create_collection("c", Schema::dynamic()).unwrap();
    h.backup_to(backup.path()).unwrap();
    for i in 0..10 {
        h.insert("c", RecordId(i), rec(i)).unwrap();
    }
    drop(h);

    let entries = Wal::read_all(&HeapStore::wal_path(t.path())).unwrap();
    let cutoff = entries
        .iter()
        .find_map(|e| match &e.op {
            WalOp::Insert { id, .. } if id.0 == 4 => Some(e.lsn),
            _ => None,
        })
        .expect("insert of id 4 must be in the log");

    let (_dest, mut restored) = restore_to(backup.path(), t.path(), "pitr-lsn-dest", cutoff);
    assert_eq!(restored.count("c").unwrap(), 5, "ids 0..=4 only");
    for i in 0..=4 {
        assert_eq!(restored.get("c", RecordId(i)).unwrap(), Some(rec(i)), "{i}");
    }
    for i in 5..10 {
        assert_eq!(restored.get("c", RecordId(i)).unwrap(), None, "{i}");
    }
}

#[test]
fn open_at_refuses_a_target_the_backups_own_checkpoint_already_passed() {
    let t = Tmp::new("pitr-refuse");
    let mut h = HeapStore::open(t.path(), Durability::Strict, 32).unwrap();
    h.create_collection("c", Schema::dynamic()).unwrap();
    for i in 0..5 {
        h.insert("c", RecordId(i), rec(i)).unwrap();
    }
    h.checkpoint().unwrap();
    for i in 5..10 {
        h.insert("c", RecordId(i), rec(i)).unwrap();
    }
    drop(h);

    match HeapStore::open_at(t.path(), Durability::Strict, 32, RecoverTarget::Lsn(Lsn(0))) {
        Err(adabt_core::error::Error::RestoreTargetUnreachable { .. }) => {}
        Err(e) => panic!("wrong error: {e}"),
        Ok(_) => panic!("should have refused a target before the checkpoint"),
    }
}

#[test]
fn lsn_at_or_before_resolves_a_timestamp_to_the_right_prefix() {
    let t = Tmp::new("pitr-time");
    let backup = Tmp::new("pitr-time-backup");
    let mut h = HeapStore::open(t.path(), Durability::Strict, 32).unwrap();
    h.create_collection("c", Schema::dynamic()).unwrap();
    h.backup_to(backup.path()).unwrap();
    for i in 0..3 {
        h.insert("c", RecordId(i), rec(i)).unwrap();
    }
    let marker_nanos = Wal::read_all(&HeapStore::wal_path(t.path()))
        .unwrap()
        .last()
        .unwrap()
        .nanos;
    // Wide enough that clock resolution cannot make the next batch's
    // timestamps collide with the marker's.
    std::thread::sleep(Duration::from_millis(5));
    for i in 3..10 {
        h.insert("c", RecordId(i), rec(i)).unwrap();
    }
    drop(h);

    let lsn = Wal::lsn_at_or_before(&HeapStore::wal_path(t.path()), marker_nanos)
        .unwrap()
        .expect("an entry at or before the marker must exist");
    let (_dest, mut restored) = restore_to(backup.path(), t.path(), "pitr-time-dest", lsn);
    assert_eq!(restored.count("c").unwrap(), 3);
    for i in 0..3 {
        assert_eq!(restored.get("c", RecordId(i)).unwrap(), Some(rec(i)));
    }
}

#[test]
fn a_pitr_restore_reaches_a_point_after_the_backup_via_supplied_wal_segments() {
    // The production shape: a base backup at a checkpoint, plus WAL segments
    // from after it — ordinarily supplied by `set_log_archive` rotating them
    // out of the live directory, stood in for here by copying the live
    // database's own log directly, since these few inserts are nowhere near
    // the rotation threshold. What this actually exercises either way: that
    // `open_at` replays a target lsn from a log a backup did not itself
    // contain, as long as the segments covering it are present.
    let live = Tmp::new("full-live");
    let backup = Tmp::new("full-backup");

    let mut h = HeapStore::open(live.path(), Durability::Strict, 32).unwrap();
    h.create_collection("c", Schema::dynamic()).unwrap();
    for i in 0..5 {
        h.insert("c", RecordId(i), rec(i)).unwrap();
    }
    h.backup_to(backup.path()).unwrap();

    // More writes after the backup, which prove the backup really did stop
    // at the checkpoint rather than accidentally capturing these too.
    for i in 5..15 {
        h.insert("c", RecordId(i), rec(i)).unwrap();
    }
    let entries = Wal::read_all(&HeapStore::wal_path(live.path())).unwrap();
    let target = entries
        .iter()
        .find_map(|e| match &e.op {
            WalOp::Insert { id, .. } if id.0 == 10 => Some(e.lsn),
            _ => None,
        })
        .unwrap();
    drop(h);

    // Copy the live database's current log segments into the backup's log
    // directory — standing in for "copy what the archive collected since the
    // backup," since nothing here rotated a segment out to archive yet.
    let backup_wal = HeapStore::wal_path(backup.path());
    std::fs::remove_dir_all(&backup_wal).unwrap();
    copy_dir(&HeapStore::wal_path(live.path()), &backup_wal);

    let mut restored = HeapStore::open_at(
        backup.path(),
        Durability::Strict,
        32,
        RecoverTarget::Lsn(target),
    )
    .unwrap();
    assert_eq!(restored.count("c").unwrap(), 11, "ids 0..=10");
    for i in 0..=10 {
        assert_eq!(restored.get("c", RecordId(i)).unwrap(), Some(rec(i)));
    }
    for i in 11..15 {
        assert_eq!(restored.get("c", RecordId(i)).unwrap(), None);
    }
}

fn copy_dir(from: &Path, to: &Path) {
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

// ---------------------------------------------------------------------------
// Point-in-time recovery from a base image.
//
// The buffer pool steals: it writes a dirty page to the heap file whenever it
// needs the frame. A restore that stops at a target cannot correct a page
// written after that target, so it must start from a heap known to hold
// nothing after some earlier point — a backup — and replay the archived log
// from there. These tests force stealing with a two-page pool.
// ---------------------------------------------------------------------------

/// A record big enough that a handful fill a page, so a two-page pool has to
/// evict on nearly every write.
fn wide(i: u64, generation: u64) -> Record {
    Record::new()
        .with("i", i)
        .with("gen", generation)
        .with("pad", "x".repeat(900))
}

/// The LSN of the newest entry in the live log.
fn last_lsn(dir: &Path) -> Lsn {
    Wal::read_all(&HeapStore::wal_path(dir))
        .unwrap()
        .last()
        .expect("the log is not empty")
        .lsn
}

/// Copy every log segment in `from` into `dir`'s log, replacing a segment of
/// the same name — the step that hands a restored backup the history after it.
fn supply_segments(from: &Path, dir: &Path) {
    let wal = HeapStore::wal_path(dir);
    for e in std::fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        if e.file_type().unwrap().is_file() {
            std::fs::copy(e.path(), wal.join(e.file_name())).unwrap();
        }
    }
}

/// A database whose heap file has had pages written since its last
/// checkpoint: ten records, a checkpoint (or a backup, which takes one), then
/// twenty more through a two-page pool. Returns the LSN of the first of the
/// twenty.
fn steal_after_checkpoint(dir: &Path, backup: Option<&Path>) -> Lsn {
    let mut h = HeapStore::open(dir, Durability::Strict, 32).unwrap();
    h.create_collection("c", Schema::dynamic()).unwrap();
    for i in 0..10 {
        h.insert("c", RecordId(i), wide(i, 0)).unwrap();
    }
    match backup {
        Some(b) => h.backup_to(b).unwrap(),
        None => h.checkpoint().unwrap(),
    }
    h.set_pool_capacity(2).unwrap();
    let evicted = h.buffer_stats().evictions;
    let first = Lsn(last_lsn(dir).0 + 1);
    for i in 10..30 {
        h.insert("c", RecordId(i), wide(i, 0)).unwrap();
    }
    assert!(
        h.buffer_stats().evictions > evicted,
        "the fixture must write pages to the heap file after the checkpoint"
    );
    first
}

#[test]
fn open_at_refuses_a_heap_with_pages_written_after_its_checkpoint() {
    // The original failure: replaying to a target over this heap file showed
    // every row the pool had already written — 20 where the target had 11.
    let t = Tmp::new("steal-refuse");
    let target = steal_after_checkpoint(t.path(), None);

    let heap_before = std::fs::read(HeapStore::heap_path(t.path())).unwrap();
    match HeapStore::open_at(t.path(), Durability::Strict, 32, RecoverTarget::Lsn(target)) {
        Err(adabt_core::error::Error::InvalidRestore(msg)) => {
            assert!(msg.contains("not a base image"), "{msg}");
            assert!(
                msg.contains("restore_from"),
                "the error must say what to do: {msg}"
            );
        }
        Err(e) => panic!("wrong error: {e}"),
        Ok(mut h) => panic!(
            "restored over a heap holding later writes: {} rows",
            h.count("c").unwrap()
        ),
    }
    // A refusal changes nothing: the directory still opens to its latest state.
    assert_eq!(
        std::fs::read(HeapStore::heap_path(t.path())).unwrap(),
        heap_before
    );
    let mut h = HeapStore::open(t.path(), Durability::Strict, 32).unwrap();
    assert_eq!(h.count("c").unwrap(), 30);
}

#[test]
fn a_restored_backup_recovers_exactly_the_target_however_much_the_source_stole() {
    let live = Tmp::new("steal-live");
    let backup = Tmp::new("steal-backup");
    let dest = Tmp::new("steal-dest");
    let target = steal_after_checkpoint(live.path(), Some(backup.path()));

    HeapStore::restore_from(backup.path(), dest.path()).unwrap();
    supply_segments(&HeapStore::wal_path(live.path()), dest.path());
    let mut r = HeapStore::open_at(
        dest.path(),
        Durability::Strict,
        2,
        RecoverTarget::Lsn(target),
    )
    .unwrap();
    assert_eq!(r.count("c").unwrap(), 11, "ids 0..=10");
    for i in 0..=10 {
        assert_eq!(r.get("c", RecordId(i)).unwrap(), Some(wide(i, 0)), "{i}");
    }
}

#[test]
fn a_checkpoint_with_no_later_page_write_is_a_valid_base_in_place() {
    // Refusing is only necessary when it cannot be shown that the heap holds
    // nothing after the target. Straight after a checkpoint it can.
    let t = Tmp::new("clean-in-place");
    let mut h = HeapStore::open(t.path(), Durability::Strict, 64).unwrap();
    h.create_collection("c", Schema::dynamic()).unwrap();
    for i in 0..5 {
        h.insert("c", RecordId(i), rec(i)).unwrap();
    }
    h.checkpoint().unwrap();
    let checkpoint_entry = last_lsn(t.path());
    for i in 5..8 {
        h.insert("c", RecordId(i), rec(i)).unwrap();
    }
    assert_eq!(
        h.buffer_stats().evictions,
        0,
        "nothing reached the heap file"
    );
    drop(h);

    let target = Lsn(checkpoint_entry.0 + 2);
    let mut r =
        HeapStore::open_at(t.path(), Durability::Strict, 64, RecoverTarget::Lsn(target)).unwrap();
    assert_eq!(r.count("c").unwrap(), 7, "ids 0..=6");
}

#[test]
fn a_restore_refuses_a_log_that_does_not_reach_the_target() {
    let live = Tmp::new("short-live");
    let backup = Tmp::new("short-backup");
    let dest = Tmp::new("short-dest");
    let target = steal_after_checkpoint(live.path(), Some(backup.path()));

    // Without the source's later log, the backup's own log ends at its
    // checkpoint. Replaying "to the target" would quietly stop short of it.
    HeapStore::restore_from(backup.path(), dest.path()).unwrap();
    match HeapStore::open_at(
        dest.path(),
        Durability::Strict,
        32,
        RecoverTarget::Lsn(target),
    ) {
        Err(adabt_core::error::Error::InvalidRestore(msg)) => {
            assert!(msg.contains("log ends at"), "{msg}")
        }
        Err(e) => panic!("wrong error: {e}"),
        Ok(_) => panic!("restored to a target its log does not reach"),
    }
    // A refusal changes nothing, so supplying the log afterwards works.
    supply_segments(&HeapStore::wal_path(live.path()), dest.path());
    let mut r = HeapStore::open_at(
        dest.path(),
        Durability::Strict,
        32,
        RecoverTarget::Lsn(target),
    )
    .unwrap();
    assert_eq!(r.count("c").unwrap(), 11);
}

#[test]
fn a_restored_backup_given_later_log_opens_to_the_end_of_it() {
    // The supplied log records the source's later checkpoints, whose pages the
    // backup's heap file never received. Recovery must start from the backup's
    // own checkpoint, not from the last one the log mentions.
    let live = Tmp::new("later-live");
    let backup = Tmp::new("later-backup");
    let dest = Tmp::new("later-dest");
    steal_after_checkpoint(live.path(), Some(backup.path()));
    let mut h = HeapStore::open(live.path(), Durability::Strict, 32).unwrap();
    h.checkpoint().unwrap();
    h.insert("c", RecordId(30), wide(30, 0)).unwrap();
    drop(h);

    HeapStore::restore_from(backup.path(), dest.path()).unwrap();
    supply_segments(&HeapStore::wal_path(live.path()), dest.path());
    let mut r = HeapStore::open(dest.path(), Durability::Strict, 32).unwrap();
    assert_eq!(r.count("c").unwrap(), 31, "ids 0..=30");
    for i in 0..=30 {
        assert_eq!(r.get("c", RecordId(i)).unwrap(), Some(wide(i, 0)), "{i}");
    }
}

#[test]
fn a_restore_refuses_a_log_with_a_segment_missing() {
    let live = Tmp::new("gap-live");
    let archive = Tmp::new("gap-archive");
    let backup = Tmp::new("gap-backup");
    let dest = Tmp::new("gap-dest");

    let mut h = HeapStore::open(live.path(), Durability::Strict, 32).unwrap();
    h.set_segment_bytes(2048);
    h.set_log_archive(Some(archive.path().to_path_buf()));
    h.create_collection("c", Schema::dynamic()).unwrap();
    h.backup_to(backup.path()).unwrap();
    for i in 0..200 {
        h.insert("c", RecordId(i), rec(i)).unwrap();
    }
    h.checkpoint().unwrap();
    drop(h);
    let target = last_lsn(live.path());

    let mut archived: Vec<_> = std::fs::read_dir(archive.path())
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    archived.sort();
    assert!(
        archived.len() >= 3,
        "the fixture must archive several segments"
    );

    HeapStore::restore_from(backup.path(), dest.path()).unwrap();
    supply_segments(archive.path(), dest.path());
    supply_segments(&HeapStore::wal_path(live.path()), dest.path());
    // Lose one from the middle of the history.
    let lost = archived[archived.len() / 2].file_name().unwrap().to_owned();
    std::fs::remove_file(HeapStore::wal_path(dest.path()).join(lost)).unwrap();
    match HeapStore::open_at(
        dest.path(),
        Durability::Strict,
        32,
        RecoverTarget::Lsn(target),
    ) {
        Err(adabt_core::error::Error::InvalidRestore(msg)) => {
            assert!(msg.contains("missing lsns"), "{msg}")
        }
        Err(e) => panic!("wrong error: {e}"),
        Ok(_) => panic!("replayed around a hole in the log"),
    }
}

#[test]
fn a_restore_ends_the_log_at_its_target_and_keeps_what_followed() {
    // The entries after the target are a history the restored database did
    // not have. Left in the log, the next ordinary open would replay them and
    // the restored database's own writes would be interleaved with them.
    let live = Tmp::new("timeline-live");
    let backup = Tmp::new("timeline-backup");
    let dest = Tmp::new("timeline-dest");
    let target = steal_after_checkpoint(live.path(), Some(backup.path()));
    HeapStore::restore_from(backup.path(), dest.path()).unwrap();
    supply_segments(&HeapStore::wal_path(live.path()), dest.path());

    let mut r = HeapStore::open_at(
        dest.path(),
        Durability::Strict,
        32,
        RecoverTarget::Lsn(target),
    )
    .unwrap();
    r.insert("c", RecordId(100), wide(100, 1)).unwrap();
    drop(r);

    let mut reopened = HeapStore::open(dest.path(), Durability::Strict, 32).unwrap();
    let ids: Vec<u64> = reopened
        .scan("c")
        .unwrap()
        .into_iter()
        .map(|(id, _)| id.0)
        .collect();
    let mut want: Vec<u64> = (0..=10).collect();
    want.push(100);
    assert_eq!(ids, want, "the superseded history came back");

    // Nothing was deleted: the log after the target was set aside whole.
    let aside = dest
        .path()
        .join("superseded-log")
        .join(format!("after-{:016}", target.0));
    let kept = Wal::read_all(&aside).unwrap();
    assert!(kept.iter().any(|e| e.lsn.0 == target.0 + 1));
    assert!(kept
        .iter()
        .any(|e| matches!(&e.op, WalOp::Insert { id, .. } if id.0 == 29)));
}

/// One step of the reference workload, applied to the store and to the model.
fn step(
    rng: &mut adabt_testkit::rng::Rng,
    h: &mut HeapStore,
    model: &mut std::collections::BTreeMap<RecordId, Record>,
    next_id: &mut u64,
    generation: u64,
) {
    let existing: Vec<RecordId> = model.keys().copied().collect();
    match rng.below(10) {
        4..=5 if !existing.is_empty() => {
            let id = *rng.pick(&existing);
            h.update("c", id, wide(id.0, generation)).unwrap();
            model.insert(id, wide(id.0, generation));
        }
        6..=7 if !existing.is_empty() => {
            let id = *rng.pick(&existing);
            h.delete("c", id).unwrap();
            model.remove(&id);
        }
        8..=9 => {
            // A crash-atomic group: puts to new and existing ids, and a delete.
            let mut writes = Vec::new();
            for _ in 0..3 {
                let id = RecordId(*next_id);
                *next_id += 1;
                writes.push((id, Some(wide(id.0, generation))));
            }
            if !existing.is_empty() {
                let id = *rng.pick(&existing);
                writes.push((id, Some(wide(id.0, generation))));
                let gone = *rng.pick(&existing);
                if gone != id {
                    writes.push((gone, None));
                }
            }
            let group = writes
                .iter()
                .map(|(id, r)| match r {
                    Some(record) => adabt_storage::heap::TxnWrite::Put {
                        collection: "c".into(),
                        id: *id,
                        record: record.clone(),
                    },
                    None => adabt_storage::heap::TxnWrite::Delete {
                        collection: "c".into(),
                        id: *id,
                    },
                })
                .collect();
            h.commit_writes(group).unwrap();
            for (id, r) in writes {
                match r {
                    Some(record) => model.insert(id, record),
                    None => model.remove(&id),
                };
            }
        }
        _ => {
            let id = RecordId(*next_id);
            *next_id += 1;
            h.insert("c", id, wide(id.0, generation)).unwrap();
            model.insert(id, wide(id.0, generation));
        }
    }
}

#[test]
fn pitr_from_a_backup_recovers_exactly_the_committed_prefix_at_every_lsn() {
    let live = Tmp::new("sweep-live");
    let archive = Tmp::new("sweep-archive");
    let backup = Tmp::new("sweep-backup");

    let mut h = HeapStore::open(live.path(), Durability::Strict, 32).unwrap();
    h.set_segment_bytes(16 * 1024);
    h.set_log_archive(Some(archive.path().to_path_buf()));
    h.create_collection("c", Schema::dynamic()).unwrap();

    let mut rng = adabt_testkit::rng::Rng::new(0x5eed_9172);
    let mut model = std::collections::BTreeMap::new();
    let mut next_id = 0u64;
    for _ in 0..12 {
        step(&mut rng, &mut h, &mut model, &mut next_id, 0);
    }
    h.backup_to(backup.path()).unwrap();
    // The backup's checkpoint covers everything before its own log entry.
    let base = Lsn(last_lsn(live.path()).0 - 1);

    // From here on every write competes for two frames, and checkpoints move
    // segments into the archive, so the restore needs both sources of log.
    h.set_pool_capacity(2).unwrap();
    let evicted = h.buffer_stats().evictions;
    // The state after each operation, keyed by the LSN that completed it: the
    // write itself, or a group's commit record.
    let mut reference = vec![(base, model.clone())];
    for n in 1..=90u64 {
        step(&mut rng, &mut h, &mut model, &mut next_id, n);
        reference.push((last_lsn(live.path()), model.clone()));
        if n % 30 == 0 {
            h.checkpoint().unwrap();
        }
    }
    assert!(
        h.buffer_stats().evictions > evicted,
        "the workload must steal"
    );
    drop(h);
    assert!(
        std::fs::read_dir(archive.path()).unwrap().count() > 0,
        "the workload must archive segments"
    );
    let end = last_lsn(live.path());

    // Every LSN from the backup to the end, including each frame inside every
    // group and each checkpoint entry.
    for t in base.0..=end.0 {
        let dest = Tmp::new(&format!("sweep-{t}"));
        HeapStore::restore_from(backup.path(), dest.path()).unwrap();
        supply_segments(archive.path(), dest.path());
        supply_segments(&HeapStore::wal_path(live.path()), dest.path());
        let mut r = HeapStore::open_at(
            dest.path(),
            Durability::Strict,
            2,
            RecoverTarget::Lsn(Lsn(t)),
        )
        .unwrap_or_else(|e| panic!("restore to lsn {t}: {e}"));
        let want: Vec<(RecordId, Record)> = reference
            .iter()
            .rev()
            .find(|(lsn, _)| lsn.0 <= t)
            .map(|(_, state)| state.clone().into_iter().collect())
            .unwrap();
        let got = r.scan("c").unwrap();
        // Compared whole, reported as (id, generation): the padding would
        // drown the message.
        let summary = |rows: &[(RecordId, Record)]| {
            rows.iter()
                .map(|(id, r)| (id.0, r.get("gen").cloned()))
                .collect::<Vec<_>>()
        };
        assert!(
            got == want,
            "restore to lsn {t} is not the committed prefix:\n got {:?}\nwant {:?}",
            summary(&got),
            summary(&want)
        );
    }

    // Before the backup is out of reach, and said so.
    let dest = Tmp::new("sweep-early");
    HeapStore::restore_from(backup.path(), dest.path()).unwrap();
    match HeapStore::open_at(
        dest.path(),
        Durability::Strict,
        2,
        RecoverTarget::Lsn(Lsn(base.0 - 1)),
    ) {
        Err(adabt_core::error::Error::RestoreTargetUnreachable { .. }) => {}
        Err(e) => panic!("wrong error: {e}"),
        Ok(_) => panic!("restored to before the backup"),
    }
}
