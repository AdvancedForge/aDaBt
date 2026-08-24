//! Paged file access and the buffer pool.
//!
//! The eviction policy sits behind a trait from the start. It is one of the
//! earliest things a resource-priority workload will want to change, and
//! retrofitting a policy seam into a buffer pool later means touching every
//! access path.

use adabt_core::error::{Error, Result};
use std::collections::{HashMap, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use crate::page::{Page, PageId, PAGE_SIZE};

/// A file addressed as an array of fixed-size pages.
pub struct PagedFile {
    file: File,
    page_count: u32,
}

impl PagedFile {
    pub fn open(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        let len = file.metadata()?.len();
        if len % PAGE_SIZE as u64 != 0 {
            // A partial trailing page means the process died mid-write. The WAL
            // is the authority on what should be there, so refuse to guess.
            return Err(Error::Corruption(format!(
                "heap file length {len} is not a multiple of the {PAGE_SIZE}-byte page size"
            )));
        }
        Ok(Self {
            file,
            page_count: (len / PAGE_SIZE as u64) as u32,
        })
    }

    pub fn page_count(&self) -> u32 {
        self.page_count
    }

    pub fn read_page(&mut self, id: PageId) -> Result<Page> {
        if id.0 >= self.page_count {
            return Err(Error::Corruption(format!(
                "page {} out of range (file has {})",
                id.0, self.page_count
            )));
        }
        let mut buf = [0u8; PAGE_SIZE];
        self.file
            .seek(SeekFrom::Start(id.0 as u64 * PAGE_SIZE as u64))?;
        self.file.read_exact(&mut buf)?;
        Page::from_bytes(buf)
    }

    /// Read `count` consecutive pages in one call.
    ///
    /// The point is the syscall count, not the bytes. Reading sixteen pages
    /// one at a time is sixteen seeks and sixteen reads for 128 KiB that the
    /// kernel would have handed over in one; on a sequential scan that
    /// difference is most of the cost of the scan.
    ///
    /// Stops at the end of the file rather than erroring, so a caller may ask
    /// for more than exists and take what there is.
    pub fn read_pages(&mut self, start: PageId, count: u32) -> Result<Vec<Page>> {
        if start.0 >= self.page_count {
            return Err(Error::Corruption(format!(
                "page {} out of range (file has {})",
                start.0, self.page_count
            )));
        }
        let count = count.min(self.page_count - start.0).max(1);
        let mut buf = vec![0u8; count as usize * PAGE_SIZE];
        self.file
            .seek(SeekFrom::Start(start.0 as u64 * PAGE_SIZE as u64))?;
        self.file.read_exact(&mut buf)?;
        buf.chunks_exact(PAGE_SIZE)
            .map(|c| {
                let mut page = [0u8; PAGE_SIZE];
                page.copy_from_slice(c);
                Page::from_bytes(page)
            })
            .collect()
    }

    pub fn write_page(&mut self, id: PageId, page: &mut Page) -> Result<()> {
        page.seal();
        self.file
            .seek(SeekFrom::Start(id.0 as u64 * PAGE_SIZE as u64))?;
        self.file.write_all(page.as_bytes())?;
        if id.0 >= self.page_count {
            self.page_count = id.0 + 1;
        }
        Ok(())
    }

    pub fn allocate(&mut self) -> Result<PageId> {
        let id = PageId(self.page_count);
        let mut p = Page::new();
        self.write_page(id, &mut p)?;
        Ok(id)
    }

    /// Shorten the file to `pages`, returning the space to the filesystem.
    ///
    /// The caller is responsible for knowing that nothing lives up there. This
    /// deliberately has no opinion about that: a pager that second-guessed its
    /// caller would need the page directory, which is a layer above it.
    pub fn truncate_to(&mut self, pages: u32) -> Result<()> {
        if pages >= self.page_count {
            return Ok(());
        }
        self.file.set_len(pages as u64 * PAGE_SIZE as u64)?;
        self.file.sync_all()?;
        self.page_count = pages;
        Ok(())
    }

    pub fn sync(&self) -> Result<()> {
        self.file.sync_all()?;
        Ok(())
    }
}

// -- eviction -------------------------------------------------------------

pub trait EvictionPolicy: Send {
    fn name(&self) -> &'static str;
    fn touch(&mut self, id: PageId);
    fn forget(&mut self, id: PageId);
    /// Choose a victim. Returning `None` means the policy holds no candidates.
    fn victim(&mut self) -> Option<PageId>;
}

/// Least-recently-used.
#[derive(Default)]
pub struct Lru {
    order: VecDeque<PageId>,
}

impl EvictionPolicy for Lru {
    fn name(&self) -> &'static str {
        "lru"
    }
    fn touch(&mut self, id: PageId) {
        if let Some(i) = self.order.iter().position(|p| *p == id) {
            self.order.remove(i);
        }
        self.order.push_back(id);
    }
    fn forget(&mut self, id: PageId) {
        if let Some(i) = self.order.iter().position(|p| *p == id) {
            self.order.remove(i);
        }
    }
    fn victim(&mut self) -> Option<PageId> {
        self.order.pop_front()
    }
}

