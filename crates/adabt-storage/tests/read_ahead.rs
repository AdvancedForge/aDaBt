//! Sequential read-ahead in the buffer pool.
//!
//! The claim is narrow: on a scan, sixteen page faults become one read instead
//! of sixteen. The tests measure that directly through the pool's own counters
//! rather than through a stopwatch, because a timing test on this machine would
//! mostly measure the page cache of the filesystem underneath.
//!
//! The other half — that read-ahead never makes anything worse — is the half
//! worth being careful about. Speculative reads that displace pages someone
//! actually wanted turn a working cache into a worse one, so the pool only reads
//! ahead into frames nobody is using, and that is asserted here.

use adabt_core::ids::RecordId;
use adabt_core::policy::Durability;
use adabt_core::record::Record;
use adabt_core::schema::Schema;
use adabt_core::store::LogicalStore;
use adabt_storage::heap::HeapStore;
use adabt_storage::page::{Page, PageId};
use adabt_storage::pager::{BufferPool, PagedFile};
use std::path::{Path, PathBuf};

struct Tmp(PathBuf);
impl Tmp {
    fn new(tag: &str) -> Self {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "adabt-ra-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        Tmp(p)
    }
    fn path(&self) -> &Path {
        &self.0
    }
    fn heap(&self) -> PathBuf {
        self.0.join("pages.adabt")
    }
}
impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A file of `n` pages, each carrying one identifiable record.
fn file_of(t: &Tmp, n: u32) -> PathBuf {
    let mut f = PagedFile::open(&t.heap()).unwrap();
    for i in 0..n {
        let id = f.allocate().unwrap();
        let mut page = Page::new();
        page.insert(&i.to_le_bytes()).unwrap();
        f.write_page(id, &mut page).unwrap();
    }
    f.sync().unwrap();
    t.heap()
}

fn pool(path: &Path, capacity: usize, read_ahead: bool) -> BufferPool {
    let mut p = BufferPool::open(path, capacity).unwrap();
    p.set_read_ahead(read_ahead);
    p
}

#[test]
fn a_scan_costs_one_read_per_batch_instead_of_one_per_page() {
    let t = Tmp::new("scan");
    let path = file_of(&t, 160);

    let mut plain = pool(&path, 200, false);
    for i in 0..160 {
        plain.get(PageId(i)).unwrap();
    }
    let mut ahead = pool(&path, 200, true);
    for i in 0..160 {
        ahead.get(PageId(i)).unwrap();
    }

    assert_eq!(plain.stats().reads, 160, "the baseline changed shape");
    assert!(
        ahead.stats().reads <= 160 / 8,
        "read-ahead made {} reads for 160 pages",
        ahead.stats().reads
    );
    assert!(ahead.stats().read_ahead_pages > 100);
    // And every page still arrived, however it got there.
    assert_eq!(ahead.stats().misses + ahead.stats().hits, 160);
}

#[test]
fn a_single_page_read_does_not_trigger_read_ahead() {
    // One miss is the common case and says nothing about what comes next.
    let t = Tmp::new("single");
    let path = file_of(&t, 100);
    let mut p = pool(&path, 100, true);
    p.get(PageId(42)).unwrap();
    assert_eq!(p.stats().reads, 1);
    assert_eq!(p.stats().read_ahead_pages, 0);
}

#[test]
fn random_access_never_reads_ahead() {
    // The workload read-ahead is a bad bet on. A scattered access pattern must
    // cost exactly what it costs without it.
    let t = Tmp::new("random");
    let path = file_of(&t, 100);
    let mut p = pool(&path, 100, true);
    // A stride of 7 over 100 pages: every page eventually, never two adjacent.
    for k in 0..100u32 {
        p.get(PageId((k * 7) % 100)).unwrap();
    }
    assert_eq!(
        p.stats().read_ahead_pages,
        0,
        "read ahead on a non-sequential pattern"
    );
}

#[test]
fn read_ahead_is_bounded_by_a_fraction_of_the_pool() {
    // The failure mode that matters. Read-ahead is allowed to evict — refusing
    // would mean no read-ahead at all once the pool is warm — but one scan must
    // not be able to clear the pool to make room for guesses.
    let t = Tmp::new("bounded");
    let path = file_of(&t, 200);
    let mut p = pool(&path, 8, true);
    for i in 0..200 {
        p.get(PageId(i)).unwrap();
        assert!(
            p.resident() <= 8,
            "the pool grew past its capacity at page {i}"
        );
    }
    // A quarter of eight is two, so each batched read brings one extra page.
    assert!(
        p.stats().read_ahead_pages <= p.stats().reads,
        "a pool of 8 frames read ahead {} pages over {} reads",
        p.stats().read_ahead_pages,
        p.stats().reads
    );
}

