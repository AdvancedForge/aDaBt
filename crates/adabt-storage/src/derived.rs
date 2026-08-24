//! An on-disk cache of derived representations.
//!
//! Indexes, column stores and directly-addressed arrays are all rebuildable
//! from the primary — that is the invariant the whole engine rests on, and it
//! is why the optimizer can switch them on and off under a live workload. It is
//! also why losing one costs a scan and never a record.
//!
//! The cost of a scan, though, is real: reopening a database rebuilds every
//! index by decoding every record in the heap. This file is how that is avoided
//! when it safely can be. **Nothing here is authoritative.** It is a cache in
//! the strict sense: it may be deleted, truncated, corrupted or absent, and the
//! only consequence is that the database rebuilds what it holds.
//!
//! # Why a stamp, and why this stamp
//!
//! Reading a stale index would produce wrong answers — the one failure mode a
//! cache of derived state can have that is not merely slow. So the cache carries
//! a [`Stamp`] describing the primary it was built from, and it is used only on
//! an exact match.
//!
//! The stamp is deliberately over-specified. A log sequence number alone would
//! identify the state of *one* database, but says nothing if a file is copied
//! between directories; adding the heap file's length and every collection's
//! live record count means a false match requires a database that is, to all
//! available evidence, the same database in the same state. Being conservative
//! here costs a rebuild. Being wrong here costs correctness, so the trade is not
//! close.
//!
//! Every failure to read is reported as "no cache" rather than as an error.
//! A damaged cache is not a damaged database, and the correct response to one is
//! to rebuild without comment, not to refuse to open.

use adabt_core::error::Result;
use std::path::{Path, PathBuf};

const MAGIC: &[u8; 8] = b"aDaBtDrv";
const FORMAT_VERSION: u32 = 1;

/// What the primary looked like when the cache was written.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Stamp {
    /// Which database this is. Not which *state* — which database.
    ///
    /// Without it the stamp describes a shape rather than an identity, and two
    /// databases built by the same sequence of operations over different values
    /// stamp identically: same log position, same heap length, same row counts,
    /// completely different keys. Copy one directory's cache into the other and
    /// it is adopted, and every indexed query then returns the wrong rows
    /// without any error being raised anywhere.
    ///
    /// That is not hypothetical. It is what the first version of this file did,
    /// and the test that found it is still there.
    pub identity: u128,
    pub lsn: u64,
    pub heap_bytes: u64,
    /// Live record count per collection, sorted by name.
    pub counts: Vec<(String, u64)>,
}

impl Stamp {
    fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.identity.to_le_bytes());
        out.extend_from_slice(&self.lsn.to_le_bytes());
        out.extend_from_slice(&self.heap_bytes.to_le_bytes());
        out.extend_from_slice(&(self.counts.len() as u32).to_le_bytes());
        for (name, n) in &self.counts {
            put_str(name, out);
            out.extend_from_slice(&n.to_le_bytes());
        }
    }

    fn decode(r: &mut Reader<'_>) -> Option<Stamp> {
        let identity = u128::from_le_bytes(r.take(16)?.try_into().ok()?);
        let lsn = r.u64()?;
        let heap_bytes = r.u64()?;
        let n = r.u32()? as usize;
        // A length read out of a possibly-corrupt file is attacker-shaped even
        // when there is no attacker: reserve nothing on the strength of it.
        let mut counts = Vec::new();
        for _ in 0..n {
            let name = r.string()?;
            counts.push((name, r.u64()?));
        }
        Some(Stamp {
            identity,
            lsn,
            heap_bytes,
            counts,
        })
    }
}

pub fn path(dir: &Path) -> PathBuf {
    dir.join("derived.adabt")
}

