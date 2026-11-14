//! A log that ends below a checkpoint never hands out an LSN the checkpoint
//! already covers.
//!
//! A checkpoint syncs the log through its own entry before it writes the
//! directory snapshot, the catalog or the base-image marker. So a log that
//! ends below one of those was damaged after the fact — a lost segment, a
//! write the device acknowledged and then dropped — not cut short by a crash.
//!
//! The log used to carry on from wherever it now ended. Every write made
//! after that was given an LSN recovery treats as already folded into the
//! checkpoint, and the next restart skipped it: committed writes, gone
//! without an error. These tests damage the log below a checkpoint, write,
//! restart, and demand every acknowledged write back — over a live directory,
//! a base image restored in place, and a restored backup — or, where the
//! damage destroyed something recovery cannot do without, a refusal.

use adabt_core::error::Error;
use adabt_core::ids::{Lsn, RecordId};
use adabt_core::policy::Durability;
use adabt_core::record::Record;
use adabt_core::schema::Schema;
use adabt_core::store::LogicalStore;
use adabt_storage::heap::{HeapStore, RecoverTarget};
use adabt_storage::wal::{Wal, WalOp};
use std::path::{Path, PathBuf};

struct Tmp(PathBuf);
impl Tmp {
    fn new(tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!(
            "adabt-log-behind-{tag}-{}-{:?}",
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
    Record::new().with("i", i).with("pad", "x".repeat(40))
}

fn open(dir: &Path) -> HeapStore {
    HeapStore::open(dir, Durability::Strict, 32)
        .unwrap_or_else(|e| panic!("{} refused to open: {e}", dir.display()))
}

fn insert(h: &mut HeapStore, ids: std::ops::Range<u64>) {
    for i in ids {
        h.insert("c", RecordId(i), rec(i)).unwrap();
    }
}

/// Every record in `c` is exactly `ids`, each with the contents its id names.
fn assert_holds(h: &mut HeapStore, ids: std::ops::Range<u64>, what: &str) {
    let got: Vec<u64> = h
        .scan("c")
        .unwrap()
        .into_iter()
        .map(|(id, _)| id.0)
        .collect();
    let want: Vec<u64> = ids.clone().collect();
    assert_eq!(got, want, "{what}: the wrong records came back");
    for i in ids {
        assert_eq!(
            h.get("c", RecordId(i)).unwrap(),
            Some(rec(i)),
            "{what}: {i}"
        );
    }
}

/// The LSN the newest checkpoint entry in `dir`'s log flushed through.
fn checkpointed_through(dir: &Path) -> Lsn {
    Wal::read_all(&HeapStore::wal_path(dir))
        .unwrap()
        .iter()
        .rev()
        .find_map(|e| match e.op {
            WalOp::Checkpoint { flushed_lsn } => Some(flushed_lsn),
            _ => None,
        })
        .expect("the log records a checkpoint")
}

/// The LSN of the newest entry `dir`'s log holds, or zero for none.
fn log_end(dir: &Path) -> u64 {
    Wal::read_all(&HeapStore::wal_path(dir))
        .unwrap()
        .last()
        .map_or(0, |e| e.lsn.0)
}

/// The log segments in `dir`, oldest first.
fn segments(dir: &Path) -> Vec<PathBuf> {
    let mut segs: Vec<PathBuf> = std::fs::read_dir(HeapStore::wal_path(dir))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    segs.sort();
    segs
}

/// Damage `dir`'s log so it ends below the checkpoint at `through`: every
/// segment after the oldest is removed, and the oldest is cut to
/// `keep_percent` of its length.
fn cut_log_below(dir: &Path, through: Lsn, keep_percent: u64) {
    let segs = segments(dir);
    for later in &segs[1..] {
        std::fs::remove_file(later).unwrap();
    }
    let oldest = &segs[0];
    let len = std::fs::metadata(oldest).unwrap().len();
    let f = std::fs::OpenOptions::new()
        .write(true)
        .open(oldest)
        .unwrap();
    f.set_len(len * keep_percent / 100).unwrap();
    f.sync_all().unwrap();
    assert!(
        log_end(dir) < through.0,
        "the fixture must leave the log ending below the checkpoint at lsn {}, \
         but it ends at lsn {}",
        through.0,
        log_end(dir)
    );
}

/// A database with ids `0..20` checkpointed. Returns the LSN the checkpoint
/// covers.
fn checkpointed(dir: &Path) -> Lsn {
    let mut h = open(dir);
    h.create_collection("c", Schema::dynamic()).unwrap();
    insert(&mut h, 0..20);
    h.checkpoint().unwrap();
    drop(h);
    checkpointed_through(dir)
}

#[test]
fn writes_after_a_log_cut_below_the_checkpoint_survive_the_next_restart() {
    // The reported loss, end to end. Ids 20..30 were acknowledged after the
    // damaged log was reopened, and used to come back missing.
    for keep_percent in [0, 30, 60, 90] {
        let t = Tmp::new(&format!("latest-{keep_percent}"));
        let through = checkpointed(t.path());
        cut_log_below(t.path(), through, keep_percent);

        let mut h = open(t.path());
        assert_holds(&mut h, 0..20, "the checkpoint's state");
        insert(&mut h, 20..30);
        drop(h);

        let entries = Wal::read_all(&HeapStore::wal_path(t.path())).unwrap();
        assert!(
            entries.iter().all(|e| e.lsn > through),
            "keep {keep_percent}%: an entry was given an lsn the checkpoint at {} covers: {:?}",
            through.0,
            entries.iter().map(|e| e.lsn.0).collect::<Vec<_>>()
        );
        for restart in ["first", "second"] {
            let mut h = open(t.path());
            assert_holds(
                &mut h,
                0..30,
                &format!("keep {keep_percent}%, {restart} restart"),
            );
        }
    }
}

#[test]
fn the_resumed_log_carries_on_from_just_after_the_checkpoint() {
    // `through + 1` exactly, not further: a restore from a base image at
    // `through` demands the log after it be contiguous.
    let t = Tmp::new("contiguous");
    let through = checkpointed(t.path());
    cut_log_below(t.path(), through, 50);

    let mut h = open(t.path());
    insert(&mut h, 20..21);
    drop(h);
    let entries = Wal::read_all(&HeapStore::wal_path(t.path())).unwrap();
    assert_eq!(
        entries.first().map(|e| e.lsn),
        Some(Lsn(through.0 + 1)),
        "the log no longer holds anything below the checkpoint, and resumes right after it"
    );
}

#[test]
fn the_damaged_segments_are_set_aside_rather_than_deleted() {
    let t = Tmp::new("set-aside");
    let through = checkpointed(t.path());
    cut_log_below(t.path(), through, 50);
    let damaged = std::fs::read(&segments(t.path())[0]).unwrap();

    drop(open(t.path()));
    let aside: Vec<PathBuf> = std::fs::read_dir(t.path().join("superseded-log"))
        .expect("the damaged log was set aside")
        .flat_map(|d| std::fs::read_dir(d.unwrap().path()).unwrap())
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(aside.len(), 1, "{aside:?}");
    let kept = std::fs::read(&aside[0]).unwrap();
    assert!(
        kept.starts_with(&damaged[..damaged.len().min(64)]),
        "the set-aside segment is the damaged one"
    );
}

#[test]
fn a_log_cut_below_the_checkpoint_spanning_several_segments_resumes_after_it() {
    let t = Tmp::new("segments");
    {
        let mut h = open(t.path());
        h.set_segment_bytes(1024);
        h.create_collection("c", Schema::dynamic()).unwrap();
        insert(&mut h, 0..60);
        h.checkpoint().unwrap();
        insert(&mut h, 60..80);
    }
    let through = checkpointed_through(t.path());
    assert!(segments(t.path()).len() > 2, "the fixture spans segments");
    cut_log_below(t.path(), through, 50);

    let mut h = open(t.path());
    h.set_segment_bytes(1024);
    // Ids 60..80 were after the checkpoint, and the damage took them: the
    // same loss as any other log that lost its tail. Nothing written from
    // here on may share their fate.
    assert_holds(&mut h, 0..60, "the checkpoint's state");
    insert(&mut h, 60..100);
    drop(h);
    for restart in ["first", "second"] {
        assert_holds(&mut open(t.path()), 0..100, restart);
    }
}

#[test]
fn a_resumed_log_survives_a_later_checkpoint_and_another_cut() {
    let t = Tmp::new("twice");
    let through = checkpointed(t.path());
    cut_log_below(t.path(), through, 50);
    {
        let mut h = open(t.path());
        insert(&mut h, 20..30);
        h.checkpoint().unwrap();
    }
    let second = checkpointed_through(t.path());
    assert!(second > through);
    cut_log_below(t.path(), second, 0);
    let mut h = open(t.path());
    insert(&mut h, 30..40);
    drop(h);
    assert_holds(&mut open(t.path()), 0..40, "after the second cut");
}

#[test]
fn a_resumed_log_no_longer_claims_to_reach_back_to_the_start() {
    // The resumed log holds nothing below the checkpoint, so it can no longer
    // rebuild the catalog on its own. A later open without the catalog must
    // refuse, not replay a log that lost its beginning.
    let t = Tmp::new("start");
    let through = checkpointed(t.path());
    cut_log_below(t.path(), through, 0);
    {
        let mut h = open(t.path());
        insert(&mut h, 20..30);
        h.checkpoint().unwrap();
    }
    std::fs::remove_file(adabt_storage::metadata::path(t.path())).unwrap();
    match HeapStore::open(t.path(), Durability::Strict, 32) {
        Err(Error::Corruption(_)) => {}
        Err(e) => panic!("wrong error: {e}"),
        Ok(_) => panic!("opened without its catalog over a log missing its beginning"),
    }
}

#[test]
fn a_cut_that_took_definitions_only_the_log_held_is_refused() {
    // The one case that cannot continue. A crash between a checkpoint's
    // directory write and its catalog write leaves a catalog older than the
    // directory, and the collection definitions in between only in the log.
    // With the log intact that is an ordinary recovery; with the log cut
    // below the directory, collection `d` exists only in the snapshot, under
    // an id nothing defines any more.
    let t = Tmp::new("refuse");
    let catalog = adabt_storage::metadata::path(t.path());
    let older = t.path().join("older-catalog");
    {
        let mut h = open(t.path());
        h.create_collection("c", Schema::dynamic()).unwrap();
        insert(&mut h, 0..10);
        h.checkpoint().unwrap();
        std::fs::copy(&catalog, &older).unwrap();
        h.create_collection("d", Schema::dynamic()).unwrap();
        h.insert("d", RecordId(0), rec(0)).unwrap();
        h.checkpoint().unwrap();
    }
    let through = checkpointed_through(t.path());
    std::fs::copy(&older, &catalog).unwrap();

    // Control: the same files over an intact log recover everything.
    {
        let mut h = open(t.path());
        assert_eq!(h.count("d").unwrap(), 1, "the control must recover `d`");
    }

    cut_log_below(t.path(), through, 0);
    match HeapStore::open(t.path(), Durability::Strict, 32) {
        Err(Error::Corruption(msg)) => assert!(
            msg.contains("catalog") && msg.contains(&through.0.to_string()),
            "the refusal should name what is inconsistent: {msg}"
        ),
        Err(e) => panic!("wrong error: {e}"),
        Ok(_) => panic!("opened a database whose cut log took collection definitions with it"),
    }
}

#[test]
fn a_base_image_restored_in_place_over_a_cut_log_never_reissues_its_lsn() {
    // `open_at` the base image's own LSN is the one restore a log ending
    // below it can still serve. What the restored database writes next must
    // land after the base image.
    let t = Tmp::new("pitr");
    let through = checkpointed(t.path());
    cut_log_below(t.path(), through, 50);

    let past = Lsn(through.0 + 1);
    match HeapStore::open_at(t.path(), Durability::Strict, 32, RecoverTarget::Lsn(past)) {
        Err(Error::InvalidRestore(_)) => {}
        Err(e) => panic!("wrong error: {e}"),
        Ok(_) => panic!("restored to lsn {} from a log that ends below it", past.0),
    }

    let mut h = HeapStore::open_at(
        t.path(),
        Durability::Strict,
        32,
        RecoverTarget::Lsn(through),
    )
    .unwrap();
    assert_holds(&mut h, 0..20, "the base image");
    insert(&mut h, 20..30);
    drop(h);
    for restart in ["first", "second"] {
        assert_holds(&mut open(t.path()), 0..30, restart);
    }
}

#[test]
fn a_restored_backup_whose_log_was_cut_below_its_checkpoint_keeps_new_writes() {
    let src = Tmp::new("backup-src");
    let backup = Tmp::new("backup");
    let dest = Tmp::new("backup-dest");
    {
        let mut h = open(src.path());
        h.create_collection("c", Schema::dynamic()).unwrap();
        insert(&mut h, 0..20);
        h.backup_to(backup.path()).unwrap();
    }
    HeapStore::restore_from(backup.path(), dest.path()).unwrap();
    let through = checkpointed_through(dest.path());
    cut_log_below(dest.path(), through, 0);

    let mut h = open(dest.path());
    assert_holds(&mut h, 0..20, "the backup");
    insert(&mut h, 20..30);
    drop(h);
    for restart in ["first", "second"] {
        assert_holds(&mut open(dest.path()), 0..30, restart);
    }

    // And through the restore path: a fresh copy of the same backup, cut the
    // same way, restored to its base image and written to.
    let again = Tmp::new("backup-again");
    HeapStore::restore_from(backup.path(), again.path()).unwrap();
    cut_log_below(again.path(), through, 0);
    let mut h = HeapStore::open_at(
        again.path(),
        Durability::Strict,
        32,
        RecoverTarget::Lsn(through),
    )
    .unwrap();
    insert(&mut h, 20..25);
    drop(h);
    assert_holds(&mut open(again.path()), 0..25, "restored, then reopened");
}
