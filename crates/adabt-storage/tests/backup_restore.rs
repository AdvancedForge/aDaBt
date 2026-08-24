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
fn a_backup_omits_the_discardable_caches() {
    let src = Tmp::new("nocache-src");
    let dest = Tmp::new("nocache-dest");
    let mut h = HeapStore::open(src.path(), Durability::Strict, 32).unwrap();
    h.create_collection("c", Schema::dynamic()).unwrap();
    for i in 0..20 {
        h.insert("c", RecordId(i), rec(i)).unwrap();
    }
    h.checkpoint().unwrap(); // writes directory.adabt in the source
    h.backup_to(dest.path()).unwrap();

    assert!(!dest.path().join("directory.adabt").exists());
    assert!(!dest.path().join("derived.adabt").exists());
    assert!(dest.path().join("heap.adabt").exists());
    assert!(dest.path().join("catalog.adabt").exists());
    assert!(dest.path().join("superblock.adabt").exists());
    assert!(dest.path().join("wal").is_dir());
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

#[test]
fn open_at_a_target_lsn_replays_only_up_to_it() {
    let t = Tmp::new("pitr-lsn");
    let mut h = HeapStore::open(t.path(), Durability::Strict, 32).unwrap();
    h.create_collection("c", Schema::dynamic()).unwrap();
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

    let mut restored =
        HeapStore::open_at(t.path(), Durability::Strict, 32, RecoverTarget::Lsn(cutoff)).unwrap();
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
    let mut h = HeapStore::open(t.path(), Durability::Strict, 32).unwrap();
    h.create_collection("c", Schema::dynamic()).unwrap();
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
    let mut restored =
        HeapStore::open_at(t.path(), Durability::Strict, 32, RecoverTarget::Lsn(lsn)).unwrap();
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