#[test]
fn a_small_pool_still_reads_ahead_a_little() {
    // The other side of the bound. A cap that reduced to "never" would be a
    // disabled optimization with a comment explaining why it is on.
    let t = Tmp::new("small");
    let path = file_of(&t, 200);
    let mut p = pool(&path, 8, true);
    for i in 0..200 {
        p.get(PageId(i)).unwrap();
    }
    assert!(
        p.stats().read_ahead_pages > 0,
        "a warm pool never read ahead"
    );
    assert!(p.stats().reads < 200, "read-ahead saved no reads at all");
}

#[test]
fn read_ahead_never_discards_an_unwritten_change() {
    // A batch read must not overwrite a resident dirty page with the stale copy
    // on disk. Nothing else in the pool would notice if it did: the write is
    // simply gone, and the page checksums fine.
    let t = Tmp::new("dirty");
    let path = file_of(&t, 64);
    let mut p = pool(&path, 64, true);

    // Dirty page 5 without flushing it.
    p.get_mut(PageId(5)).unwrap().insert(b"changed").unwrap();
    let dirty_slots = p.get(PageId(5)).unwrap().slots().count();

    // Then scan across it, which will read a batch spanning page 5.
    for i in 0..64 {
        p.get(PageId(i)).unwrap();
    }
    assert_eq!(
        p.get(PageId(5)).unwrap().slots().count(),
        dirty_slots,
        "read-ahead overwrote an unflushed change"
    );
}

#[test]
fn turning_read_ahead_off_stops_it_immediately() {
    let t = Tmp::new("off");
    let path = file_of(&t, 100);
    let mut p = pool(&path, 100, true);
    for i in 0..40 {
        p.get(PageId(i)).unwrap();
    }
    assert!(p.stats().read_ahead_pages > 0);
    let so_far = p.stats().read_ahead_pages;

    p.set_read_ahead(false);
    let mut fresh = pool(&path, 100, false);
    for i in 0..100 {
        fresh.get(PageId(i)).unwrap();
    }
    for i in 40..100 {
        p.get(PageId(i)).unwrap();
    }
    assert_eq!(
        p.stats().read_ahead_pages,
        so_far,
        "read-ahead continued after being switched off"
    );
}

#[test]
fn a_batched_read_returns_exactly_what_the_pages_hold() {
    // Reading sixteen pages in one call must split them back into the same
    // sixteen pages, in order.
    let t = Tmp::new("batch");
    let path = file_of(&t, 40);
    let mut f = PagedFile::open(&path).unwrap();
    let batch = f.read_pages(PageId(8), 16).unwrap();
    assert_eq!(batch.len(), 16);
    for (i, page) in batch.iter().enumerate() {
        let slot = page.slots().next().expect("empty page");
        let got = u32::from_le_bytes(page.get(slot).unwrap().try_into().unwrap());
        assert_eq!(got, 8 + i as u32, "page {i} of the batch is out of order");
    }
    // Asking past the end takes what there is rather than failing.
    assert_eq!(f.read_pages(PageId(38), 16).unwrap().len(), 2);
    assert!(f.read_pages(PageId(40), 1).is_err());
}

#[test]
fn a_scan_returns_the_same_records_with_read_ahead_as_without() {
    // The point of all of it: faster, identical.
    let t = Tmp::new("records");
    let expected = {
        let mut h = HeapStore::open(t.path(), Durability::Relaxed, 4_096).unwrap();
        h.create_collection("c", Schema::dynamic()).unwrap();
        for i in 0..20_000u64 {
            h.insert(
                "c",
                RecordId(i),
                Record::new().with("i", i).with("pad", "x".repeat(64)),
            )
            .unwrap();
        }
        h.checkpoint().unwrap();
        h.scan("c").unwrap()
    };

    // A pool far smaller than the data, or every page would already be resident
    // from recovery and the scan would fault nothing.
    let mut h = HeapStore::open(t.path(), Durability::Relaxed, 64).unwrap();
    h.set_prefetch(true);
    assert!(h.prefetch_enabled());
    assert_eq!(h.scan("c").unwrap(), expected);
    assert!(
        h.pool_stats().read_ahead_pages() > 0,
        "the scan never read ahead, so this proves nothing"
    );
}