/// Second-chance (clock): approximates LRU without reordering on every hit.
#[derive(Default)]
pub struct Clock {
    entries: Vec<(PageId, bool)>,
    hand: usize,
}

impl EvictionPolicy for Clock {
    fn name(&self) -> &'static str {
        "clock"
    }
    fn touch(&mut self, id: PageId) {
        match self.entries.iter_mut().find(|(p, _)| *p == id) {
            Some(e) => e.1 = true,
            None => self.entries.push((id, true)),
        }
    }
    fn forget(&mut self, id: PageId) {
        if let Some(i) = self.entries.iter().position(|(p, _)| *p == id) {
            self.entries.remove(i);
            if self.hand > i {
                self.hand -= 1;
            }
        }
    }
    fn victim(&mut self) -> Option<PageId> {
        if self.entries.is_empty() {
            return None;
        }
        // Every full sweep clears at least one reference bit, so this
        // terminates within two passes.
        for _ in 0..self.entries.len() * 2 {
            if self.hand >= self.entries.len() {
                self.hand = 0;
            }
            let (id, referenced) = self.entries[self.hand];
            if referenced {
                self.entries[self.hand].1 = false;
                self.hand += 1;
            } else {
                self.entries.remove(self.hand);
                return Some(id);
            }
        }
        let (id, _) = self.entries.remove(0);
        Some(id)
    }
}

// -- buffer pool ----------------------------------------------------------

struct Frame {
    page: Page,
    dirty: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BufferStats {
    pub hits: u64,
    pub misses: u64,
    pub evictions: u64,
    pub writes: u64,
    pub reads: u64,
    pub read_ahead_pages: u64,
}

impl BufferStats {
    /// Pages that arrived because a neighbour was wanted.
    ///
    /// Worth counting separately from `reads`: read-ahead makes the read count
    /// go *down* while the page count goes up, and a single figure would hide
    /// both halves of what it did.
    pub fn read_ahead_pages(&self) -> u64 {
        self.read_ahead_pages
    }
    pub fn hit_rate(&self) -> Option<f64> {
        let total = self.hits + self.misses;
        if total == 0 {
            None
        } else {
            Some(self.hits as f64 / total as f64)
        }
    }
}

/// Consecutive misses before read-ahead starts.
///
/// Two, not one. A single miss is the common case and says nothing; two in a row
/// at adjacent addresses is the shortest evidence of a scan that is evidence at
/// all, and waiting longer means the read-ahead misses the start of every scan
/// it is supposed to accelerate.
const RUN_BEFORE_READ_AHEAD: u32 = 2;
/// Pages pulled in per read-ahead. 16 pages is 128 KiB — one comfortable read.
const READ_AHEAD_PAGES: u32 = 16;

pub struct BufferPool {
    file: PagedFile,
    frames: HashMap<PageId, Frame>,
    capacity: usize,
    policy: Box<dyn EvictionPolicy>,
    stats: BufferStats,
    /// Whether sequential misses trigger a batched read.
    read_ahead: bool,
    /// The last page faulted in, and how long the run of adjacent ones is.
    last_miss: Option<PageId>,
    run: u32,
}

impl BufferPool {
    pub fn new(file: PagedFile, capacity: usize, policy: Box<dyn EvictionPolicy>) -> Self {
        assert!(capacity > 0, "a buffer pool needs at least one frame");
        Self {
            file,
            frames: HashMap::new(),
            capacity,
            policy,
            stats: BufferStats::default(),
            read_ahead: false,
            last_miss: None,
            run: 0,
        }
    }

    /// Turn sequential read-ahead on or off.
    ///
    /// Off by default. Read-ahead is a bet that the next page will be wanted,
    /// and on a point-lookup workload it is a bet that loses every time — so it
    /// is an optimization the driver enables when the evidence supports it,
    /// rather than a behaviour the pool has always had.
    pub fn set_read_ahead(&mut self, on: bool) {
        self.read_ahead = on;
        self.last_miss = None;
        self.run = 0;
    }

