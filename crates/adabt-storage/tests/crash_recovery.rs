//! Crash recovery returns exactly the committed state: no deleted record comes
//! back, and an overwritten record comes back once, as its newest version.
//!
//! A heap file is not self-describing. Beside its live records it holds the
//! slots of versions deleted or overwritten but not yet reclaimed, and copies
//! of writes the buffer pool stole to disk before a crash. An older recovery
//! rebuilt the page directory by scanning pages whenever the heap file had
//! grown since the last checkpoint, and so brought those slots back as records.
//!
//! A crash is what a killed process leaves behind: the files as they are on
//! disk while the store is still open — every page the pool has not written
//! is lost, every page it has written is kept. `crash_image` copies exactly
//! that. Every scenario then demands the same three things: the recovered
//! state equals a model of the committed writes, a second open sees the same
//! state, and a store that crashes again after recovering recovers again.

use adabt_core::ids::RecordId;
use adabt_core::policy::Durability;
use adabt_core::record::Record;
use adabt_core::schema::Schema;
use adabt_core::store::LogicalStore;
use adabt_storage::heap::{HeapStore, TxnWrite};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

struct Tmp(PathBuf);
impl Tmp {
    fn new(tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!(
            "adabt-crash-recovery-{tag}-{}-{:?}",
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
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &to);
        } else {
            std::fs::copy(entry.path(), &to).unwrap();
        }
    }
}

/// The disk a process killed right now would leave behind.
fn crash_image(live: &Path, tag: &str) -> Tmp {
    let image = Tmp::new(tag);
    copy_dir(live, image.path());
    image
}

/// A row whose contents say which write produced it. `pad` sets its size, so
/// a workload can make records grow, shrink and move between pages.
fn row(id: u64, generation: u64, pad: usize) -> Record {
    Record::new()
        .with("id", id)
        .with("generation", generation)
        .with("pad", "x".repeat(pad))
}

type Model = BTreeMap<u64, Record>;

/// Recover `dir` and demand exactly `model`, then do it again: a recovery
/// must leave nothing behind that changes the next one.
fn assert_recovers_to(dir: &Path, pool: usize, model: &Model, what: &str) {
    for open in ["first", "second"] {
        let mut h = HeapStore::open(dir, Durability::Strict, pool)
            .unwrap_or_else(|e| panic!("{what}: {open} open refused: {e}"));
        let got: Model = h
            .scan("c")
            .unwrap()
            .into_iter()
            .map(|(id, r)| (id.0, r))
            .collect();
        if &got != model {
            let back: Vec<u64> = got
                .keys()
                .copied()
                .filter(|k| !model.contains_key(k))
                .collect();
            let lost: Vec<u64> = model
                .keys()
                .copied()
                .filter(|k| !got.contains_key(k))
                .collect();
            let stale: Vec<u64> = model
                .iter()
                .filter(|(k, v)| got.get(k).is_some_and(|g| g != *v))
                .map(|(k, _)| *k)
                .collect();
            panic!(
                "{what}, {open} open: came back {back:?}, lost {lost:?}, wrong version {stale:?}"
            );
        }
        assert_eq!(h.count("c").unwrap(), model.len(), "{what}: count");
        for (id, rec) in model {
            assert_eq!(
                h.get("c", RecordId(*id)).unwrap().as_ref(),
                Some(rec),
                "{what}: get {id}"
            );
        }
    }
}

// -- the reported reproductions --------------------------------------------

#[test]
fn a_record_deleted_before_a_checkpoint_stays_deleted_once_the_heap_grows() {
    let live = Tmp::new("delete");
    let mut model = Model::new();
    let mut h = HeapStore::open(live.path(), Durability::Strict, 4).unwrap();
    h.create_collection("c", Schema::dynamic()).unwrap();
    for i in 0..10 {
        h.insert("c", RecordId(i), row(i, 0, 100)).unwrap();
        model.insert(i, row(i, 0, 100));
    }
    h.delete("c", RecordId(3)).unwrap();
    model.remove(&3);
    h.checkpoint().unwrap();
    let pages = h.page_count();
    for i in 10..200 {
        h.insert("c", RecordId(i), row(i, 0, 100)).unwrap();
        model.insert(i, row(i, 0, 100));
    }
    assert!(
        h.page_count() > pages,
        "the heap file must grow after the checkpoint"
    );
    let image = crash_image(live.path(), "delete-crash");
    drop(h);

    assert_eq!(model.len(), 199);
    assert_recovers_to(image.path(), 4, &model, "deleted before the checkpoint");
}