/// Write the cache, replacing whatever was there.
///
/// Written to a temporary file and renamed, so a crash part-way through leaves
/// either the previous cache or none — never a half-written one that happens to
/// checksum correctly over a truncated tail.
pub fn write(dir: &Path, stamp: &Stamp, blobs: &[(String, Vec<u8>)]) -> Result<()> {
    let mut body = Vec::new();
    body.extend_from_slice(MAGIC);
    body.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
    stamp.encode(&mut body);
    body.extend_from_slice(&(blobs.len() as u32).to_le_bytes());
    for (name, blob) in blobs {
        put_str(name, &mut body);
        body.extend_from_slice(&(blob.len() as u64).to_le_bytes());
        body.extend_from_slice(blob);
    }
    let sum = checksum(&body);
    body.extend_from_slice(&sum.to_le_bytes());

    let tmp = dir.join("derived.adabt.tmp");
    std::fs::write(&tmp, &body)?;
    std::fs::rename(&tmp, path(dir))?;
    Ok(())
}

/// Read the cache, if there is one and it describes this exact primary.
///
/// Returns `None` for every reason a cache can be unusable — absent, truncated,
/// corrupt, written by another format version, or built from different data —
/// because the caller's response to all of them is the same.
pub fn read(dir: &Path, stamp: &Stamp) -> Option<Vec<(String, Vec<u8>)>> {
    let bytes = std::fs::read(path(dir)).ok()?;
    if bytes.len() < MAGIC.len() + 8 {
        return None;
    }
    let (body, tail) = bytes.split_at(bytes.len() - 8);
    let stored: u64 = u64::from_le_bytes(tail.try_into().ok()?);
    if checksum(body) != stored {
        return None;
    }
    let mut r = Reader { buf: body, pos: 0 };
    if r.take(MAGIC.len())? != MAGIC {
        return None;
    }
    if r.u32()? != FORMAT_VERSION {
        return None;
    }
    if Stamp::decode(&mut r)? != *stamp {
        return None;
    }
    let n = r.u32()? as usize;
    let mut out = Vec::new();
    for _ in 0..n {
        let name = r.string()?;
        let len = r.u64()? as usize;
        out.push((name, r.take(len)?.to_vec()));
    }
    Some(out)
}

/// Remove the cache. Called when what it holds is known to be out of date.
pub fn discard(dir: &Path) {
    let _ = std::fs::remove_file(path(dir));
}

/// FNV-1a, the same function the page checksum uses.
///
/// Not a cryptographic digest and does not need to be: it is here to catch a
/// truncated write or a flipped bit, both of which it catches, and the stamp
/// does the work of establishing that the contents describe this database.
fn checksum(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x1000_0000_01b3);
    }
    h
}

