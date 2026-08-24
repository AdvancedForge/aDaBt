//! An on-disk copy of the page directory.
//!
//! Opening a database means knowing where every record lives. That map is
//! derived — it can always be rebuilt by reading every page and looking at the
//! collection and record ids in each slot prefix — and until now it always was.
//! Which means opening a database cost a full pass over the heap, whatever else
//! had or had not changed.
//!
//! That pass is measurable and it dominates: on 200,000 records it is 801ms,
//! against 817ms for rebuilding three indexes and essentially nothing for
//! everything else. Once the derived-representation cache removed the index
//! rebuild, this became the whole of the remaining cost.
//!
//! Like [`crate::derived`], nothing here is authoritative and every failure to
//! read is reported as "no cache". A directory that cannot be loaded costs a
//! scan, which is what used to happen every time anyway.
//!
//! # Why this stamp is different
//!
//! The derived cache is validated *after* recovery, against the state recovery
//! produced. This one has to be validated *before* it, because its whole purpose
//! is to replace a step of recovery — so it can only be checked against facts
//! available before a single page has been read: the database identity, the log
//! position of the last checkpoint, and the length of the heap file.
//!
//! That is enough, and it is enough for a specific reason. A checkpoint flushes
//! every dirty page and *then* records the log position at which it did so. The
//! directory written at that moment describes exactly the heap the checkpoint
//! left behind, and everything after it is in the log and gets replayed as
//! usual. So the cache does not have to describe the current state — it has to
//! describe the checkpoint, and replay carries it the rest of the way.

use crate::page::{PageId, RecordLocation, SlotId};
use adabt_core::error::Result;
use adabt_core::ids::RecordId;
use std::path::{Path, PathBuf};

const MAGIC: &[u8; 8] = b"aDaBtDir";
const FORMAT_VERSION: u32 = 1;

/// What was true at the checkpoint this directory describes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stamp {
    pub identity: u128,
    /// The log position the checkpoint flushed up to.
    pub checkpoint_lsn: u64,
    pub heap_bytes: u64,
}

/// A directory as it was at a checkpoint.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Snapshot {
    /// Live records per collection, **by collection id, not by name**.
    ///
    /// The id is what a record's slot prefix carries, so it is what a page scan
    /// recovers, so it is what this has to reproduce. Keying by name looks
    /// equivalent and is not: a schema migration hands one collection's records
    /// to another under a *different* id while keeping the name, and a cache
    /// keyed by name puts those records back under the wrong id. Recovery then
    /// completes the migration by freeing the old collection's pages — which are
    /// now the ones holding all the data.
    ///
    /// That is not hypothetical. Keying by name deleted every record in a frozen
    /// collection on restart, and the test that caught it is
    /// `a_frozen_schema_survives_a_restart`.
    pub collections: Vec<(u32, Vec<(RecordId, RecordLocation)>)>,
    /// Free bytes per page.
    pub free_space: Vec<(PageId, u32)>,
}

pub fn path(dir: &Path) -> PathBuf {
    dir.join("directory.adabt")
}

pub fn write(dir: &Path, stamp: &Stamp, snapshot: &Snapshot) -> Result<()> {
    let mut body = Vec::new();
    body.extend_from_slice(MAGIC);
    body.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
    body.extend_from_slice(&stamp.identity.to_le_bytes());
    body.extend_from_slice(&stamp.checkpoint_lsn.to_le_bytes());
    body.extend_from_slice(&stamp.heap_bytes.to_le_bytes());

    body.extend_from_slice(&(snapshot.collections.len() as u32).to_le_bytes());
    for (cid, records) in &snapshot.collections {
        body.extend_from_slice(&cid.to_le_bytes());
        body.extend_from_slice(&(records.len() as u32).to_le_bytes());
        for (id, loc) in records {
            body.extend_from_slice(&id.0.to_le_bytes());
            body.extend_from_slice(&loc.page.0.to_le_bytes());
            body.extend_from_slice(&loc.slot.0.to_le_bytes());
        }
    }
    body.extend_from_slice(&(snapshot.free_space.len() as u32).to_le_bytes());
    for (page, free) in &snapshot.free_space {
        body.extend_from_slice(&page.0.to_le_bytes());
        body.extend_from_slice(&free.to_le_bytes());
    }
    let sum = checksum(&body);
    body.extend_from_slice(&sum.to_le_bytes());

    let tmp = dir.join("directory.adabt.tmp");
    std::fs::write(&tmp, &body)?;
    std::fs::rename(&tmp, path(dir))?;
    Ok(())
}