#[test]
fn an_overwritten_record_comes_back_once_as_its_newest_version() {
    // Two layouts: the new version after the old one on the page, and — by
    // freeing an earlier slot for it to reuse — before it. A page scan kept
    // whichever slot it met last, so the second layout came back as the old
    // version.
    for reuse_earlier_slot in [false, true] {
        let tag = format!("overwrite-{reuse_earlier_slot}");
        let live = Tmp::new(&tag);
        let mut model = Model::new();
        let mut h = HeapStore::open(live.path(), Durability::Strict, 4).unwrap();
        h.create_collection("c", Schema::dynamic()).unwrap();
        for i in 0..10 {
            h.insert("c", RecordId(i), row(i, 0, 100)).unwrap();
            model.insert(i, row(i, 0, 100));
        }
        if reuse_earlier_slot {
            h.delete("c", RecordId(1)).unwrap();
            model.remove(&1);
            h.reclaim().unwrap();
        }
        h.update("c", RecordId(3), row(3, 1, 100)).unwrap();
        model.insert(3, row(3, 1, 100));
        h.checkpoint().unwrap();
        for i in 10..200 {
            h.insert("c", RecordId(i), row(i, 0, 100)).unwrap();
            model.insert(i, row(i, 0, 100));
        }
        let image = crash_image(live.path(), &format!("{tag}-crash"));
        drop(h);
        assert_recovers_to(image.path(), 4, &model, &tag);
    }
}

// -- what the heap file can hold after a checkpoint ---------------------------

#[test]
fn a_slot_freed_after_the_checkpoint_is_never_freed_from_under_its_new_owner() {
    // The directory snapshot says record 3 lives in a slot. After the
    // checkpoint, 3 is deleted and its slot freed, and the page reaches the
    // heap file with that slot dead. Record 99 is written either before the
    // delete or after it, which decides who reuses the slot:
    //
    // - after: the live store hands the slot to 99, and the stolen page shows
    //   99 in it;
    // - before: the live store puts 99 elsewhere, but replay — which meets
    //   the dead slot before it reaches the delete — hands it to 99.
    //
    // Either way, replaying the delete of 3 must not free the slot 99 is in.
    for record_99_first in [false, true] {
        let tag = format!("reuse-{record_99_first}");
        let live = Tmp::new(&tag);
        let mut model = Model::new();
        let mut h = HeapStore::open(live.path(), Durability::Strict, 2).unwrap();
        h.create_collection("c", Schema::dynamic()).unwrap();
        for i in 0..10 {
            h.insert("c", RecordId(i), row(i, 0, 100)).unwrap();
            model.insert(i, row(i, 0, 100));
        }
        h.checkpoint().unwrap();
        if record_99_first {
            h.insert("c", RecordId(99), row(99, 0, 100)).unwrap();
        }
        h.delete("c", RecordId(3)).unwrap();
        model.remove(&3);
        h.reclaim().unwrap();
        if !record_99_first {
            h.insert("c", RecordId(99), row(99, 0, 100)).unwrap();
        }
        model.insert(99, row(99, 0, 100));
        // Rows too large for the first page's remaining space each take a page
        // of their own, until the two-page pool has written the first page.
        let written = h.pool_stats().writes;
        let mut i = 100;
        while h.pool_stats().writes < written + 4 {
            h.insert("c", RecordId(i), row(i, 0, 7_000)).unwrap();
            model.insert(i, row(i, 0, 7_000));
            i += 1;
        }
        let image = crash_image(live.path(), &format!("{tag}-crash"));
        drop(h);
        assert_recovers_to(image.path(), 2, &model, &tag);
    }
}

