//! The persisted catalog: which collections exist, and what they are called.
//!
//! Not to be confused with [`crate::catalog`], which serialises a single
//! `Schema`. This is the database's own record of *what there is* — the
//! name-to-id binding, the schema for each collection, the index definitions,
//! and how far through the log all of that reflects.
//!
//! # Why this has to exist before the log can be truncated
//!
//! `register_collection` assigns `CollectionId(next_collection_id)` in **write-
//! ahead log replay order**, and `scan_pages` reads that id back out of the
//! first four bytes of every heap slot. So the name-to-id binding is physically
//! embedded in every page and logically derived from replaying the log from byte
//! zero.
//!
//! Truncate the log and the surviving `CreateCollection` entries renumber from
//! zero. Every heap slot then points at a different collection than the one that
//! wrote it — or at none, in which case it is silently treated as an orphan and
//! left in place. No checksum notices, because the pages are intact; they are
//! simply being attributed to the wrong collection.
//!
//! That is the same failure this project keeps finding in itself: not a crash,
//! not an error, just plausible records containing the wrong data. So the
//! binding is written down rather than re-derived.
//!
//! # Authoritative, but not yet load-bearing
//!
//! Today the log is never truncated, so a missing catalog is recoverable by
//! walking the whole log — and that is exactly what happens. `log_start_lsn`
//! records the oldest entry the log still contains; while it is zero, the log is
//! complete and the fallback is sound. When truncation arrives, that field
//! becomes non-zero and the fallback becomes an error rather than a guess. The
//! field is here now so that truncation is a behaviour change rather than a
//! format change.

use adabt_core::error::{Error, Result};
use adabt_core::ids::Lsn;
use std::path::{Path, PathBuf};