/// Read the directory, if one describes this exact checkpoint.
pub fn read(dir: &Path, stamp: &Stamp) -> Option<Snapshot> {
    let bytes = std::fs::read(path(dir)).ok()?;
    if bytes.len() < MAGIC.len() + 8 {
        return None;
    }
    let (body, tail) = bytes.split_at(bytes.len() - 8);
    if checksum(body) != u64::from_le_bytes(tail.try_into().ok()?) {
        return None;
    }
    let mut r = Reader { buf: body, pos: 0 };
    if r.take(MAGIC.len())? != MAGIC || r.u32()? != FORMAT_VERSION {
        return None;
    }
    let found = Stamp {
        identity: u128::from_le_bytes(r.take(16)?.try_into().ok()?),
        checkpoint_lsn: r.u64()?,
        heap_bytes: r.u64()?,
    };
    if found != *stamp {
        return None;
    }

    let mut snapshot = Snapshot::default();
    let collections = r.u32()? as usize;
    for _ in 0..collections {
        let cid = r.u32()?;
        let n = r.u32()? as usize;
        let mut records = Vec::new();
        for _ in 0..n {
            let id = RecordId(r.u64()?);
            let page = PageId(r.u32()?);
            let slot = SlotId(r.u16()?);
            records.push((id, RecordLocation { page, slot }));
        }
        snapshot.collections.push((cid, records));
    }
    let pages = r.u32()? as usize;
    for _ in 0..pages {
        let page = PageId(r.u32()?);
        snapshot.free_space.push((page, r.u32()?));
    }
    // Trailing bytes mean the two sides disagree about the format. Rebuilding is
    // always available and always right, so there is no reason to guess.
    if r.pos != body.len() {
        return None;
    }
    Some(snapshot)
}

pub fn discard(dir: &Path) {
    let _ = std::fs::remove_file(path(dir));
}

fn checksum(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x1000_0000_01b3);
    }
    h
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(n)?;
        let s = self.buf.get(self.pos..end)?;
        self.pos = end;
        Some(s)
    }
    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_le_bytes(self.take(2)?.try_into().ok()?))
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Tmp(PathBuf);
    impl Tmp {
        fn new(tag: &str) -> Self {
            let mut p = std::env::temp_dir();
            p.push(format!(
                "adabt-dircache-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).unwrap();
            Tmp(p)
        }
    }
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn stamp() -> Stamp {
        Stamp {
            identity: 0xdead_beef_dead_beef_dead_beef_dead_beef,
            checkpoint_lsn: 991,
            heap_bytes: 8192 * 40,
        }
    }

    fn snapshot() -> Snapshot {
        Snapshot {
            collections: vec![
                (
                    7,
                    (0..500u64)
                        .map(|i| {
                            (
                                RecordId(i),
                                RecordLocation {
                                    page: PageId((i / 20) as u32),
                                    slot: SlotId((i % 20) as u16),
                                },
                            )
                        })
                        .collect(),
                ),
                (9, Vec::new()),
            ],
            free_space: (0..40u32).map(|p| (PageId(p), p * 13)).collect(),
        }
    }

    #[test]
    fn a_directory_round_trips() {
        let t = Tmp::new("round");
        write(&t.0, &stamp(), &snapshot()).unwrap();
        assert_eq!(read(&t.0, &stamp()), Some(snapshot()));
    }

    #[test]
    fn an_absent_directory_is_not_an_error() {
        let t = Tmp::new("absent");
        assert_eq!(read(&t.0, &stamp()), None);
    }

    #[test]
    fn a_directory_from_another_checkpoint_is_refused() {
        let t = Tmp::new("stale");
        write(&t.0, &stamp(), &snapshot()).unwrap();
        for mutate in [
            (|s: &mut Stamp| s.checkpoint_lsn += 1) as fn(&mut Stamp),
            |s: &mut Stamp| s.heap_bytes += 8192,
            |s: &mut Stamp| s.identity ^= 1,
        ] {
            let mut other = stamp();
            mutate(&mut other);
            assert_eq!(read(&t.0, &other), None);
        }
    }

    #[test]
    fn a_truncated_directory_is_refused_at_every_length() {
        let t = Tmp::new("truncated");
        write(&t.0, &stamp(), &snapshot()).unwrap();
        let full = std::fs::read(path(&t.0)).unwrap();
        // Every 37th offset: the whole sweep is 20k reads of a 15k file and the
        // point is coverage of each region, not of each byte.
        for cut in (0..full.len()).step_by(37) {
            std::fs::write(path(&t.0), &full[..cut]).unwrap();
            assert_eq!(read(&t.0, &stamp()), None, "truncation to {cut} accepted");
        }
    }

    #[test]
    fn corruption_anywhere_is_refused() {
        let t = Tmp::new("corrupt");
        write(&t.0, &stamp(), &snapshot()).unwrap();
        let full = std::fs::read(path(&t.0)).unwrap();
        for at in (0..full.len()).step_by(29) {
            let mut damaged = full.clone();
            damaged[at] ^= 0x80;
            std::fs::write(path(&t.0), &damaged).unwrap();
            assert_eq!(read(&t.0, &stamp()), None, "damage at {at} went unnoticed");
        }
    }

    #[test]
    fn an_empty_database_round_trips() {
        let t = Tmp::new("empty");
        let s = Snapshot::default();
        write(&t.0, &stamp(), &s).unwrap();
        assert_eq!(read(&t.0, &stamp()), Some(s));
    }
}
