//! Backup, restore, and point-in-time recovery, at the engine layer.
//!
//! `adabt-storage`'s own suite (`backup_restore.rs`) is the evidence that the
//! mechanism is correct at the byte level. This shows the engine adds the one
//! thing the storage layer cannot know about on its own — the
//! unique-constraint sidecar — that `open_at` produces a database exactly as
//! usable, indexes and all, as any other reopen, and that the supported
//! point-in-time procedure (restore a backup, supply the log, open at the
//! target) recovers exactly the committed prefix through the engine's own
//! write paths, transactions included.

use adabt_core::ids::{Lsn, RecordId};
use adabt_core::policy::Policy;
use adabt_core::record::Record;
use adabt_core::schema::{FieldDef, FieldType, Schema, SchemaMode};
use adabt_core::store::LogicalStore;
use adabt_engine::sharded::ShardedDatabase;
use adabt_engine::Database;
use adabt_index::IndexKind;
use adabt_storage::heap::{HeapStore, RecoverTarget};
use adabt_storage::wal::{Wal, WalOp};
use std::path::{Path, PathBuf};

struct Tmp(PathBuf);
impl Tmp {
    fn new(tag: &str) -> Self {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "adabt-engine-backup-{tag}-{}-{:?}",
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

fn schema() -> Schema {
    Schema::new(
        SchemaMode::Strict,
        vec![
            FieldDef::new("id", FieldType::U64).required(),
            FieldDef::new("name", FieldType::Str { max_len: Some(32) }),
        ],
    )
    .unwrap()
}

#[test]
fn a_database_backup_carries_unique_constraints_across() {
    let src = Tmp::new("uniq-src");
    let dest = Tmp::new("uniq-dest");
    let mut db = Database::open(src.path(), Policy::conventional()).unwrap();
    db.create_collection("users", schema()).unwrap();
    db.add_unique_constraint("users", "name").unwrap();
    db.insert(
        "users",
        RecordId(1),
        Record::new().with("id", 1u64).with("name", "ada"),
    )
    .unwrap();

    db.backup_to(dest.path()).unwrap();

    let restored = Database::open(dest.path(), Policy::conventional()).unwrap();
    assert!(restored.has_unique_constraint("users", "name"));
    assert_eq!(
        restored.unique_constraints(),
        vec![("users".to_string(), "name".to_string())]
    );
}

#[test]
fn a_database_backup_needs_no_sidecar_when_there_are_no_constraints() {
    let src = Tmp::new("nouniq-src");
    let dest = Tmp::new("nouniq-dest");
    let mut db = Database::open(src.path(), Policy::conventional()).unwrap();
    db.create_collection("users", schema()).unwrap();
    db.backup_to(dest.path()).unwrap();

    let restored = Database::open(dest.path(), Policy::conventional()).unwrap();
    assert!(restored.unique_constraints().is_empty());
}

#[test]
fn sharded_database_backup_produces_an_openable_copy() {
    let src = Tmp::new("shard-src");
    let dest = Tmp::new("shard-dest");
    let sdb = ShardedDatabase::open(src.path(), 3, Policy::conventional()).unwrap();
    sdb.create_collection("users", schema()).unwrap();
    for i in 0..30u64 {
        sdb.insert(
            "users",
            RecordId(i),
            Record::new().with("id", i).with("name", format!("u{i}")),
        )
        .unwrap();
    }

    sdb.backup_to(dest.path()).unwrap();

    let restored = ShardedDatabase::open(dest.path(), 3, Policy::conventional()).unwrap();
    for i in 0..30u64 {
        assert_eq!(
            restored.get("users", RecordId(i)).unwrap(),
            Some(Record::new().with("id", i).with("name", format!("u{i}")))
        );
    }
    // Untouched by the backup: the source keeps taking writes afterward.
    sdb.insert(
        "users",
        RecordId(30),
        Record::new().with("id", 30u64).with("name", "u30"),
    )
    .unwrap();
    assert_eq!(restored.get("users", RecordId(30)).unwrap(), None);
}

/// Copy every file in `from` into `dir`'s log directory, replacing a segment
/// of the same name — the operator's step of handing a restored backup the
/// log written after it.
fn supply_segments(from: &Path, dir: &Path) {
    let wal = HeapStore::wal_path(dir);
    for e in std::fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        if e.file_type().unwrap().is_file() {
            std::fs::copy(e.path(), wal.join(e.file_name())).unwrap();
        }
    }
}

/// The supported procedure end to end: restore `backup` into a fresh
/// directory, hand it the archived log and then the source's own, and open
/// it at `target`.
fn restore_at(
    backup: &Path,
    archive: &Path,
    live: &Path,
    tag: &str,
    target: Lsn,
) -> (Tmp, adabt_core::error::Result<Database>) {
    let dest = Tmp::new(tag);
    Database::restore_from(backup, dest.path()).unwrap();
    supply_segments(archive, dest.path());
    supply_segments(&HeapStore::wal_path(live), dest.path());
    let db = Database::open_at(
        dest.path(),
        Policy::conventional(),
        RecoverTarget::Lsn(target),
    );
    (dest, db)
}

/// The LSN of the insert of `id`, wherever in the live log it is.
fn insert_lsn(dir: &Path, id: u64) -> Lsn {
    Wal::read_all(&HeapStore::wal_path(dir))
        .unwrap()
        .iter()
        .find_map(|e| match &e.op {
            WalOp::Insert { id: i, .. } if i.0 == id => Some(e.lsn),
            _ => None,
        })
        .expect("the insert must be in the log")
}

fn user(i: u64) -> Record {
    Record::new().with("id", i).with("name", format!("u{i}"))
}

#[test]
fn archiving_the_log_is_what_lets_pitr_reach_past_a_backups_checkpoint() {
    // `backup_to` checkpoints first, and a checkpoint discards the segments it
    // folded into pages, so a bare backup can only ever be reopened at its own
    // checkpoint. With archiving on, those segments are kept, and a restore of
    // the backup can land on a moment *between* two later checkpoints.
    let live = Tmp::new("archive-live");
    let archive = Tmp::new("archive-dir");
    let backup = Tmp::new("archive-backup");
    std::fs::create_dir_all(archive.path()).unwrap();

    let mut db = Database::open(live.path(), Policy::conventional()).unwrap();
    db.set_log_archive(Some(archive.path().to_path_buf()));
    db.create_collection("users", schema()).unwrap();
    for i in 0..5u64 {
        db.insert("users", RecordId(i), user(i)).unwrap();
    }
    db.backup_to(backup.path()).unwrap();
    for i in 5..12u64 {
        db.insert("users", RecordId(i), user(i)).unwrap();
    }
    // A checkpoint here is what would discard segments without archiving.
    db.checkpoint().unwrap();
    drop(db);

    // Restore to just after id 8 — strictly between the backup and the
    // checkpoint after it.
    let target = insert_lsn(live.path(), 8);
    let (_dest, restored) = restore_at(
        backup.path(),
        archive.path(),
        live.path(),
        "archive-dest",
        target,
    );
    let mut restored = restored.unwrap();
    assert_eq!(restored.count("users").unwrap(), 9, "ids 0..=8");
    assert!(restored.get("users", RecordId(8)).unwrap().is_some());
    assert!(restored.get("users", RecordId(9)).unwrap().is_none());
}

/// Ten wide rows, a checkpoint (or a backup, which takes one), then twenty
/// more through a two-page pool, so the pool writes pages carrying them to
/// the heap file. Returns the LSN of the first of the twenty.
fn steal_after_checkpoint(dir: &Path, backup: Option<&Path>) -> Lsn {
    let wide = |i: u64| Record::new().with("v", i).with("pad", "x".repeat(1500));
    let mut db = Database::open(dir, Policy::conventional()).unwrap();
    db.create_collection("c", Schema::dynamic()).unwrap();
    for i in 0..10u64 {
        db.insert("c", RecordId(i), wide(i)).unwrap();
    }
    match backup {
        Some(b) => db.backup_to(b).unwrap(),
        None => db.checkpoint().unwrap(),
    }
    db.set_pool_capacity(2).unwrap();
    let evicted = db.buffer_stats().evictions;
    for i in 10..30u64 {
        db.insert("c", RecordId(i), wide(i)).unwrap();
    }
    assert!(
        db.buffer_stats().evictions > evicted,
        "the fixture must write pages to the heap file after the checkpoint"
    );
    insert_lsn(dir, 10)
}

#[test]
fn open_at_refuses_a_live_directory_whose_heap_holds_later_writes() {
    // The original failure: restoring this directory in place to the first of
    // the twenty inserts returned 20 rows where the target had 11, because
    // pages carrying later inserts were already in the heap file.
    let t = Tmp::new("refuse-live");
    let target = steal_after_checkpoint(t.path(), None);
    match Database::open_at(t.path(), Policy::conventional(), RecoverTarget::Lsn(target)) {
        Err(adabt_core::error::Error::InvalidRestore(msg)) => {
            assert!(msg.contains("not a base image"), "{msg}")
        }
        Err(e) => panic!("wrong error: {e}"),
        Ok(mut db) => panic!(
            "restored over a heap holding later writes: {} rows",
            db.count("c").unwrap()
        ),
    }
    // The refusal left the directory as it was.
    let mut db = Database::open(t.path(), Policy::conventional()).unwrap();
    assert_eq!(db.count("c").unwrap(), 30);
}

#[test]
fn a_restored_backup_reaches_exactly_the_target_however_much_the_source_stole() {
    let live = Tmp::new("steal-live");
    let archive = Tmp::new("steal-archive");
    let backup = Tmp::new("steal-backup");
    std::fs::create_dir_all(archive.path()).unwrap();
    let target = steal_after_checkpoint(live.path(), Some(backup.path()));

    let (_dest, restored) = restore_at(
        backup.path(),
        archive.path(),
        live.path(),
        "steal-dest",
        target,
    );
    let mut restored = restored.unwrap();
    assert_eq!(restored.count("c").unwrap(), 11, "ids 0..=10");
    assert!(restored.get("c", RecordId(10)).unwrap().is_some());
    assert!(restored.get("c", RecordId(11)).unwrap().is_none());
}

#[test]
fn pitr_through_the_engine_matches_a_reference_at_every_lsn() {
    // Autocommit writes and transactions — each with a delete — through a
    // two-page pool, restored from a backup at every LSN after it, against
    // the state a model of the same operations had at that point. A target
    // inside a transaction's frames must show none of it.
    let live = Tmp::new("sweep-live");
    let archive = Tmp::new("sweep-archive");
    let backup = Tmp::new("sweep-backup");
    std::fs::create_dir_all(archive.path()).unwrap();
    let wide = |i: u64, generation: u64| {
        Record::new()
            .with("v", i)
            .with("gen", generation)
            .with("pad", "x".repeat(1200))
    };
    let last_lsn = || {
        Wal::read_all(&HeapStore::wal_path(live.path()))
            .unwrap()
            .last()
            .unwrap()
            .lsn
    };

    let mut db = Database::open(live.path(), Policy::conventional()).unwrap();
    db.set_log_archive(Some(archive.path().to_path_buf()));
    db.create_collection("c", Schema::dynamic()).unwrap();
    let mut model = std::collections::BTreeMap::new();
    for i in 0..6u64 {
        db.insert("c", RecordId(i), wide(i, 0)).unwrap();
        model.insert(RecordId(i), wide(i, 0));
    }
    db.backup_to(backup.path()).unwrap();
    // The backup's checkpoint covers everything before its own log entry.
    let base = Lsn(last_lsn().0 - 1);
    db.set_pool_capacity(2).unwrap();
    let evicted = db.buffer_stats().evictions;

    let mut reference = vec![(base, model.clone())];
    let mut next = 6u64;
    for round in 1..=8u64 {
        // Autocommit: an insert, an update, a delete.
        db.insert("c", RecordId(next), wide(next, round)).unwrap();
        model.insert(RecordId(next), wide(next, round));
        next += 1;
        reference.push((last_lsn(), model.clone()));
        let victim = *model.keys().nth(round as usize % model.len()).unwrap();
        db.update("c", victim, wide(victim.0, round)).unwrap();
        model.insert(victim, wide(victim.0, round));
        reference.push((last_lsn(), model.clone()));
        let gone = *model.keys().next().unwrap();
        db.delete("c", gone).unwrap();
        model.remove(&gone);
        reference.push((last_lsn(), model.clone()));

        // A transaction: two inserts, an update and a delete, all or nothing.
        let mut txn = db.begin();
        let (a, b) = (RecordId(next), RecordId(next + 1));
        next += 2;
        let updated = *model.keys().last().unwrap();
        let deleted = *model.keys().next().unwrap();
        txn.insert(&mut db, "c", a, wide(a.0, round)).unwrap();
        txn.insert(&mut db, "c", b, wide(b.0, round)).unwrap();
        txn.update(&mut db, "c", updated, wide(updated.0, 100 + round))
            .unwrap();
        txn.delete(&mut db, "c", deleted).unwrap();
        db.commit(txn).unwrap();
        model.insert(a, wide(a.0, round));
        model.insert(b, wide(b.0, round));
        model.insert(updated, wide(updated.0, 100 + round));
        model.remove(&deleted);
        reference.push((last_lsn(), model.clone()));

        if round % 3 == 0 {
            db.checkpoint().unwrap();
        }
    }
    assert!(
        db.buffer_stats().evictions > evicted,
        "the workload must steal"
    );
    drop(db);
    let end = last_lsn();

    let mut inside_a_transaction = 0;
    let entries = Wal::read_all(&HeapStore::wal_path(live.path())).unwrap();
    for t in base.0..=end.0 {
        let (_dest, restored) = restore_at(
            backup.path(),
            archive.path(),
            live.path(),
            &format!("sweep-{t}"),
            Lsn(t),
        );
        let mut restored = restored.unwrap_or_else(|e| panic!("restore to lsn {t}: {e}"));
        let want: Vec<(RecordId, Record)> = reference
            .iter()
            .rev()
            .find(|(lsn, _)| lsn.0 <= t)
            .map(|(_, state)| state.clone().into_iter().collect())
            .unwrap();
        let got = restored.scan("c").unwrap();
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
        assert!(restored.verify().unwrap().problems.is_empty(), "lsn {t}");
        if entries
            .iter()
            .any(|e| e.lsn.0 == t && e.txn != adabt_core::ids::TxnId(0))
        {
            inside_a_transaction += 1;
        }
    }
    assert!(
        inside_a_transaction >= 8 * 4,
        "every frame of every transaction must have been a target"
    );
}

#[test]
fn open_at_restores_a_database_to_an_earlier_point_indexes_and_all() {
    let t = Tmp::new("pitr-engine");
    let archive = Tmp::new("pitr-engine-archive");
    let backup = Tmp::new("pitr-engine-backup");
    std::fs::create_dir_all(archive.path()).unwrap();
    let dir = t.path().to_path_buf();
    let mut db = Database::open(&dir, Policy::conventional()).unwrap();
    db.create_collection("users", schema()).unwrap();
    db.create_index("users", "name", IndexKind::Hash).unwrap();
    db.backup_to(backup.path()).unwrap();
    for i in 0..10u64 {
        db.insert(
            "users",
            RecordId(i),
            Record::new().with("id", i).with("name", format!("u{i}")),
        )
        .unwrap();
    }
    drop(db);

    let cutoff = insert_lsn(&dir, 4);
    let (_dest, restored) = restore_at(
        backup.path(),
        archive.path(),
        &dir,
        "pitr-engine-dest",
        cutoff,
    );
    let mut restored = restored.unwrap();
    assert_eq!(restored.count("users").unwrap(), 5);
    for i in 0..=4u64 {
        assert_eq!(
            restored.get("users", RecordId(i)).unwrap(),
            Some(Record::new().with("id", i).with("name", format!("u{i}")))
        );
    }
    for i in 5..10u64 {
        assert_eq!(restored.get("users", RecordId(i)).unwrap(), None);
    }

    // The index restored is a definition, not stale content: it must still
    // answer correctly over exactly the rows that made it through.
    let q = adabt_ir::plan::LogicalPlan::new(
        adabt_ir::plan::LogicalOp::scan("users").filter(adabt_ir::Expr::eq("name", "u2")),
    );
    let got = restored.query(&q).unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].0, RecordId(2));
}

