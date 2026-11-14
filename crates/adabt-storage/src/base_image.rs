//! Whether a directory holds a *base image*: a heap file exactly as a
//! checkpoint at some LSN left it, with nothing written to it since, together
//! with the page directory that checkpoint recorded.
//!
//! # Why point-in-time recovery needs this
//!
//! Crash recovery and point-in-time recovery replay the same log, but they
//! ask different things of the heap file underneath it.
//!
//! Crash recovery replays *everything* after the last checkpoint. The buffer
//! pool steals — it writes a dirty page to the heap file whenever it needs the
//! frame — so the heap may already hold changes from after that checkpoint.
//! That is harmless when replay runs to the end of the log: replay is
//! idempotent, and any record the heap holds that the final state does not is
//! deleted again by an entry replay reaches.
//!
//! Point-in-time recovery stops at a target `T`. A change from after `T` that a
//! stolen page carried into the heap file is never undone, because the entry
//! that would correct it lies past the target. Pages carry no LSN, and a slot
//! freed after a checkpoint can be reused by a later record, so there is no way
//! to tell such a page apart after the fact. The only sound starting point for
//! a restore to `T` is a heap known to hold nothing after some `B <= T`, from
//! which the log `B+1..=T` is then replayed. That is a base image, and this
//! file is the record that a directory holds one.
//!
//! # Why the page directory is part of it
//!
//! A heap file at a checkpoint is not self-describing. A deleted record, or
//! the previous version of an updated one, keeps its slot until version
//! reclamation gets to it, and reclamation is lazy, so the checkpoint can flush
//! those slots to disk. Rebuilding the directory by scanning the pages cannot
//! tell them from live records. The directory snapshot the checkpoint writes
//! (`crate::directory`) can: it names the one live location of every record.
//! For ordinary recovery that snapshot is a cache; for a base image it is part
//! of the state, and a restore refuses a base image whose snapshot is missing
//! rather than falling back to a scan.
//!
//! # The invariant
//!
//! **While the marker exists, the heap file is byte for byte what the
//! checkpoint at the LSN it names left behind.** It is kept by two rules:
//!
//! - It is written only once that state is entirely on disk: at the end of a
//!   checkpoint, after the pages, the log entry, the directory snapshot and the
//!   catalog are all durable; and into a backup, after the heap has been
//!   copied from a fresh checkpoint.
//! - It is removed — durably, before the write is issued — by the first
//!   change to the heap file, of any kind ([`crate::pager::PagedFile`] does
//!   this, so no write path can forget).
//!
//! A crash anywhere in between leaves either a marker that is still true or no
//! marker, never a false one. Losing the marker costs only the ability to use
//! that directory as a base until the next checkpoint; it never costs data.
//!
//! Because the file is unchanged, the directory snapshot's own staleness check
//! (which compares the heap file's length) passes exactly when it should.

use adabt_core::error::Result;
use adabt_core::ids::Lsn;
use std::path::{Path, PathBuf};

const MAGIC: &[u8; 8] = b"aDaBtBas";
const FORMAT_VERSION: u32 = 1;

pub fn path(dir: &Path) -> PathBuf {
    dir.join("base.adabt")
}

/// Record, durably, that the heap file in `dir` is a base image as of `lsn`.
///
/// The caller is responsible for that being true at the moment of the call;
/// see the module documentation for when it is.
pub fn write(dir: &Path, identity: u128, lsn: Lsn) -> Result<()> {
    let mut body = Vec::with_capacity(48);
    body.extend_from_slice(MAGIC);
    body.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
    body.extend_from_slice(&identity.to_le_bytes());
    body.extend_from_slice(&lsn.0.to_le_bytes());
    let sum = checksum(&body);
    body.extend_from_slice(&sum.to_le_bytes());

    let tmp = dir.join("base.adabt.tmp");
    {
        use std::io::Write as _;
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&body)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path(dir))?;
    crate::sync_dir(dir)
}

/// The LSN the heap file in `dir` is a base image as of, if it is one.
///
/// `None` for absent, damaged or foreign — each of which means the same thing
/// to a restore: this heap cannot be shown to hold nothing after a given point,
/// so it is not a starting point for one.
pub fn read(dir: &Path, identity: u128) -> Option<Lsn> {
    let bytes = std::fs::read(path(dir)).ok()?;
    if bytes.len() != MAGIC.len() + 4 + 16 + 8 + 8 {
        return None;
    }
    let (body, tail) = bytes.split_at(bytes.len() - 8);
    if checksum(body) != u64::from_le_bytes(tail.try_into().ok()?) {
        return None;
    }
    if &body[..8] != MAGIC {
        return None;
    }
    if u32::from_le_bytes(body[8..12].try_into().ok()?) != FORMAT_VERSION {
        return None;
    }
    if u128::from_le_bytes(body[12..28].try_into().ok()?) != identity {
        return None;
    }
    Some(Lsn(u64::from_le_bytes(body[28..36].try_into().ok()?)))
}

/// Remove the marker durably: on return, a crash cannot bring it back.
///
/// Must complete before the heap file is changed, which is why an error here is
/// propagated rather than swallowed — carrying on with the write would leave a
/// marker on disk that is no longer true.
pub fn discard(dir: &Path) -> Result<()> {
    match std::fs::remove_file(path(dir)) {
        Ok(()) => crate::sync_dir(dir),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

fn checksum(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x1000_0000_01b3);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Tmp(PathBuf);
    impl Tmp {
        fn new(tag: &str) -> Self {
            let p = std::env::temp_dir().join(format!(
                "adabt-base-{tag}-{}-{:?}",
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

    #[test]
    fn a_marker_round_trips_for_its_own_database_only() {
        let t = Tmp::new("round-trip");
        write(&t.0, 7, Lsn(42)).unwrap();
        assert_eq!(read(&t.0, 7), Some(Lsn(42)));
        assert_eq!(
            read(&t.0, 8),
            None,
            "another database's marker was accepted"
        );
    }

    #[test]
    fn a_damaged_marker_is_no_marker() {
        let t = Tmp::new("damaged");
        write(&t.0, 7, Lsn(42)).unwrap();
        let mut bytes = std::fs::read(path(&t.0)).unwrap();
        bytes[30] ^= 1;
        std::fs::write(path(&t.0), bytes).unwrap();
        assert_eq!(read(&t.0, 7), None);
    }

    #[test]
    fn discarding_is_idempotent() {
        let t = Tmp::new("discard");
        write(&t.0, 7, Lsn(1)).unwrap();
        discard(&t.0).unwrap();
        discard(&t.0).unwrap();
        assert_eq!(read(&t.0, 7), None);
    }
}