    pub fn read_ahead_enabled(&self) -> bool {
        self.read_ahead
    }

    pub fn open(path: &Path, capacity: usize) -> Result<Self> {
        Ok(Self::new(
            PagedFile::open(path)?,
            capacity,
            Box::<Clock>::default(),
        ))
    }

    pub fn stats(&self) -> BufferStats {
        self.stats
    }
    pub fn capacity(&self) -> usize {
        self.capacity
    }
    pub fn resident(&self) -> usize {
        self.frames.len()
    }
    pub fn page_count(&self) -> u32 {
        self.file.page_count()
    }
    pub fn policy_name(&self) -> &'static str {
        self.policy.name()
    }

    /// Change how much memory the pool may hold, evicting down to the new size.
    ///
    /// Exposed because pool size is one of the first knobs an optimizer reaches
    /// for: it is the cleanest RAM-for-latency trade the engine has.
    pub fn set_capacity(&mut self, capacity: usize) -> Result<()> {
        assert!(capacity > 0);
        self.capacity = capacity;
        while self.frames.len() > self.capacity {
            self.evict_one()?;
        }
        Ok(())
    }

    fn evict_one(&mut self) -> Result<()> {
        while let Some(victim) = self.policy.victim() {
            if let Some(mut frame) = self.frames.remove(&victim) {
                if frame.dirty {
                    self.file.write_page(victim, &mut frame.page)?;
                    self.stats.writes += 1;
                }
                self.stats.evictions += 1;
                return Ok(());
            }
            // The policy named a page the pool no longer holds; keep looking.
        }
        Err(Error::Corruption(
            "buffer pool is full but the eviction policy offered no victim".into(),
        ))
    }

    fn ensure_resident(&mut self, id: PageId) -> Result<()> {
        if self.frames.contains_key(&id) {
            self.stats.hits += 1;
            self.policy.touch(id);
            return Ok(());
        }
        self.stats.misses += 1;
        // A run of misses at adjacent addresses is what a scan looks like from
        // in here. Tracked on misses only: a hit says nothing about whether the
        // next page will have to be fetched.
        self.run = match self.last_miss {
            Some(prev) if prev.0 + 1 == id.0 => self.run + 1,
            _ => 1,
        };
        self.last_miss = Some(id);

        let ahead = self.read_ahead_span(id);
        // Make room for the whole batch before reading any of it, so the pool
        // never momentarily holds more than its capacity.
        while self.frames.len() + ahead as usize > self.capacity {
            self.evict_one()?;
        }
        if ahead > 1 {
            // One read for the demanded page and its neighbours together.
            let pages = self.file.read_pages(id, ahead)?;
            self.stats.reads += 1;
            self.stats.read_ahead_pages += pages.len() as u64 - 1;
            for (i, page) in pages.into_iter().enumerate() {
                let pid = PageId(id.0 + i as u32);
                // Never displace what is already there: a resident page may be
                // dirty, and overwriting it with the copy on disk would silently
                // discard a write.
                self.frames
                    .entry(pid)
                    .or_insert(Frame { page, dirty: false });
                self.policy.touch(pid);
            }
        } else {
            let page = self.file.read_page(id)?;
            self.stats.reads += 1;
            self.frames.insert(id, Frame { page, dirty: false });
        }
        self.policy.touch(id);
        Ok(())
    }

    /// How many pages to fetch for a miss at `id`, including `id` itself.
    ///
    /// A batch may evict — refusing to would mean no read-ahead at all once the
    /// pool is warm, which is exactly when a scan needs it — but it is capped at
    /// a quarter of the pool. That bound is the whole safety argument: a
    /// sequential scan can steadily push its own trailing pages out, which costs
    /// nothing because they have already been read, but it can never displace
    /// more than a quarter of what the pool holds in a single speculative act.
    /// Without a cap, one scan through a large file would evict everything else
    /// in the database to make room for pages nobody asked for.
    fn read_ahead_span(&self, id: PageId) -> u32 {
        if !self.read_ahead || self.run < RUN_BEFORE_READ_AHEAD {
            return 1;
        }
        let quarter = (self.capacity / 4).max(1) as u32;
        let remaining = self.file.page_count().saturating_sub(id.0);
        READ_AHEAD_PAGES.min(quarter).min(remaining).max(1)
    }