#[test]
fn point_in_time_recovery_is_reachable_entirely_from_the_engine_api() {
    // The whole PITR story through `Database` alone, with no reach into
    // `adabt-storage`. Each of the three pieces below existed in the storage
    // crate and was exposed nowhere above it, which made the
    // "point-in-time recovery" true of the storage layer and not of anything
    // an application could call:
    //   - set_log_archive   (keeps the segments a checkpoint would discard)
    //   - lsn_at_or_before  (turns a wall-clock moment into an lsn)
    //   - open_at           (replays only that far)
    // A sweep for the same pattern found them; this test is what stops the
    // gap reopening. The only step outside the API is the operator's: copying
    // log segments into the restored directory.
    let live = Tmp::new("pitr-api-live");
    let archive = Tmp::new("pitr-api-archive");
    let backup = Tmp::new("pitr-api-backup");
    let dest = Tmp::new("pitr-api-dest");
    std::fs::create_dir_all(archive.path()).unwrap();

    let mut db = Database::open(live.path(), Policy::conventional()).unwrap();
    db.set_log_archive(Some(archive.path().to_path_buf()));
    db.create_collection("users", schema()).unwrap();
    for i in 0..5u64 {
        db.insert(
            "users",
            RecordId(i),
            Record::new().with("id", i).with("name", format!("u{i}")),
        )
        .unwrap();
    }
    db.backup_to(backup.path()).unwrap();

    // A moment in the middle, captured the way an operator would: by the
    // clock, not by an lsn they have no way to know.
    let moment = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64;
    std::thread::sleep(std::time::Duration::from_millis(5));

    for i in 5..12u64 {
        db.insert(
            "users",
            RecordId(i),
            Record::new().with("id", i).with("name", format!("u{i}")),
        )
        .unwrap();
    }
    drop(db);

    Database::restore_from(backup.path(), dest.path()).unwrap();
    supply_segments(archive.path(), dest.path());
    supply_segments(&HeapStore::wal_path(live.path()), dest.path());
    let target = Database::lsn_at_or_before(dest.path(), moment)
        .unwrap()
        .expect("a log entry at or before the captured moment must exist");
    let mut restored = Database::open_at(
        dest.path(),
        Policy::conventional(),
        RecoverTarget::Lsn(target),
    )
    .unwrap();

    // Everything before the moment, nothing after it.
    assert_eq!(restored.count("users").unwrap(), 5);
    assert!(restored.get("users", RecordId(4)).unwrap().is_some());
    assert!(restored.get("users", RecordId(5)).unwrap().is_none());
}

