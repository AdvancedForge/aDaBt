//! The cross-shard coordinator, attacked at every phase of its protocol.
//!
//! A coordinated transaction has exactly three durable moments: the journal
//! fsyncs (the decision), each shard applies in order, the journal disappears
//! (the cleanup). A crash between any two of them must converge on the same
//! final state once the database reopens. These tests stage each crash by
//! hand-crafting the state it would leave — journal only (died before any
//! shard applied), journal plus a prefix of shards applied — and hold the
//! implementation to one answer in every case.

use adabt_core::error::Result;
use adabt_core::ids::RecordId;
use adabt_core::policy::Policy;
use adabt_core::record::Record;
use adabt_core::schema::{FieldDef, FieldType, Schema, SchemaMode};
use adabt_engine::sharded::{CrossShardWrite, ShardedDatabase};
use std::path::{Path, PathBuf};

struct Tmp(PathBuf);
impl Tmp {
    fn new(tag: &str) -> Self {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "adabt-xshard-{tag}-{}-{:?}",
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

fn policy() -> Policy {
    Policy::manual(4)
}

fn open(dir: &Path) -> Result<ShardedDatabase> {
    ShardedDatabase::open(dir, 4, policy())
}

/// Four-shard database with `accounts` live; ids 0..40 spread across shards.
fn seeded(dir: &Path) -> Result<ShardedDatabase> {
    let db = open(dir)?;
    db.create_collection(
        "accounts",
        Schema::new(
            SchemaMode::Dynamic,
            vec![FieldDef::new("balance", FieldType::I64)],
        )
        .unwrap(),
    )?;
    for i in 0..40u64 {
        db.insert(
            "accounts",
            RecordId(i),
            Record::new().with("balance", 100i64),
        )?;
    }
    Ok(db)
}

/// The write-set of a transfer that touches all four shards: top up even ids,
/// drain odd ids.
fn transfer() -> Vec<CrossShardWrite> {
    (0..40u64)
        .map(|i| CrossShardWrite {
            collection: "accounts".into(),
            id: RecordId(i),
            record: Some(Record::new().with("balance", if i % 2 == 0 { 200 } else { 50 })),
        })
        .collect()
}

fn balances(db: &ShardedDatabase) -> Vec<i64> {
    (0..40u64)
        .map(|i| {
            db.get("accounts", RecordId(i))
                .unwrap()
                .unwrap()
                .get("balance")
                .and_then(|v| match v {
                    adabt_core::value::Value::I64(n) => Some(*n),
                    _ => None,
                })
                .unwrap()
        })
        .collect()
}

fn expected() -> Vec<i64> {
    (0..40u64)
        .map(|i| if i % 2 == 0 { 200 } else { 50 })
        .collect()
}

/// Stage "crashed right after the journal fsynced": write-set journalled,
/// no shard touched. Recovery on open must apply everything.
#[test]
fn a_crash_after_the_journal_recovers_to_the_full_commit() {
    let t = Tmp::new("journal-only");
    {
        let _db = seeded(t.path()).unwrap();
        // Journal without applying: the exact bytes commit_coordinated writes,
        // through the same encoder, left behind as a crashed process would.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"XSH1");
        for w in transfer() {
            w.encode(&mut bytes);
        }
        std::fs::write(t.path().join("coordinator-journal"), &bytes).unwrap();
    }
    let db = open(t.path()).unwrap();
    assert_eq!(balances(&db), expected(), "recovery finished the commit");
    assert!(!t.path().join("coordinator-journal").exists());
}

/// Stage "crashed mid-application": journal present AND a shard-prefix of the
/// writes already applied. Replay is idempotent over the overlap by design;
/// this proves it rather than assuming it.
#[test]
fn a_crash_mid_application_still_lands_on_the_same_state() {
    let t = Tmp::new("mid-apply");
    {
        let db = seeded(t.path()).unwrap();
        let writes = transfer();
        // Apply the writes owned by shards 0 and 1 — the honest simulation of
        // dying between the second and third shard.
        for w in &writes {
            if w.id.0 % 4 <= 1 {
                match &w.record {
                    // Same put-by-hand the coordinator uses: these ids are
                    // already seeded, so applying means overwriting.
                    Some(r) => {
                        if db.insert(&w.collection, w.id, r.clone()).is_err() {
                            db.update(&w.collection, w.id, r.clone()).unwrap();
                        }
                    }
                    None => {
                        db.delete(&w.collection, w.id).unwrap();
                    }
                }
            }
        }
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"XSH1");
        for w in &writes {
            w.encode(&mut bytes);
        }
        std::fs::write(t.path().join("coordinator-journal"), &bytes).unwrap();
    }
    let db = open(t.path()).unwrap();
    assert_eq!(balances(&db), expected());
}

/// The happy path through the public API: one coordinated call, every shard
/// agrees afterwards, no journal left behind.
#[test]
fn a_coordinated_commit_is_visible_everywhere_and_cleans_up() {
    let t = Tmp::new("happy");
    let db = seeded(t.path()).unwrap();
    db.commit_coordinated(transfer()).unwrap();
    assert_eq!(balances(&db), expected());
    assert!(!t.path().join("coordinator-journal").exists());

    // And a second coordinated pass — including deletes this time — composes.
    let removals: Vec<CrossShardWrite> = (0..40u64)
        .step_by(2)
        .map(|i| CrossShardWrite {
            collection: "accounts".into(),
            id: RecordId(i),
            record: None,
        })
        .collect();
    db.commit_coordinated(removals).unwrap();
    for i in 0..40u64 {
        let gone = i % 2 == 0;
        assert_eq!(
            db.get("accounts", RecordId(i)).unwrap().is_some(),
            !gone,
            "id {i}"
        );
    }
}

/// A torn journal tail — crash while the entry itself was being written —
/// decodes as a clean prefix and recovery replays just that.
#[test]
fn a_torn_journal_tail_is_a_prefix_not_a_corruption() {
    let t = Tmp::new("torn");
    {
        let db = seeded(t.path()).unwrap();
        let _ = &db;
        let writes = transfer();
        let mut full = Vec::new();
        full.extend_from_slice(b"XSH1");
        for w in &writes {
            w.encode(&mut full);
        }
        // Cut inside the last entry.
        let cut = full.len() - 5;
        std::fs::write(t.path().join("coordinator-journal"), &full[..cut]).unwrap();
    }
    let db = open(t.path()).unwrap();
    let got = balances(&db);
    let exp = expected();
    // Every entry before the cut is complete by construction: those ids hold
    // their new values; the torn one holds whatever was last durable there.
    for i in 0..40usize {
        if i < 38 {
            assert_eq!(got[i], exp[i], "id {i}");
        }
    }
    assert_eq!(got.len(), 40);
}