    pub fn get(&mut self, id: PageId) -> Result<&Page> {
        self.ensure_resident(id)?;
        Ok(&self.frames.get(&id).expect("just made resident").page)
    }

    pub fn get_mut(&mut self, id: PageId) -> Result<&mut Page> {
        self.ensure_resident(id)?;
        let f = self.frames.get_mut(&id).expect("just made resident");
        f.dirty = true;
        Ok(&mut f.page)
    }

    pub fn allocate(&mut self) -> Result<PageId> {
        let id = self.file.allocate()?;
        self.stats.writes += 1;
        if self.frames.len() >= self.capacity {
            self.evict_one()?;
        }
        self.frames.insert(
            id,
            Frame {
                page: Page::new(),
                dirty: true,
            },
        );
        self.policy.touch(id);
        Ok(id)
    }

    /// Shorten the heap, dropping any resident frames above the new end.
    ///
    /// Frames are dropped rather than flushed: they describe pages that are
    /// about to stop existing, and writing them back on the way out would extend
    /// the file again.
    pub fn truncate_to(&mut self, pages: u32) -> Result<()> {
        self.flush_all()?;
        self.frames.retain(|id, _| id.0 < pages);
        self.file.truncate_to(pages)
    }

    /// Write every dirty frame back. Does not fsync; see `checkpoint`.
    pub fn flush_all(&mut self) -> Result<()> {
        let dirty: Vec<PageId> = self
            .frames
            .iter()
            .filter(|(_, f)| f.dirty)
            .map(|(id, _)| *id)
            .collect();
        for id in dirty {
            let frame = self.frames.get_mut(&id).expect("collected above");
            self.file.write_page(id, &mut frame.page)?;
            frame.dirty = false;
            self.stats.writes += 1;
        }
        Ok(())
    }