#[test]
fn versions_a_reader_held_across_a_checkpoint_do_not_come_back() {
    // A checkpoint cannot reclaim what an open snapshot still reads, so those
    // slots reach the heap file. They must not reach the recovered database.
    let live = Tmp::new("held");
    let mut model = Model::new();
    let mut h = HeapStore::open(live.path(), Durability::Strict, 4).unwrap();
    h.create_collection("c", Schema::dynamic()).unwrap();
    for i in 0..50 {
        h.insert("c", RecordId(i), row(i, 0, 100)).unwrap();
        model.insert(i, row(i, 0, 100));
    }
    let reader = h.snapshot();
    for i in 0..10 {
        h.delete("c", RecordId(i)).unwrap();
        model.remove(&i);
    }
    for i in 10..20 {
        h.update("c", RecordId(i), row(i, 1, 40)).unwrap();
        model.insert(i, row(i, 1, 40));
    }
    h.checkpoint().unwrap();
    assert_eq!(
        h.get_at("c", RecordId(0), &reader).unwrap(),
        Some(row(0, 0, 100)),
        "the checkpoint reclaimed a version an open snapshot still reads"
    );
    drop(reader);
    for i in 50..250 {
        h.insert("c", RecordId(i), row(i, 0, 100)).unwrap();
        model.insert(i, row(i, 0, 100));
    }
    let image = crash_image(live.path(), "held-crash");
    drop(h);
    assert_recovers_to(image.path(), 4, &model, "versions held across a checkpoint");

    // Nothing refers to the held versions' slots after the restart; vacuum is
    // what gives them back, and must not take anything else with them.
    let mut h = HeapStore::open(image.path(), Durability::Strict, 4).unwrap();
    h.vacuum().unwrap();
    drop(h);
    assert_recovers_to(image.path(), 4, &model, "vacuumed after recovery");
}

#[test]
fn a_vacuum_survives_a_crash_before_the_next_checkpoint() {
    // Vacuum moves records without changing them. Each move is logged, so the
    // directory snapshot from before the vacuum — which names the old places,
    // some on pages vacuum then truncated away — still recovers exactly.
    let live = Tmp::new("vacuum");
    let mut model = Model::new();
    let mut h = HeapStore::open(live.path(), Durability::Strict, 4).unwrap();
    h.create_collection("c", Schema::dynamic()).unwrap();
    for i in 0..400 {
        h.insert("c", RecordId(i), row(i, 0, 200)).unwrap();
        model.insert(i, row(i, 0, 200));
    }
    h.checkpoint().unwrap();
    for i in (0..400).filter(|i| i % 4 != 0) {
        h.delete("c", RecordId(i)).unwrap();
        model.remove(&i);
    }
    let before = h.page_count();
    assert!(h.vacuum().unwrap() > 0, "vacuum returned no pages");
    assert!(h.page_count() < before);
    let image = crash_image(live.path(), "vacuum-crash");

    // And growth again after the vacuum, still before any checkpoint.
    for i in 400..600 {
        h.insert("c", RecordId(i), row(i, 0, 200)).unwrap();
    }
    let regrown = crash_image(live.path(), "vacuum-regrown");
    drop(h);
    assert_recovers_to(image.path(), 4, &model, "crash after vacuum");
    for i in 400..600 {
        model.insert(i, row(i, 0, 200));
    }
    assert_recovers_to(regrown.path(), 4, &model, "crash after vacuum and regrowth");
}

// -- the model-checked matrix ---------------------------------------------------

/// A small deterministic generator, so a failure names a seed that replays it.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }
}

/// Drives a store and a model of what it should hold through the same writes.
struct Workload {
    rng: Rng,
    model: Model,
    next_id: u64,
    generation: u64,
}

impl Workload {
    fn pad(&mut self) -> usize {
        // Mostly small, sometimes large enough that a few fill a page.
        if self.rng.chance(10) {
            1_500 + self.rng.below(1_500) as usize
        } else {
            self.rng.below(300) as usize
        }
    }

    fn existing(&mut self) -> Option<u64> {
        if self.model.is_empty() {
            return None;
        }
        let n = self.rng.below(self.model.len() as u64) as usize;
        self.model.keys().nth(n).copied()
    }

    fn fresh(&mut self) -> (u64, Record) {
        let id = self.next_id;
        self.next_id += 1;
        self.generation += 1;
        let pad = self.pad();
        (id, row(id, self.generation, pad))
    }

    fn rewrite(&mut self, id: u64) -> Record {
        self.generation += 1;
        let pad = self.pad();
        row(id, self.generation, pad)
    }

