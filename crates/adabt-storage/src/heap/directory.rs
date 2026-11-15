//! A collection's directory: every record id it has seen, mapped to that
//! record's version chain, with the count of live records kept alongside.
//!
//! The count is what the planner's cost model and the optimizer's inputs ask
//! for on every plan and every cycle. Counting it by walking the directory
//! made both cost a pass over every id. Keeping it here, beside the only
//! methods that can change whether a record is live, means it cannot drift
//! from what it counts: reads go through `Deref` to the map, and every
//! mutation goes through a method below that knows its effect on liveness.

use std::collections::BTreeMap;
use std::ops::Deref;

use adabt_core::ids::{RecordId, TxnId};

use super::mvcc::VersionChain;
use crate::page::RecordLocation;

#[derive(Debug, Default)]
pub(super) struct Directory {
    chains: BTreeMap<RecordId, VersionChain>,
    /// Chains whose newest version is a record rather than a tombstone.
    live: usize,
}

impl Deref for Directory {
    type Target = BTreeMap<RecordId, VersionChain>;

    fn deref(&self) -> &Self::Target {
        &self.chains
    }
}

impl<'a> IntoIterator for &'a Directory {
    type Item = (&'a RecordId, &'a VersionChain);
    type IntoIter = std::collections::btree_map::Iter<'a, RecordId, VersionChain>;

    fn into_iter(self) -> Self::IntoIter {
        self.chains.iter()
    }
}

impl Directory {
    /// Records whose newest version is live. O(1).
    pub(super) fn live(&self) -> usize {
        self.live
    }

    /// Push a version onto `id`'s chain, creating the chain if this is the
    /// first write to `id`: a record at `loc`, or a tombstone for `None`.
    ///
    /// Returns whether the chain already held a version, which is what
    /// reclamation needs to know a version was superseded.
    pub(super) fn push(&mut self, id: RecordId, txn: TxnId, loc: Option<RecordLocation>) -> bool {
        let chain = self.chains.entry(id).or_default();
        let superseded = !chain.versions.is_empty();
        let was_live = !chain.is_absent();
        chain.push(txn, loc);
        self.adjust(was_live, loc.is_some());
        superseded
    }

    /// Install the single version recovery loaded for `id` from a directory
    /// snapshot, replacing any chain `id` had.
    pub(super) fn insert_recovered(&mut self, id: RecordId, loc: RecordLocation) {
        let was_live = self
            .chains
            .insert(id, VersionChain::recovered(loc))
            .is_some_and(|old| !old.is_absent());
        self.adjust(was_live, true);
    }

    /// Forget `id` entirely.
    pub(super) fn remove(&mut self, id: RecordId) -> Option<VersionChain> {
        let old = self.chains.remove(&id)?;
        self.adjust(!old.is_absent(), false);
        Some(old)
    }

    /// Drop every version no snapshot at or after `horizon` can reach, and
    /// every chain left holding only a tombstone, calling `freed` for each
    /// dropped version that occupied a slot.
    ///
    /// Leaves the live count alone: a chain keeps its newest version, and a
    /// chain that is only a tombstone was not live.
    pub(super) fn reclaim_to(
        &mut self,
        horizon: TxnId,
        mut freed: impl FnMut(RecordId, TxnId, RecordLocation),
    ) {
        self.chains.retain(|id, chain| {
            for (txn, loc) in chain.reclaim_to(horizon) {
                freed(*id, txn, loc);
            }
            !(chain.len() == 1 && chain.is_absent())
        });
    }

    fn adjust(&mut self, was_live: bool, is_live: bool) {
        match (was_live, is_live) {
            (false, true) => self.live += 1,
            (true, false) => self.live -= 1,
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::page::{PageId, SlotId};

    fn loc(n: u32) -> Option<RecordLocation> {
        Some(RecordLocation {
            page: PageId(n),
            slot: SlotId(0),
        })
    }

    fn walked(d: &Directory) -> usize {
        d.values().filter(|c| !c.is_absent()).count()
    }

    #[test]
    fn the_live_count_is_what_a_walk_would_count() {
        let mut d = Directory::default();
        let mut txn = 0u64;
        let mut step = |d: &mut Directory, id: u64, l: Option<RecordLocation>| {
            txn += 1;
            d.push(RecordId(id), TxnId(txn), l);
        };
        for id in 0..10 {
            step(&mut d, id, loc(id as u32));
        }
        step(&mut d, 3, None);
        step(&mut d, 3, None); // a second tombstone changes nothing
        step(&mut d, 4, loc(40)); // an update keeps it live
        step(&mut d, 5, None);
        step(&mut d, 5, loc(50)); // reinserted after a delete
        assert_eq!(d.live(), walked(&d));
        assert_eq!(d.live(), 9);

        d.remove(RecordId(0));
        d.remove(RecordId(3)); // removing a tombstoned chain
        d.remove(RecordId(99)); // and one that was never there
        assert_eq!(d.live(), walked(&d));

        d.insert_recovered(RecordId(1), loc(1).unwrap()); // replaces a live chain
        d.insert_recovered(RecordId(77), loc(7).unwrap());
        assert_eq!(d.live(), walked(&d));

        let mut freed = 0;
        d.reclaim_to(TxnId(u64::MAX), |_, _, _| freed += 1);
        assert!(freed > 0);
        assert_eq!(d.live(), walked(&d));
    }
}