    /// Flush and fsync, so everything written so far is on stable storage.
    pub fn checkpoint(&mut self) -> Result<()> {
        self.flush_all()?;
        self.file.sync()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::page::SlotId;

    struct Tmp(std::path::PathBuf);
    impl Tmp {
        fn new(tag: &str) -> Self {
            let mut p = std::env::temp_dir();
            p.push(format!(
                "adabt-pager-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_file(&p);
            Tmp(p)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[test]
    fn pages_survive_a_reopen() {
        let t = Tmp::new("reopen");
        let slot;
        let id;
        {
            let mut pool = BufferPool::open(t.path(), 4).unwrap();
            id = pool.allocate().unwrap();
            slot = pool.get_mut(id).unwrap().insert(b"persisted").unwrap();
            pool.checkpoint().unwrap();
        }
        let mut pool = BufferPool::open(t.path(), 4).unwrap();
        assert_eq!(pool.get(id).unwrap().get(slot).unwrap(), b"persisted");
    }

    #[test]
    fn a_dirty_page_is_written_before_it_is_evicted() {
        let t = Tmp::new("evict-dirty");
        let mut pool = BufferPool::open(t.path(), 2).unwrap();
        let a = pool.allocate().unwrap();
        let slot = pool.get_mut(a).unwrap().insert(b"keepme").unwrap();
        // Force `a` out by touching more pages than the pool can hold.
        for _ in 0..5 {
            let p = pool.allocate().unwrap();
            pool.get_mut(p).unwrap().insert(b"filler").unwrap();
        }
        assert!(pool.stats().evictions > 0, "nothing was evicted");
        assert_eq!(
            pool.get(a).unwrap().get(slot).unwrap(),
            b"keepme",
            "an evicted dirty page lost its contents"
        );
    }

    #[test]
    fn the_pool_never_exceeds_its_capacity() {
        let t = Tmp::new("capacity");
        let mut pool = BufferPool::open(t.path(), 3).unwrap();
        for _ in 0..20 {
            let p = pool.allocate().unwrap();
            pool.get_mut(p).unwrap().insert(b"x").unwrap();
            assert!(
                pool.resident() <= 3,
                "resident {} > capacity 3",
                pool.resident()
            );
        }
    }

    #[test]
    fn shrinking_capacity_evicts_down_and_preserves_data() {
        let t = Tmp::new("shrink");
        let mut pool = BufferPool::open(t.path(), 16).unwrap();
        let mut ids = Vec::new();
        for i in 0..10u8 {
            let p = pool.allocate().unwrap();
            pool.get_mut(p).unwrap().insert(&[i; 32]).unwrap();
            ids.push(p);
        }
        pool.set_capacity(2).unwrap();
        assert!(pool.resident() <= 2);
        for (i, p) in ids.iter().enumerate() {
            assert_eq!(
                pool.get(*p).unwrap().get(SlotId(0)).unwrap(),
                &[i as u8; 32]
            );
        }
    }

    #[test]
    fn hit_rate_reflects_locality() {
        let t = Tmp::new("hitrate");
        let mut pool = BufferPool::open(t.path(), 4).unwrap();
        let hot = pool.allocate().unwrap();
        pool.get_mut(hot).unwrap().insert(b"hot").unwrap();
        for _ in 0..50 {
            pool.get(hot).unwrap();
        }
        let hr = pool.stats().hit_rate().unwrap();
        assert!(
            hr > 0.9,
            "expected a high hit rate for a single hot page, got {hr}"
        );
    }

    #[test]
    fn an_empty_pool_reports_no_hit_rate_rather_than_zero() {
        assert_eq!(BufferStats::default().hit_rate(), None);
    }

    #[test]
    fn a_truncated_file_is_rejected_at_open() {
        let t = Tmp::new("truncated");
        {
            let mut pool = BufferPool::open(t.path(), 2).unwrap();
            pool.allocate().unwrap();
            pool.checkpoint().unwrap();
        }
        // Simulate a process that died halfway through writing a page.
        let mut bytes = std::fs::read(t.path()).unwrap();
        bytes.truncate(bytes.len() - 100);
        std::fs::write(t.path(), &bytes).unwrap();
        assert!(BufferPool::open(t.path(), 2).is_err());
    }

    #[test]
    fn on_disk_corruption_is_detected_on_read() {
        let t = Tmp::new("bitrot");
        let id;
        {
            let mut pool = BufferPool::open(t.path(), 2).unwrap();
            id = pool.allocate().unwrap();
            pool.get_mut(id).unwrap().insert(b"valuable").unwrap();
            pool.checkpoint().unwrap();
        }
        let mut bytes = std::fs::read(t.path()).unwrap();
        bytes[PAGE_SIZE / 2] ^= 0xff;
        std::fs::write(t.path(), &bytes).unwrap();
        let mut pool = BufferPool::open(t.path(), 2).unwrap();
        assert!(matches!(pool.get(id), Err(Error::Corruption(_))));
    }

    fn policy_behaves(mut p: Box<dyn EvictionPolicy>) {
        for i in 0..4 {
            p.touch(PageId(i));
        }
        let v = p
            .victim()
            .expect("a policy holding pages must offer a victim");
        assert!(v.0 < 4);
        p.forget(PageId(3));
        assert_ne!(p.victim(), Some(PageId(3)), "a forgotten page was evicted");
    }

    #[test]
    fn every_eviction_policy_meets_the_contract() {
        policy_behaves(Box::<Lru>::default());
        policy_behaves(Box::<Clock>::default());
    }

    #[test]
    fn an_empty_policy_offers_no_victim() {
        assert_eq!(Lru::default().victim(), None);
        assert_eq!(Clock::default().victim(), None);
    }

    #[test]
    fn lru_evicts_the_least_recently_used() {
        let mut p = Lru::default();
        for i in 0..3 {
            p.touch(PageId(i));
        }
        p.touch(PageId(0)); // 0 is now the most recent, so 1 is the oldest
        assert_eq!(p.victim(), Some(PageId(1)));
    }

    #[test]
    fn clock_gives_a_referenced_page_a_second_chance() {
        let mut p = Clock::default();
        p.touch(PageId(0));
        p.touch(PageId(1));
        // Both carry a reference bit, so the first sweep clears rather than
        // evicts, and the victim is still one of the two.
        let v = p.victim().unwrap();
        assert!(v == PageId(0) || v == PageId(1));
    }

    #[test]
    fn both_policies_work_end_to_end_in_the_pool() {
        for policy in [
            Box::<Lru>::default() as Box<dyn EvictionPolicy>,
            Box::<Clock>::default(),
        ] {
            let t = Tmp::new(policy.name());
            let name = policy.name();
            let file = PagedFile::open(t.path()).unwrap();
            let mut pool = BufferPool::new(file, 2, policy);
            let mut ids = Vec::new();
            for i in 0..8u8 {
                let p = pool.allocate().unwrap();
                pool.get_mut(p).unwrap().insert(&[i; 16]).unwrap();
                ids.push(p);
            }
            for (i, p) in ids.iter().enumerate() {
                assert_eq!(
                    pool.get(*p).unwrap().get(SlotId(0)).unwrap(),
                    &[i as u8; 16],
                    "policy {name} lost data"
                );
            }
        }
    }
}
