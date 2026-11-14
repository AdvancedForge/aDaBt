//! The page directory as of a checkpoint: where every live record was.
//!
//! # Why this is part of the durable state
//!
//! A heap file is not self-describing. Its pages hold, beside every live
//! record, slots no reader will ever be shown again:
//!
//! - the previous version of an overwritten record, and the last version of a
//!   deleted one, until version reclamation frees them. Reclamation waits for
//!   the in-process snapshots that may still read them, so a checkpoint can
//!   flush such slots to disk;
//! - writes from after the last checkpoint that the buffer pool stole to disk
//!   early, possibly several copies of the same record. Replay places those
//!   writes afresh, so the stolen copies are never referenced again.
//!
//! Each carries a well-formed slot prefix naming a collection and a record id.
//! A directory rebuilt by reading every page cannot tell any of them from a
//! live record, and a scan that met two slots for one id would keep whichever
//! came last on disk. That is how an older recovery brought deleted records
//! back and returned an overwritten version instead of the newest one.
//!
//! So recovery never rebuilds the directory from the pages. It starts from
//! this file — the newest version's location of every record, written by each
//! checkpoint once its pages are on disk — and replays the log from the LSN in
//! its stamp ([`crate::heap`] states the invariant that makes that sound). A
//! checkpoint that cannot write it fails, and a database whose log no longer
//! reaches back to the start refuses to open without it, exactly as it does
//! without its catalog.
//!
//! It also keeps opening a database cheap: loading it reads no pages, where
//! reading every page cost 801ms on 200,000 records.
//!
//! # The stamp
//!
//! The stamp is what recovery can check before reading a single page: the
//! database identity, the LSN the checkpoint flushed up to, and the length of
//! the heap file it left behind.
//!
//! - The identity keeps a directory copied in from another database out.
//! - The LSN is where replay starts. Any checkpoint's directory is a valid
//!   starting point for as long as the log reaches back to it, whatever the
//!   heap file went through afterwards.
//! - The length says which pages existed at the checkpoint. Any page past it
//!   was allocated later and holds nothing replay does not write again.
//!
//! A point-in-time restore asks more — the heap file must be exactly as that
//! checkpoint left it ([`crate::base_image`]) — and so matches the whole stamp
//! against the current file with [`read`]. Crash recovery takes the newest
//! directory there is with [`read_latest`].

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
    /// The id is what a record's slot prefix carries, so it is what this has to
    /// reproduce. Keying by name looks equivalent and is not: a schema migration
    /// hands one collection's records to another under a *different* id while
    /// keeping the name, and a directory keyed by name puts those records back
    /// under the wrong id. Recovery then
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
    {
        use std::io::Write as _;
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&body)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path(dir))?;
    Ok(())
}

/// Read the directory, if one describes this exact checkpoint.
pub fn read(dir: &Path, stamp: &Stamp) -> Option<Snapshot> {
    read_latest(dir, stamp.identity)
        .filter(|(found, _)| found == stamp)
        .map(|(_, snapshot)| snapshot)
}

/// Read the directory and the stamp it was written under, if it is intact and
/// belongs to the database `identity` names.
pub fn read_latest(dir: &Path, identity: u128) -> Option<(Stamp, Snapshot)> {
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
    let stamp = Stamp {
        identity: u128::from_le_bytes(r.take(16)?.try_into().ok()?),
        checkpoint_lsn: r.u64()?,
        heap_bytes: r.u64()?,
    };
    if stamp.identity != identity {
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
    // Trailing bytes mean the two sides disagree about the format, and a
    // directory read wrongly is worse than none.
    if r.pos != body.len() {
        return None;
    }
    Some((stamp, snapshot))
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
    fn the_latest_directory_is_read_with_its_stamp_for_its_own_database_only() {
        let t = Tmp::new("latest");
        write(&t.0, &stamp(), &snapshot()).unwrap();
        assert_eq!(
            read_latest(&t.0, stamp().identity),
            Some((stamp(), snapshot()))
        );
        assert_eq!(read_latest(&t.0, stamp().identity ^ 1), None);
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