const MAGIC: &[u8; 8] = b"aDaBtCat";
// Bumped to 2 when `next_record_id` was added to `CollectionMeta`. A catalog
// written by version 1 is simply not read — `read` returns `None` on a version
// it does not recognise, and recovery falls back to the log, which is exactly
// the degraded-but-correct path this file exists to make unnecessary rather
// than the path that makes it unsafe to change the format.
const FORMAT_VERSION: u32 = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollectionMeta {
    pub name: String,
    pub id: u32,
    /// Encoded by [`crate::catalog::encode_schema`].
    pub schema: Vec<u8>,
    /// The id an auto-allocated insert will use next. Persisted so a restart
    /// never hands out an id that was already used, even one whose record has
    /// since been deleted.
    pub next_record_id: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexMeta {
    pub collection: String,
    pub field: String,
    pub kind: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Catalog {
    pub collections: Vec<CollectionMeta>,
    pub indexes: Vec<IndexMeta>,
    /// The next id to hand out. Persisted rather than recomputed as
    /// `max(id) + 1`, because a dropped collection's id must never be reused
    /// while its records may still be lying in pages waiting to be reclaimed.
    pub next_collection_id: u32,
    /// The log position this catalog reflects. Entries above it still need replay.
    pub through_lsn: u64,
    /// The oldest entry the log still holds. Zero means the log is complete.
    pub log_start_lsn: u64,
}

pub fn path(dir: &Path) -> PathBuf {
    dir.join("catalog.adabt")
}

/// Write the catalog durably.
///
/// Temp file, fsync, rename — this one is authoritative, so it gets the same
/// ceremony as the superblock rather than the best-effort treatment the caches
/// get. A cache that fails to write costs a rebuild; this failing to write and
/// then being trusted costs the database.
pub fn write(dir: &Path, identity: u128, cat: &Catalog) -> Result<()> {
    let mut body = Vec::new();
    body.extend_from_slice(MAGIC);
    body.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
    body.extend_from_slice(&identity.to_le_bytes());
    body.extend_from_slice(&cat.next_collection_id.to_le_bytes());
    body.extend_from_slice(&cat.through_lsn.to_le_bytes());
    body.extend_from_slice(&cat.log_start_lsn.to_le_bytes());

    body.extend_from_slice(&(cat.collections.len() as u32).to_le_bytes());
    for c in &cat.collections {
        put_str(&c.name, &mut body);
        body.extend_from_slice(&c.id.to_le_bytes());
        body.extend_from_slice(&c.next_record_id.to_le_bytes());
        body.extend_from_slice(&(c.schema.len() as u32).to_le_bytes());
        body.extend_from_slice(&c.schema);
    }
    body.extend_from_slice(&(cat.indexes.len() as u32).to_le_bytes());
    for i in &cat.indexes {
        put_str(&i.collection, &mut body);
        put_str(&i.field, &mut body);
        put_str(&i.kind, &mut body);
    }
    let sum = checksum(&body);
    body.extend_from_slice(&sum.to_le_bytes());

    let tmp = dir.join("catalog.adabt.tmp");
    {
        use std::io::Write as _;
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&body)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path(dir))?;
    Ok(())
}

/// Read the catalog, if one exists and belongs to this database.
///
/// `None` for absent, damaged, foreign or unreadable — all of which mean the
/// same thing to the caller while the log is complete: rebuild from the log.
/// Once the log has been truncated the caller must treat `None` as fatal, which
/// is why [`Catalog::log_start_lsn`] exists.
pub fn read(dir: &Path, identity: u128) -> Option<Catalog> {
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
    if u128::from_le_bytes(r.take(16)?.try_into().ok()?) != identity {
        return None;
    }
    let mut cat = Catalog {
        next_collection_id: r.u32()?,
        through_lsn: r.u64()?,
        log_start_lsn: r.u64()?,
        ..Default::default()
    };
    let n = r.u32()? as usize;
    for _ in 0..n {
        let name = r.string()?;
        let id = r.u32()?;
        let next_record_id = r.u64()?;
        let len = r.u32()? as usize;
        cat.collections.push(CollectionMeta {
            name,
            id,
            next_record_id,
            schema: r.take(len)?.to_vec(),
        });
    }
    let n = r.u32()? as usize;
    for _ in 0..n {
        cat.indexes.push(IndexMeta {
            collection: r.string()?,
            field: r.string()?,
            kind: r.string()?,
        });
    }
    if r.pos != body.len() {
        return None;
    }
    Some(cat)
}

pub fn discard(dir: &Path) {
    let _ = std::fs::remove_file(path(dir));
}

/// Whether the log alone can still rebuild the catalog.
pub fn log_is_complete(cat: Option<&Catalog>) -> bool {
    cat.map(|c| c.log_start_lsn == 0).unwrap_or(true)
}

/// The error raised when the catalog is needed and is not there.
pub fn missing(start: Lsn) -> Error {
    Error::Corruption(format!(
        "the catalog is missing or unreadable and the log begins at {} rather than \
         the start, so the name-to-id binding cannot be rebuilt from it. Restore \
         the catalog or recover from a backup.",
        start.0
    ))
}

fn put_str(s: &str, out: &mut Vec<u8>) {
    out.extend_from_slice(&(s.len() as u32).to_le_bytes());
    out.extend_from_slice(s.as_bytes());
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
                "adabt-cat-{tag}-{}-{:?}",
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

    const ID: u128 = 0xfeed_face_feed_face_feed_face_feed_face;

    fn catalog() -> Catalog {
        Catalog {
            collections: vec![
                CollectionMeta {
                    name: "users".into(),
                    id: 0,
                    next_record_id: 42,
                    schema: vec![1, 2, 3, 4],
                },
                CollectionMeta {
                    name: "orders".into(),
                    id: 7,
                    next_record_id: 0,
                    schema: Vec::new(),
                },
            ],
            indexes: vec![IndexMeta {
                collection: "users".into(),
                field: "country".into(),
                kind: "hash".into(),
            }],
            next_collection_id: 8,
            through_lsn: 4242,
            log_start_lsn: 0,
        }
    }

    #[test]
    fn a_catalog_round_trips() {
        let t = Tmp::new("round");
        write(&t.0, ID, &catalog()).unwrap();
        assert_eq!(read(&t.0, ID), Some(catalog()));
    }

    #[test]
    fn an_absent_catalog_is_not_an_error() {
        let t = Tmp::new("absent");
        assert_eq!(read(&t.0, ID), None);
    }

    #[test]
    fn another_databases_catalog_is_refused() {
        // It would otherwise supply a name-to-id binding for the wrong data,
        // and every page would be attributed to the wrong collection.
        let t = Tmp::new("foreign");
        write(&t.0, ID, &catalog()).unwrap();
        assert_eq!(read(&t.0, ID ^ 1), None);
    }

    #[test]
    fn corruption_and_truncation_are_refused() {
        let t = Tmp::new("damaged");
        write(&t.0, ID, &catalog()).unwrap();
        let good = std::fs::read(path(&t.0)).unwrap();
        for cut in 0..good.len() {
            std::fs::write(path(&t.0), &good[..cut]).unwrap();
            assert_eq!(read(&t.0, ID), None, "length {cut} accepted");
        }
        for at in (0..good.len()).step_by(7) {
            let mut damaged = good.clone();
            damaged[at] ^= 0x20;
            std::fs::write(path(&t.0), &damaged).unwrap();
            assert_eq!(read(&t.0, ID), None, "damage at {at} accepted");
        }
    }

    #[test]
    fn ids_survive_exactly_so_pages_stay_attributable() {
        // The whole reason this file exists: a collection's id is embedded in
        // every one of its heap slots, so it must come back identical.
        let t = Tmp::new("ids");
        write(&t.0, ID, &catalog()).unwrap();
        let back = read(&t.0, ID).unwrap();
        assert_eq!(back.collections[1].id, 7);
        assert_eq!(
            back.next_collection_id, 8,
            "a dropped collection's id must not be handed out again"
        );
    }

    #[test]
    fn a_complete_log_can_still_rebuild_the_catalog() {
        assert!(log_is_complete(None));
        let mut c = catalog();
        assert!(log_is_complete(Some(&c)));
        c.log_start_lsn = 900;
        assert!(
            !log_is_complete(Some(&c)),
            "a truncated log must not be treated as rebuildable"
        );
    }

    #[test]
    fn an_empty_catalog_round_trips() {
        let t = Tmp::new("empty");
        let c = Catalog::default();
        write(&t.0, ID, &c).unwrap();
        assert_eq!(read(&t.0, ID), Some(c));
    }
}
