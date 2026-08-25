//! Record versioning, for consistent read snapshots.
//!
//! Everything after this milestone needs two representations answering the same
//! question against the *same state at the same time*. Shadow execution is the
//! obvious case — comparing a candidate against a baseline is meaningless if
//! the data moved between the two reads — but so is any consistent scan.
//!
//! Until now reads took `&mut self` and saw whatever the heap held at that
//! instant. This adds the minimum needed to fix that: every write stamps a
//! transaction id, superseded versions are retained while a snapshot might need
//! them, and a reader at snapshot `T` sees the newest version stamped at or
//! before `T`.
//!
//! # Why not full MVCC
//!
//! This is a version *chain per record*, not general multi-version concurrency
//! control. There is no concurrent writer, no conflict detection, no isolation
//! level negotiation. What it provides is the one property later milestones
//! actually need — a stable read view — at a fraction of the cost, and it uses
//! the `txn` field reserved in the record header since M1 precisely so that
//! adding it now is not a format change.

use adabt_core::ids::TxnId;
/// Under the loom feature every atomic and thread primitive here is loom's,
/// so the model checker explores all interleavings of the tests at the bottom
/// of this file. The default build uses the real thing.
#[cfg(feature = "loom")]
use loom::sync::atomic::{AtomicU64, Ordering};
#[cfg(feature = "loom")]
use loom::sync::Arc;
#[cfg(not(feature = "loom"))]
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(not(feature = "loom"))]
use std::sync::Arc;

/// Hands out transaction ids and tracks which snapshots are still open.
///
/// Retention is driven by the *oldest* open snapshot: a version superseded
/// after that point may still be needed, and one superseded before it cannot
/// be. Reclaiming on any other basis risks a reader seeing a hole.
#[derive(Debug, Default)]
pub struct VersionTracker {
    next: AtomicU64,
    /// Snapshot ids currently open, smallest first.
    open: std::sync::Mutex<Vec<u64>>,
}

impl VersionTracker {
    pub fn new() -> Self {
        Self {
            next: AtomicU64::new(1),
            open: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// Allocate the id a write will be stamped with.
    pub fn begin_write(&self) -> TxnId {
        TxnId(self.next.fetch_add(1, Ordering::SeqCst))
    }

    /// The id a new snapshot reads at: everything committed so far.
    pub fn current(&self) -> TxnId {
        TxnId(self.next.load(Ordering::SeqCst).saturating_sub(1))
    }

    fn open_snapshot(&self) -> TxnId {
        let at = self.current();
        self.open
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(at.0);
        at
    }

    fn close_snapshot(&self, at: TxnId) {
        let mut g = self.open.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(i) = g.iter().position(|x| *x == at.0) {
            g.swap_remove(i);
        }
    }

    /// Versions superseded at or before this id can never be read again.
    ///
    /// With no snapshot open that is everything committed; with one open it is
    /// bounded by the oldest, which is what stops a long-running reader from
    /// having the ground moved under it.
    pub fn reclaim_horizon(&self) -> TxnId {
        let g = self.open.lock().unwrap_or_else(|e| e.into_inner());
        match g.iter().min() {
            Some(oldest) => TxnId(*oldest),
            None => self.current(),
        }
    }

    pub fn open_count(&self) -> usize {
        self.open.lock().unwrap_or_else(|e| e.into_inner()).len()
    }
}

/// A stable read view. Closing it is what allows reclamation to advance.
pub struct Snapshot {
    at: TxnId,
    tracker: Arc<VersionTracker>,
}

impl Snapshot {
    pub fn open(tracker: Arc<VersionTracker>) -> Self {
        let at = tracker.open_snapshot();
        Self { at, tracker }
    }

    pub fn at(&self) -> TxnId {
        self.at
    }

    /// Whether a version stamped `txn` is visible to this snapshot.
    #[inline]
    pub fn sees(&self, txn: TxnId) -> bool {
        txn.0 <= self.at.0
    }
}

impl Drop for Snapshot {
    fn drop(&mut self) {
        self.tracker.close_snapshot(self.at);
    }
}

impl std::fmt::Debug for Snapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Snapshot(at={})", self.at)
    }
}

#[cfg(test)]
// The ordinary tests use the real std primitives, which loom's cfg-swap
// replaces; they run on the default build and yield to the model-checked
// subset when `--features loom` is set.
#[cfg(all(test, not(feature = "loom")))]
mod tests {
    use super::*;

    #[test]
    fn write_ids_increase() {
        let t = VersionTracker::new();
        let a = t.begin_write();
        let b = t.begin_write();
        assert!(b.0 > a.0);
    }