#[test]
fn restore_from_round_trips_through_the_engine_api_alone() {
    let src = Tmp::new("api-src");
    let backup = Tmp::new("api-backup");
    let dest = Tmp::new("api-dest");

    let mut db = Database::open(src.path(), Policy::conventional()).unwrap();
    db.create_collection("users", schema()).unwrap();
    db.add_unique_constraint("users", "name").unwrap();
    db.insert(
        "users",
        RecordId(1),
        Record::new().with("id", 1u64).with("name", "ada"),
    )
    .unwrap();
    db.backup_to(backup.path()).unwrap();
    drop(db);

    Database::restore_from(backup.path(), dest.path()).unwrap();
    let mut restored = Database::open(dest.path(), Policy::conventional()).unwrap();
    assert_eq!(restored.count("users").unwrap(), 1);
    // The engine-level sidecar came back too, not just the storage files.
    assert!(restored.has_unique_constraint("users", "name"));
}

#[test]
fn vacuum_is_reachable_from_the_engine_and_leaves_data_intact() {
    let t = Tmp::new("vacuum-api");
    let mut db = Database::open(t.path(), Policy::conventional()).unwrap();
    db.create_collection("users", schema()).unwrap();
    for i in 0..200u64 {
        db.insert(
            "users",
            RecordId(i),
            Record::new().with("id", i).with("name", format!("u{i}")),
        )
        .unwrap();
    }
    for i in 0..150u64 {
        db.delete("users", RecordId(i)).unwrap();
    }
    db.checkpoint().unwrap();

    let before = db.count("users").unwrap();
    db.vacuum().unwrap();
    assert_eq!(
        db.count("users").unwrap(),
        before,
        "vacuum changed what the database holds"
    );
    // And the surviving rows still read back correctly after being moved.
    for i in 150..200u64 {
        assert_eq!(
            db.get("users", RecordId(i)).unwrap().unwrap().get("name"),
            Some(&adabt_core::value::Value::Str(format!("u{i}")))
        );
    }
}
