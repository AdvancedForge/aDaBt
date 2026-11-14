//! Choosing the page a new payload lands on: free-space buckets for the
//! general case, and the clustering hint's key ranges for keyed inserts.

use adabt_core::error::Result;
use std::collections::{BTreeSet, HashMap};

use super::HeapStore;
use crate::page::{Page, PageId, PageMap, RecordLocation};

/// The clustering hint's bookkeeping: the integer key range each page was
/// filled under.
///
/// This is what turns a keyed range scan from random page touches into a run
/// of consecutive ones — records whose keys are near each other are *placed*
/// on the same pages, so the ids an index range returns land on few pages.
/// Ranges only ever widen after the fact (deletes leave holes, updates may
/// move a record), which costs some pruning precision but never correctness:
/// the directory, not these ranges, is where truth about location lives.
#[derive(Default)]
pub(super) struct ClusterState {
    ranges: HashMap<PageId, (i64, i64)>,
}

impl ClusterState {
    /// Forget the ranges of pages at or past `end`, which no longer exist.
    pub(super) fn forget_from(&mut self, end: PageId) {
        self.ranges.retain(|page, _| *page < end);
    }
}

/// Every known page, ordered by how much free space it has.
///
/// Finding a page with room is a single ordered lookup — the page with the
/// least free space that still fits, ties going to the lower page id — so it
/// costs O(log pages) however large the heap grows. An earlier version kept
/// coarse buckets and scanned the bucket a request fell into page by page;
/// once a heap had filled a few hundred pages, that bucket held every nearly
/// full one, and the scan was most of the cost of an insert.
///
/// Least-free-that-fits keeps the property the buckets had: nearly full pages
/// are topped up before emptier ones are touched, so a sequential load fills
/// pages in turn and space freed by reclamation is reused before the file
/// grows.
#[derive(Default)]
pub(super) struct FreeSpaceMap {
    pub(super) free: PageMap<usize>,
    by_free: BTreeSet<(usize, PageId)>,
}

impl FreeSpaceMap {
    pub(super) fn new() -> Self {
        Self::default()
    }

    pub(super) fn set(&mut self, page: PageId, free: usize) {
        if let Some(old) = self.free.insert(page, free) {
            self.by_free.remove(&(old, page));
        }
        self.by_free.insert((free, page));
    }

    /// Forget a page entirely, so it is never offered again.
    pub(super) fn forget(&mut self, page: PageId) {
        if let Some(old) = self.free.remove(&page) {
            self.by_free.remove(&(old, page));
        }
    }

    /// A page with at least `need` bytes free, if one is known.
    pub(super) fn find(&self, need: usize) -> Option<PageId> {
        self.by_free
            .range((need, PageId(0))..)
            .next()
            .map(|&(_, page)| page)
    }
}

impl HeapStore {
    pub(super) fn place(&mut self, payload: &[u8]) -> Result<RecordLocation> {
        let need = Page::cost_of(payload.len());
        if let Some(pid) = self.fsm.find(need) {
            let page = self.pool.get_mut(pid)?;
            if page.can_fit(payload.len()) {
                let slot = page.insert(payload)?;
                let free = page.free_space();
                self.fsm.set(pid, free);
                return Ok(RecordLocation { page: pid, slot });
            }
            // The map was stale; correct it and fall through to a new page.
            let free = page.free_space();
            self.fsm.set(pid, free);
        }
        let pid = self.pool.allocate()?;
        let page = self.pool.get_mut(pid)?;
        let slot = page.insert(payload)?;
        let free = page.free_space();
        self.fsm.set(pid, free);
        Ok(RecordLocation { page: pid, slot })
    }