fn put_str(s: &str, out: &mut Vec<u8>) {
    out.extend_from_slice(&(s.len() as u32).to_le_bytes());
    out.extend_from_slice(s.as_bytes());
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
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
    fn string(&mut self) -> Option<String> {
        let n = self.u32()? as usize;
        String::from_utf8(self.take(n)?.to_vec()).ok()
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
                "adabt-derived-{tag}-{}-{:?}",
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
            identity: 0x0123_4567_89ab_cdef_0123_4567_89ab_cdef,
            lsn: 42,
            heap_bytes: 8192,
            counts: vec![("users".into(), 1000), ("orders".into(), 7)],
        }
    }

    fn blobs() -> Vec<(String, Vec<u8>)> {
        vec![
            ("idx:users:country:hash".into(), vec![1, 2, 3, 4]),
            ("idx:users:age:btree".into(), vec![]),
        ]
    }

    #[test]
    fn a_cache_round_trips() {
        let t = Tmp::new("round");
        write(&t.0, &stamp(), &blobs()).unwrap();
        assert_eq!(read(&t.0, &stamp()), Some(blobs()));
    }

    #[test]
    fn an_absent_cache_is_not_an_error() {
        let t = Tmp::new("absent");
        assert_eq!(read(&t.0, &stamp()), None);
    }

    #[test]
    fn a_cache_built_from_different_data_is_refused() {
        // Every component of the stamp on its own is enough to reject.
        let t = Tmp::new("stale");
        write(&t.0, &stamp(), &blobs()).unwrap();

        let mut later = stamp();
        later.lsn += 1;
        assert_eq!(read(&t.0, &later), None, "a newer log was accepted");

        let mut grown = stamp();
        grown.heap_bytes += 8192;
        assert_eq!(read(&t.0, &grown), None, "a grown heap was accepted");

        let mut fewer = stamp();
        fewer.counts[0].1 -= 1;
        assert_eq!(read(&t.0, &fewer), None, "a changed row count was accepted");

        let mut renamed = stamp();
        renamed.counts[0].0 = "customers".into();
        assert_eq!(
            read(&t.0, &renamed),
            None,
            "a renamed collection was accepted"
        );

        let mut elsewhere = stamp();
        elsewhere.identity ^= 1;
        assert_eq!(
            read(&t.0, &elsewhere),
            None,
            "another database's cache was accepted"
        );
    }

    #[test]
    fn a_truncated_cache_is_refused_at_every_length() {
        let t = Tmp::new("truncated");
        write(&t.0, &stamp(), &blobs()).unwrap();
        let full = std::fs::read(path(&t.0)).unwrap();
        for cut in 0..full.len() {
            std::fs::write(path(&t.0), &full[..cut]).unwrap();
            assert_eq!(
                read(&t.0, &stamp()),
                None,
                "a cache truncated to {cut} bytes was accepted"
            );
        }
    }

    #[test]
    fn a_single_flipped_bit_is_refused() {
        let t = Tmp::new("flipped");
        write(&t.0, &stamp(), &blobs()).unwrap();
        let full = std::fs::read(path(&t.0)).unwrap();
        for byte in 0..full.len() {
            let mut damaged = full.clone();
            damaged[byte] ^= 0x01;
            std::fs::write(path(&t.0), &damaged).unwrap();
            assert_eq!(
                read(&t.0, &stamp()),
                None,
                "a bit flip at byte {byte} went unnoticed"
            );
        }
    }

    #[test]
    fn a_cache_from_another_format_version_is_refused() {
        let t = Tmp::new("version");
        write(&t.0, &stamp(), &blobs()).unwrap();
        let mut full = std::fs::read(path(&t.0)).unwrap();
        let at = MAGIC.len();
        full[at..at + 4].copy_from_slice(&(FORMAT_VERSION + 1).to_le_bytes());
        // Re-checksum, so it is *only* the version that rejects it.
        let n = full.len();
        let sum = checksum(&full[..n - 8]);
        full[n - 8..].copy_from_slice(&sum.to_le_bytes());
        std::fs::write(path(&t.0), &full).unwrap();
        assert_eq!(read(&t.0, &stamp()), None);
    }

    #[test]
    fn writing_replaces_rather_than_appends() {
        let t = Tmp::new("replace");
        write(&t.0, &stamp(), &blobs()).unwrap();
        let second = vec![("only".to_string(), vec![9u8; 100])];
        write(&t.0, &stamp(), &second).unwrap();
        assert_eq!(read(&t.0, &stamp()), Some(second));
    }

    #[test]
    fn discarding_leaves_no_cache_and_no_temporary_file() {
        let t = Tmp::new("discard");
        write(&t.0, &stamp(), &blobs()).unwrap();
        discard(&t.0);
        assert_eq!(read(&t.0, &stamp()), None);
        let leftovers: Vec<_> = std::fs::read_dir(&t.0)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn an_empty_cache_is_distinguishable_from_no_cache() {
        // "This database has no indexes" and "there is no cache" are different
        // facts, and confusing them would make an index-free database rescan
        // its heap on every start for nothing.
        let t = Tmp::new("empty");
        write(&t.0, &stamp(), &[]).unwrap();
        assert_eq!(read(&t.0, &stamp()), Some(Vec::new()));
    }
}
