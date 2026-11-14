//! Choosing the page a new payload lands on: free-space buckets for the
//! general case, and the clustering hint's key ranges for keyed inserts.

use adabt_core::error::Result;
use std::collections::HashMap;

use super::HeapStore;
use crate::page::{Page, PageId, RecordLocation};

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

/// Pages bucketed by how much free space they have.
///
/// A linear scan for a page with room is O(pages) per insert, which dominates
/// everything else once a heap grows. Buckets make it near-constant while
/// staying simple enough to be obviously correct.
#[derive(Default)]
pub(super) struct FreeSpaceMap {
    pub(super) free: HashMap<PageId, usize>,
    buckets: Vec<Vec<PageId>>,
}

const BUCKET_COUNT: usize = 8;
const BUCKET_SPAN: usize = crate::page::PAGE_SIZE / BUCKET_COUNT;

impl FreeSpaceMap {
    pub(super) fn new() -> Self {
        Self {
            free: HashMap::new(),
            buckets: vec![Vec::new(); BUCKET_COUNT],
        }
    }

    fn class_of(free: usize) -> usize {
        (free / BUCKET_SPAN).min(BUCKET_COUNT - 1)
    }

    pub(super) fn set(&mut self, page: PageId, free: usize) {
        if let Some(old) = self.free.insert(page, free) {
            let b = &mut self.buckets[Self::class_of(old)];
            if let Some(i) = b.iter().position(|p| *p == page) {
                b.swap_remove(i);
            }
        }
        self.buckets[Self::class_of(free)].push(page);
    }

    /// Forget a page entirely, so it is never offered again.
    pub(super) fn forget(&mut self, page: PageId) {
        if let Some(old) = self.free.remove(&page) {
            let b = &mut self.buckets[Self::class_of(old)];
            if let Some(i) = b.iter().position(|p| *p == page) {
                b.swap_remove(i);
            }
        }
    }

    /// A page with at least `need` bytes free, if one is known.
    pub(super) fn find(&self, need: usize) -> Option<PageId> {
        // Only the top of the class containing `need` is guaranteed to fit, so
        // search that class candidate-by-candidate and higher classes freely.
        for class in Self::class_of(need)..BUCKET_COUNT {
            for &p in &self.buckets[class] {
                if self.free.get(&p).copied().unwrap_or(0) >= need {
                    return Some(p);
                }
            }
        }
        None
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