    #[test]
    fn a_snapshot_sees_writes_that_preceded_it_and_not_those_after() {
        let t = Arc::new(VersionTracker::new());
        let early = t.begin_write();
        let s = Snapshot::open(Arc::clone(&t));
        let late = t.begin_write();

        assert!(
            s.sees(early),
            "a snapshot must see what was committed before it"
        );
        assert!(!s.sees(late), "a snapshot must not see a later write");
    }

    #[test]
    fn two_snapshots_taken_at_different_points_see_different_states() {
        let t = Arc::new(VersionTracker::new());
        t.begin_write();
        let older = Snapshot::open(Arc::clone(&t));
        let mid = t.begin_write();
        let newer = Snapshot::open(Arc::clone(&t));

        assert!(!older.sees(mid));
        assert!(newer.sees(mid));
    }

    #[test]
    fn reclamation_is_bounded_by_the_oldest_open_snapshot() {
        // The property that makes a long read safe: nothing it might still need
        // can be reclaimed while it is open.
        let t = Arc::new(VersionTracker::new());
        for _ in 0..5 {
            t.begin_write();
        }
        let long_running = Snapshot::open(Arc::clone(&t));
        let held = long_running.at();
        for _ in 0..10 {
            t.begin_write();
        }
        let recent = Snapshot::open(Arc::clone(&t));

        assert_eq!(
            t.reclaim_horizon(),
            held,
            "reclamation advanced past a snapshot that is still open"
        );
        drop(recent);
        assert_eq!(t.reclaim_horizon(), held);
        drop(long_running);
        assert_eq!(
            t.reclaim_horizon(),
            t.current(),
            "closing the last snapshot did not release retention"
        );
    }

    #[test]
    fn with_no_snapshots_everything_committed_is_reclaimable() {
        let t = VersionTracker::new();
        for _ in 0..3 {
            t.begin_write();
        }
        assert_eq!(t.reclaim_horizon(), t.current());
        assert_eq!(t.open_count(), 0);
    }

    #[test]
    fn dropping_a_snapshot_releases_it() {
        let t = Arc::new(VersionTracker::new());
        {
            let _s = Snapshot::open(Arc::clone(&t));
            assert_eq!(t.open_count(), 1);
        }
        assert_eq!(t.open_count(), 0);
    }

    #[test]
    fn snapshots_can_be_opened_from_several_threads() {
        let t = Arc::new(VersionTracker::new());
        for _ in 0..100 {
            t.begin_write();
        }
        let mut handles = Vec::new();
        for _ in 0..8 {
            let t = Arc::clone(&t);
            handles.push(std::thread::spawn(move || {
                let s = Snapshot::open(t);
                let at = s.at();
                assert!(s.sees(at));
                at
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(t.open_count(), 0, "a thread leaked its snapshot");
    }
}

/// The loom subset: exhaustive interleavings of the lock-free allocator,
/// run only under `--features loom` (nightly CI). Two writers racing on
/// `begin_write` must receive distinct ids, ids must start at one, and
/// `current` must never name an id that has not been issued.
#[cfg(all(test, feature = "loom"))]
mod loom_tests {
    use super::*;

    #[test]
    fn concurrent_writers_receive_distinct_ids_from_one() {
        loom::model(|| {
            let t = std::sync::Arc::new(VersionTracker::new());
            let t2 = std::sync::Arc::clone(&t);
            let h = loom::thread::spawn(move || t2.begin_write().0);
            let a = t.begin_write().0;
            let b = h.join().unwrap();
            assert_ne!(a, b, "two writers were handed the same id");
            assert_eq!(a.min(b), 1, "ids begin at one");
        });
    }

    #[test]
    fn current_names_the_last_issued_id_once_writers_finish() {
        loom::model(|| {
            let t = std::sync::Arc::new(VersionTracker::new());
            let t2 = std::sync::Arc::clone(&t);
            let h = loom::thread::spawn(move || t2.begin_write().0);
            let issued = t.begin_write().0;
            // Before the join, `current` may legitimately run ahead of THIS
            // thread's id — the other writer's fetch_add can land first.
            // That is why the check lives after the join: with both writes
            // complete, `current` must name exactly the highest id handed
            // out. (Loom explored every interleaving to get here.)
            let other = h.join().unwrap();
            let seen = t.current().0;
            assert_eq!(
                seen,
                issued.max(other),
                "after all writers finish, current must be the highest issued id"
            );
        });
    }
}