    /// Place a payload under the clustering hint, preferring a page whose key
    /// range already contains `key`.
    ///
    /// The policy in one line: containing pages first (nearest midpoint as tie
    /// break), then the adjacent range with room, then a fresh page. It is
    /// deliberately not a B-tree split — the goal is *locality*, and locality
    /// does not need perfect ordering. What it needs is that a contiguous key
    /// span maps to a small set of pages, which "put it where its neighbours
    /// are" achieves.
    ///
    /// Pages fill left to right along the key domain; when every candidate is
    /// full, a new page extends the run. Deletes leave holes and widen no
    /// ranges, so precision decays only as far as fragmentation does.
    pub(super) fn place_keyed(
        &mut self,
        collection: &str,
        payload: &[u8],
        key: i64,
    ) -> Result<RecordLocation> {
        let c = self
            .collections
            .get_mut(collection)
            .expect("checked by caller");
        let cluster = c.cluster.get_or_insert_with(ClusterState::default);

        // Candidates are pages whose range is within `slack` of the key,
        // ordered by distance then tightness. The slack is the load-adaptive
        // part: roughly twice one page's share of the observed key domain.
        // It replaces two policies that each fail informatively:
        //
        // - nearest-at-any-distance lets the long-lived wide pages hoard
        //   every insert (the tight ones fill first, the wide ones survive
        //   to absorb whatever remains, and their ranges balloon until
        //   placement is noise);
        // - strict containment refuses to fill gaps, and random-order keys
        //   land between existing ranges often enough to give one row per
        //   page.
        //
        // With a window tied to the current page count, a page stops
        // stretching once its neighbourhood has grown a fair share of the
        // domain, and gaps between ranges still spawn fresh pages rather
        // than stretching old ones past their share.
        let mut lo_min = i64::MAX;
        let mut hi_max = i64::MIN;
        let mut candidates: Vec<(i64, i64, PageId)> = Vec::with_capacity(cluster.ranges.len());
        for (&pid, &(lo, hi)) in &cluster.ranges {
            lo_min = lo_min.min(lo);
            hi_max = hi_max.max(hi);
            let dist = if key < lo {
                lo - key
            } else if key > hi {
                key - hi
            } else {
                0
            };
            candidates.push((dist, hi - lo, pid));
        }
        let pages = candidates.len() as i64;
        let slack = if pages == 0 {
            0
        } else {
            let extent = hi_max.saturating_sub(lo_min);
            (extent / pages * 2).max(1024)
        };
        candidates.retain(|&(dist, _, _)| dist <= slack);
        candidates.sort();
        for (_, _, pid) in candidates {
            if let Ok(page) = self.pool.get_mut(pid) {
                if page.can_fit(payload.len()) {
                    let slot = page.insert(payload)?;
                    // The range stays an honest record of the page's contents:
                    // it widens only to swallow a key it genuinely took.
                    if let Some(r) = cluster.ranges.get_mut(&pid) {
                        r.0 = r.0.min(key);
                        r.1 = r.1.max(key);
                    }
                    self.fsm.set(pid, page.free_space());
                    return Ok(RecordLocation { page: pid, slot });
                }
            }
        }
        // No tracked page both contains the key and has room: start a fresh
        // one. It becomes the tightest range claiming this key, so its
        // neighbourhood settles onto it as it fills.
        let pid = self.pool.allocate()?;
        let page = self.pool.get_mut(pid)?;
        let slot = page.insert(payload)?;
        let free = page.free_space();
        self.fsm.set(pid, free);
        cluster.ranges.insert(pid, (key, key));
        Ok(RecordLocation { page: pid, slot })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fullest_page_that_fits_is_offered() {
        let mut fsm = FreeSpaceMap::new();
        fsm.set(PageId(0), 40);
        fsm.set(PageId(1), 900);
        fsm.set(PageId(2), 120);
        fsm.set(PageId(3), 120);
        assert_eq!(fsm.find(100), Some(PageId(2)), "ties go to the lower id");
        assert_eq!(fsm.find(40), Some(PageId(0)));
        assert_eq!(fsm.find(500), Some(PageId(1)));
        assert_eq!(fsm.find(901), None);
    }

    #[test]
    fn a_page_is_offered_by_its_latest_free_space_only() {
        let mut fsm = FreeSpaceMap::new();
        fsm.set(PageId(7), 800);
        fsm.set(PageId(7), 30);
        assert_eq!(fsm.find(100), None, "the stale entry must be gone");
        assert_eq!(fsm.find(30), Some(PageId(7)));
        fsm.forget(PageId(7));
        assert_eq!(fsm.find(1), None);
        assert!(fsm.free.is_empty());
    }

    /// Thousands of full pages must not stand between a write and the one
    /// page with room: scanning past them used to be most of an insert.
    #[test]
    fn many_full_pages_do_not_hide_the_one_with_room() {
        let mut fsm = FreeSpaceMap::new();
        for p in 0..10_000 {
            fsm.set(PageId(p), 10);
        }
        fsm.set(PageId(10_000), 4000);
        assert_eq!(fsm.find(64), Some(PageId(10_000)));
    }
}
