//! Backup, restore, and point-in-time recovery, at the engine layer.
//!
//! `adabt-storage`'s own suite (`backup_restore.rs`) is the evidence that the
//! mechanism is correct at the byte level. This only has to show the engine
//! adds the one thing the storage layer cannot know about on its own — the
//! unique-constraint sidecar — and that `open_at` produces a database exactly
//! as usable, indexes and all, as any other reopen.

use adabt_core::ids::RecordId;
use adabt_core::policy::Policy;
use adabt_core::record::Record;
use adabt_core::schema::{FieldDef, FieldType, Schema, SchemaMode};
use adabt_core::store::LogicalStore;
use adabt_engine::sharded::ShardedDatabase;
use adabt_engine::Database;
use adabt_index::IndexKind;
use adabt_storage::heap::RecoverTarget;
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

#[test]
fn archiving_the_log_is_what_lets_pitr_reach_past_a_backups_checkpoint() {
    // The end-to-end property M22 claimed and could not actually deliver from
    // the engine API: `backup_to` checkpoints first, and a checkpoint discards
    // the segments it folded into pages, so a bare backup can only ever be
    // reopened at its own checkpoint. With archiving on, those segments are
    // kept, and a restore can land on a moment *between* two checkpoints.
    //
    // The archive mechanism lived in `adabt-storage` from M17 but was
    // reachable from nowhere above it until an audit caught that
    // `Database`/`ShardedDatabase` never exposed it. This test exists so that
    // gap cannot silently reopen.
    let live = Tmp::new("archive-live");
    let archive = Tmp::new("archive-dir");
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
    // A checkpoint here is what would discard segments without archiving.
    db.checkpoint().unwrap();
    for i in 5..12u64 {
        db.insert(
            "users",
            RecordId(i),
            Record::new().with("id", i).with("name", format!("u{i}")),
        )
        .unwrap();
    }
    drop(db);

    // Restore to just after id 8 — a point strictly after the checkpoint,
    // which is the case a bare backup cannot express.
    let dir = live.path().to_path_buf();
    let entries = Wal::read_all(&adabt_storage::heap::HeapStore::wal_path(&dir)).unwrap();
    let target = entries
        .iter()
        .find_map(|e| match &e.op {
            WalOp::Insert { id, .. } if id.0 == 8 => Some(e.lsn),
            _ => None,
        })
        .expect("the insert of id 8 must be in the log");

    let mut restored =
        Database::open_at(&dir, Policy::conventional(), RecoverTarget::Lsn(target)).unwrap();
    assert_eq!(restored.count("users").unwrap(), 9, "ids 0..=8");
    assert!(restored.get("users", RecordId(8)).unwrap().is_some());
    assert!(restored.get("users", RecordId(9)).unwrap().is_none());
}

#[test]
fn open_at_restores_a_database_to_an_earlier_point_indexes_and_all() {
    let t = Tmp::new("pitr-engine");
    let dir = t.path().to_path_buf();
    let mut db = Database::open(&dir, Policy::conventional()).unwrap();
    db.create_collection("users", schema()).unwrap();
    db.create_index("users", "name", IndexKind::Hash).unwrap();
    for i in 0..10u64 {
        db.insert(
            "users",
            RecordId(i),
            Record::new().with("id", i).with("name", format!("u{i}")),
        )
        .unwrap();
    }
    drop(db);

    let entries = Wal::read_all(&adabt_storage::heap::HeapStore::wal_path(&dir)).unwrap();
    let cutoff = entries
        .iter()
        .find_map(|e| match &e.op {
            WalOp::Insert { id, .. } if id.0 == 4 => Some(e.lsn),
            _ => None,
        })
        .unwrap();

    let mut restored =
        Database::open_at(&dir, Policy::conventional(), RecoverTarget::Lsn(cutoff)).unwrap();
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
    // crate and was exposed nowhere above it, which made M22's
    // "point-in-time recovery" true of the storage layer and not of anything
    // an application could call:
    //   - set_log_archive   (keeps the segments a checkpoint would discard)
    //   - lsn_at_or_before  (turns a wall-clock moment into an lsn)
    //   - open_at           (replays only that far)
    // A sweep for the same pattern found them; this test is what stops the
    // gap reopening.
    let live = Tmp::new("pitr-api-live");
    let archive = Tmp::new("pitr-api-archive");
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
    db.checkpoint().unwrap();

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

    let target = Database::lsn_at_or_before(live.path(), moment)
        .unwrap()
        .expect("a log entry at or before the captured moment must exist");
    let mut restored = Database::open_at(
        live.path(),
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
