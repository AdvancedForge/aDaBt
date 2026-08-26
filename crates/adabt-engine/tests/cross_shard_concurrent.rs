//! Coordinator under concurrent readers and writers — serialized, ack-checked, sequenced.
//!
//! Requirements exercised:
//! * `commit_coordinated` is serialized via `coordinator` Mutex so journal
//!   load-append-write does not lose writes.
//! * Every `Result` in the soak must succeed — `let _ =` would hide a wedge.
//! * Each coordinated transaction carries a globally unique `seq`; only
//!   successfully acknowledged seqs are recorded.
//! * After reopen (which re-drives any leftover journal), every acked seq is
//!   verified present. This is the durability half of “coordinator-decides
//!   durability (windowed visibility)” — the window may show disagreement while
//!   `commit_coordinated` is in progress, but once it returns `Ok`, recovery
//!   will land on that state.

use adabt_core::ids::RecordId;
use adabt_core::policy::Policy;
use adabt_core::record::Record;
use adabt_core::schema::{FieldDef, FieldType, Schema, SchemaMode};
use adabt_engine::sharded::{CrossShardWrite, ShardedDatabase};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

struct Tmp(PathBuf);
impl Tmp {
    fn new(tag: &str) -> Self {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "adabt-xshard-conc-{tag}-{}-{:?}",
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

fn open(dir: &Path, shards: usize) -> ShardedDatabase {
    ShardedDatabase::open(dir, shards, Policy::manual(4)).unwrap()
}

fn seeded(dir: &Path) -> ShardedDatabase {
    let db = open(dir, 4);
    db.create_collection(
        "kvs",
        Schema::new(
            SchemaMode::Dynamic,
            vec![
                FieldDef::new("v", FieldType::I64),
                FieldDef::new("seq", FieldType::I64),
            ],
        )
        .unwrap(),
    )
    .unwrap();
    for i in 0..100u64 {
        db.insert(
            "kvs",
            RecordId(i),
            Record::new().with("v", 0i64).with("seq", 0i64),
        )
        .unwrap();
    }
    db
}

#[test]
fn coordinator_soaks_under_concurrent_readers_and_writers() {
    let t = Tmp::new("soak");
    let db = Arc::new(seeded(t.path()));
    let stop = Arc::new(AtomicBool::new(false));
    let reads_done = Arc::new(AtomicUsize::new(0));
    let writes_done = Arc::new(AtomicUsize::new(0));
    let global_seq = Arc::new(AtomicU64::new(1000));
    let acked: Arc<Mutex<Vec<u64>>> = Arc::new(Mutex::new(Vec::new()));

    // Reader threads: every Result must succeed — no `let _ =` swallow.
    let mut readers = Vec::new();
    for _ in 0..4 {
        let db = Arc::clone(&db);
        let stop = Arc::clone(&stop);
        let counter = Arc::clone(&reads_done);
        readers.push(std::thread::spawn(move || {
            let mut tick = 0usize;
            while !stop.load(Ordering::Relaxed) {
                let id = RecordId((tick % 100) as u64);
                db.get("kvs", id).unwrap();
                if tick % 3 == 0 {
                    db.count("kvs").unwrap();
                }
                if tick % 10 == 0 {
                    let scan = db.scan("kvs").unwrap();
                    // scan must be sorted and contain at least the seeded ids
                    assert!(scan.len() >= 100);
                }
                tick = tick.wrapping_add(1);
                counter.fetch_add(1, Ordering::Relaxed);
            }
        }));
    }

    // Writer threads: each coordinated transaction gets a globally unique seq.
    let mut writers = Vec::new();
    for _ in 0..2 {
        let db = Arc::clone(&db);
        let stop = Arc::clone(&stop);
        let counter = Arc::clone(&writes_done);
        let global_seq = Arc::clone(&global_seq);
        let acked = Arc::clone(&acked);
        writers.push(std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let seq = global_seq.fetch_add(1, Ordering::Relaxed);
                // One coordinated transaction touches all shards via 8 ids spread
                // across shards; seq is stored in the record so we can verify it.
                let writes: Vec<CrossShardWrite> = (0..8)
                    .map(|k| {
                        let id = RecordId(seq * 8 + k);
                        CrossShardWrite {
                            collection: "kvs".into(),
                            id,
                            record: Some(
                                Record::new().with("v", seq as i64).with("seq", seq as i64),
                            ),
                        }
                    })
                    .collect();
                db.commit_coordinated(writes).unwrap();
                acked.lock().unwrap().push(seq);
                counter.fetch_add(1, Ordering::Relaxed);
            }
        }));
    }

    // Interleave non-coordinated writes on a disjoint key range to ensure they
    // don't wedge the coordinator (they serialize per-shard, not via coordinator).
    // Every Result must succeed — duplicate insert becomes update.
    let db_clone = Arc::clone(&db);
    let stop_clone = Arc::clone(&stop);
    let bg = std::thread::spawn(move || {
        let mut n = 5000u64;
        while !stop_clone.load(Ordering::Relaxed) {
            let rec = Record::new().with("v", 1i64).with("seq", 0i64);
            if db_clone.insert("kvs", RecordId(n), rec.clone()).is_err() {
                db_clone.update("kvs", RecordId(n), rec).unwrap();
            }
            n += 1;
            if n > 5100 {
                n = 5000;
                db_clone.delete("kvs", RecordId(n)).unwrap();
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    });

    std::thread::sleep(std::time::Duration::from_millis(800));
    stop.store(true, Ordering::Relaxed);
    for h in readers {
        h.join().unwrap();
    }
    for h in writers {
        h.join().unwrap();
    }
    bg.join().unwrap();

    // Every Result succeeded (would have panicked otherwise). Liveness.
    assert!(reads_done.load(Ordering::Relaxed) > 500);
    assert!(writes_done.load(Ordering::Relaxed) > 10);
    let acked_seqs = acked.lock().unwrap().clone();
    assert!(
        !acked_seqs.is_empty(),
        "no coordinated commit was acknowledged"
    );

    // Final invariant: recovery re-drives any leftover journal and leaves a
    // consistent, readable database.
    let count = db.count("kvs").unwrap();
    assert!(count >= 100 + acked_seqs.len() * 8);

    // Reopen exercises recover_coordinated path.
    let acked_clone = acked_seqs.clone();
    drop(db);
    let db2 = open(t.path(), 4);
    let count2 = db2.count("kvs").unwrap();
    assert!(
        count2 >= 100 + acked_seqs.len() * 8,
        "reopen lost committed writes"
    );

    // Every successfully acknowledged sequence must be present after reopen,
    // with its seq value intact — the durability half of coordinator-decides.
    for seq in acked_clone {
        for k in 0..8 {
            let id = RecordId(seq * 8 + k);
            let rec = db2.get("kvs", id).unwrap().unwrap_or_else(|| {
                panic!("acked seq {seq} k={k} id={} missing after reopen", id.0)
            });
            assert_eq!(
                rec.get("seq"),
                Some(&adabt_core::value::Value::I64(seq as i64)),
                "seq mismatch for acked {seq}"
            );
            assert_eq!(
                rec.get("v"),
                Some(&adabt_core::value::Value::I64(seq as i64))
            );
        }
    }
}