    /// One step: a write, or a checkpoint, reclamation or vacuum. Returns a
    /// short name for failure messages.
    fn step(&mut self, h: &mut HeapStore) -> &'static str {
        match self.rng.below(100) {
            0..=29 => {
                let (id, rec) = self.fresh();
                if self.rng.chance(30) {
                    h.insert_keyed("c", RecordId(id), rec.clone(), (id % 50) as i64)
                        .unwrap();
                } else {
                    h.insert("c", RecordId(id), rec.clone()).unwrap();
                }
                self.model.insert(id, rec);
                "insert"
            }
            30..=49 => {
                let Some(id) = self.existing() else {
                    return "noop";
                };
                let rec = self.rewrite(id);
                h.update("c", RecordId(id), rec.clone()).unwrap();
                self.model.insert(id, rec);
                "update"
            }
            50..=64 => {
                let Some(id) = self.existing() else {
                    return "noop";
                };
                h.delete("c", RecordId(id)).unwrap();
                self.model.remove(&id);
                "delete"
            }
            65..=79 => {
                let mut writes = Vec::new();
                let mut seen = std::collections::BTreeSet::new();
                for _ in 0..1 + self.rng.below(6) {
                    let pick = if self.rng.chance(40) {
                        None
                    } else {
                        self.existing()
                    };
                    let (id, rec) = match pick {
                        Some(id) => (id, self.rewrite(id)),
                        None => self.fresh(),
                    };
                    if !seen.insert(id) {
                        continue;
                    }
                    if self.model.contains_key(&id) && self.rng.chance(40) {
                        writes.push(TxnWrite::Delete {
                            collection: "c".into(),
                            id: RecordId(id),
                        });
                        self.model.remove(&id);
                    } else {
                        writes.push(TxnWrite::Put {
                            collection: "c".into(),
                            id: RecordId(id),
                            record: rec.clone(),
                        });
                        self.model.insert(id, rec);
                    }
                }
                h.commit_writes(writes).unwrap();
                "transaction"
            }
            80..=86 => {
                let rows: Vec<(RecordId, Record)> = (0..1 + self.rng.below(20))
                    .map(|_| {
                        let (id, rec) = self.fresh();
                        self.model.insert(id, rec.clone());
                        (RecordId(id), rec)
                    })
                    .collect();
                h.insert_batch("c", rows).unwrap();
                "batch"
            }
            87..=92 => {
                h.reclaim().unwrap();
                "reclaim"
            }
            93..=98 => {
                h.checkpoint().unwrap();
                "checkpoint"
            }
            _ => {
                h.vacuum().unwrap();
                "vacuum"
            }
        }
    }
}

/// Run `steps` writes against a store with a `pool`-page buffer pool,
/// crashing it every few steps, and check every crash recovers exactly. Each
/// crash image is then written to further and crashed again, so recovery is
/// also checked starting from a state recovery produced.
fn run_matrix(seed: u64, durability: Durability, pool: usize, steps: usize) {
    let live = Tmp::new(&format!("matrix-{seed}-{durability:?}"));
    let mut h = HeapStore::open(live.path(), durability, pool).unwrap();
    h.create_collection("c", Schema::dynamic()).unwrap();
    let mut w = Workload {
        rng: Rng(seed | 1),
        model: Model::new(),
        next_id: 0,
        generation: 0,
    };
    // A reader held over a stretch of steps, so checkpoints sometimes flush
    // versions it keeps alive.
    let mut reader = None;
    let mut history: Vec<&'static str> = Vec::new();
    for step in 0..steps {
        history.push(w.step(&mut h));
        if w.rng.chance(5) {
            reader = match reader {
                Some(_) => None,
                None => Some(h.snapshot()),
            };
        }
        if step % 7 != 6 {
            continue;
        }
        let what = format!(
            "seed {seed}, {durability:?}, pool {pool}, crash after step {step} ({})",
            history[history.len().saturating_sub(4)..].join(", ")
        );
        let image = crash_image(live.path(), &format!("m{seed}-{step}"));
        assert_recovers_to(image.path(), pool, &w.model, &what);

        // Crash again after writing on top of the recovered state.
        if step % 21 == 20 {
            let mut again = HeapStore::open(image.path(), durability, pool).unwrap();
            let mut fork = Workload {
                rng: Rng(w.rng.next() | 1),
                model: w.model.clone(),
                next_id: w.next_id + 1_000_000,
                generation: w.generation + 1_000_000,
            };
            for _ in 0..10 {
                fork.step(&mut again);
            }
            let second = crash_image(image.path(), &format!("m{seed}-{step}-again"));
            drop(again);
            assert_recovers_to(
                second.path(),
                pool,
                &fork.model,
                &format!("{what}, then 10 more"),
            );
        }
    }
    drop(reader);
}

#[test]
fn every_crash_point_in_a_mixed_workload_recovers_the_committed_state() {
    for seed in [1, 2, 3] {
        run_matrix(seed, Durability::Strict, 3, 300);
    }
}

#[test]
fn every_crash_point_recovers_under_relaxed_durability_too() {
    // A process crash under `Relaxed` keeps whatever the log handed to the
    // operating system, which must be at least what any written page carries.
    for seed in [11, 12] {
        run_matrix(seed, Durability::Relaxed, 2, 300);
    }
}
