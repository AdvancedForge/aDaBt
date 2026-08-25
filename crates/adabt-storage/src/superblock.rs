//! The database's identity card, and the one file that must be read first.
//!
//! Two facts have to be established before a single page or log entry is
//! interpreted: **which database this is**, and **whether this build can read
//! it**. Everything else on disk — the heap, the log, the caches — is meaningless
//! or dangerous without them.
//!
//! # Why this exists now
//!
//! Until this file, the two *discardable caches* (`directory.rs`, `derived.rs`)
//! each carried a magic number and a format version, and the two *authoritative*
//! files — the heap and the write-ahead log — carried neither. That is exactly
//! backwards. A cache that cannot be read costs a rebuild; a heap that is
//! misread costs the data.
//!
//! The consequence is recorded in `docs/diagnosis.md`: "the record encoding has
//! changed twice this month with no migration path". Twice, silently, because
//! nothing on disk said what encoding it was. A database opened by a build that
//! disagrees with it now stops, and says both version numbers.
//!
//! # Refusing is the feature
//!
//! There is no attempt to guess. A superblock from a newer build is refused
//! outright rather than read optimistically, because the failure mode of
//! optimism here is not a crash — it is a plausible record with the wrong bytes
//! in it, which is the failure this project spends most of its test budget
//! trying to make impossible.
//!
//! An *older* version is a migration, and migrations are enumerated explicitly
//! in [`migrate`] rather than inferred. There are none yet; the mechanism is
//! here so that the next format change is a version bump rather than an
//! incident.

use adabt_core::error::{Error, Result};
use std::path::{Path, PathBuf};

const MAGIC: &[u8; 8] = b"aDaBtSB\0";

/// The on-disk format this build writes and understands.
///
/// Covers everything authoritative: the heap page layout, the record encodings,
/// the write-ahead log framing and the catalog. It is deliberately *one* number
/// rather than one per file — a database is only readable if all of it is, and
/// per-file versions invite the combinatorial question of which mixtures are
/// legal.
///
/// One deliberate exception proves the rule: the *catalog* file
/// (`metadata.rs`) carries its own version, because the catalog is the one
/// authoritative file whose total loss is recoverable by design — replaying
/// the log from the beginning rebuilds it. An unreadable catalog must be
/// refused as absent (`read → None`), never misparsed, and that contract is
/// pinned by `catalog_persistence.rs::an_unreadable_catalog_version_rebuilds_from_the_log`.
/// Everything else answers to this single number.
pub const FORMAT_VERSION: u32 = 1;

/// The file left behind by builds before the superblock existed.
///
/// Its only content was the database identity. It is adopted on first open and
/// then removed, which is the smallest possible migration and exists mostly to
/// prove the mechanism works on something real.
const LEGACY_IDENTITY: &str = "identity.adabt";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Superblock {
    pub format_version: u32,
    /// Distinguishes this database from another with an identical history.
    ///
    /// Written once and never again, so it survives every later operation and
    /// travels with the directory when it is copied — which is correct, because
    /// a copied directory *is* the same database, and a cache that came with it
    /// describes it accurately.
    pub identity: u128,
    /// Recorded so that a build compiled with a different page size refuses
    /// rather than reading every offset wrong.
    pub page_size: u32,
    pub created_unix_nanos: u128,
}

pub fn path(dir: &Path) -> PathBuf {
    dir.join("superblock.adabt")
}

/// Read the superblock, creating one if this is a new database.
pub fn open_or_create(dir: &Path, page_size: u32) -> Result<Superblock> {
    match std::fs::read(path(dir)) {
        Ok(bytes) => {
            let sb = decode(&bytes)?;
            check(&sb, page_size)?;
            Ok(migrate(sb, dir)?)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let sb = Superblock {
                format_version: FORMAT_VERSION,
                identity: adopt_or_mint_identity(dir)?,
                page_size,
                created_unix_nanos: now_nanos(),
            };
            write(dir, &sb)?;
            let _ = std::fs::remove_file(dir.join(LEGACY_IDENTITY));
            Ok(sb)
        }
        Err(e) => Err(Error::Io(e)),
    }
}

/// Whether this build may proceed.
fn check(sb: &Superblock, page_size: u32) -> Result<()> {
    if sb.format_version > FORMAT_VERSION {
        return Err(Error::IncompatibleFormat {
            found: sb.format_version,
            supported: FORMAT_VERSION,
        });
    }
    if sb.page_size != page_size {
        return Err(Error::Corruption(format!(
            "database was written with {}-byte pages; this build uses {page_size}",
            sb.page_size
        )));
    }
    Ok(())
}

/// Bring an older database up to the current format.
///
/// Enumerated rather than inferred: each step names the version it upgrades from
/// and what it changes, so the set of paths through this function is finite and
/// readable. There are none yet, which is the point at which to write it.
fn migrate(sb: Superblock, dir: &Path) -> Result<Superblock> {
    let mut sb = sb;
    // for v in sb.format_version..FORMAT_VERSION { match v { .. } }
    if sb.format_version < FORMAT_VERSION {
        sb.format_version = FORMAT_VERSION;
        write(dir, &sb)?;
    }
    Ok(sb)
}

/// Take the identity a pre-superblock database already had, or mint one.
///
/// The value only has to be unique, not unpredictable, so it is a clock reading
/// mixed with the process id and a per-process counter. That covers the case the
/// clock alone does not: two databases created by one process inside the same
/// nanosecond.
fn adopt_or_mint_identity(dir: &Path) -> Result<u128> {
    if let Ok(bytes) = std::fs::read(dir.join(LEGACY_IDENTITY)) {
        if let Ok(b) = <[u8; 16]>::try_from(bytes.as_slice()) {
            return Ok(u128::from_le_bytes(b));
        }
    }
    static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed) as u128;
    Ok(now_nanos() ^ ((std::process::id() as u128) << 80) ^ (seq << 112))
}

pub(crate) fn now_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

/// Write the superblock durably.
///
/// Temp file, fsync, rename. A superblock torn in half is a database nobody can
/// open, so this is the one write in the system that gets the full ceremony
/// regardless of the durability policy — the policy governs how much *data* loss
/// is acceptable, never whether the database can be identified afterwards.
pub fn write(dir: &Path, sb: &Superblock) -> Result<()> {
    let mut body = Vec::with_capacity(64);
    body.extend_from_slice(MAGIC);
    body.extend_from_slice(&sb.format_version.to_le_bytes());
    body.extend_from_slice(&sb.identity.to_le_bytes());
    body.extend_from_slice(&sb.page_size.to_le_bytes());
    body.extend_from_slice(&sb.created_unix_nanos.to_le_bytes());
    let sum = checksum(&body);
    body.extend_from_slice(&sum.to_le_bytes());

    let tmp = dir.join("superblock.adabt.tmp");
    {
        use std::io::Write as _;
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&body)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path(dir))?;
    Ok(())
}

fn decode(bytes: &[u8]) -> Result<Superblock> {
    const LEN: usize = 8 + 4 + 16 + 4 + 16 + 8;
    if bytes.len() != LEN {
        return Err(Error::Corruption(format!(
            "superblock is {} bytes, expected {LEN}",
            bytes.len()
        )));
    }
    let (body, tail) = bytes.split_at(LEN - 8);
    let stored = u64::from_le_bytes(tail.try_into().expect("8 bytes"));
    if checksum(body) != stored {
        return Err(Error::Corruption("superblock checksum mismatch".into()));
    }
    if &body[..8] != MAGIC {
        return Err(Error::Corruption(
            "this directory does not hold an aDaBt database".into(),
        ));
    }
    let u32_at = |i: usize| u32::from_le_bytes(body[i..i + 4].try_into().expect("4 bytes"));
    let u128_at = |i: usize| u128::from_le_bytes(body[i..i + 16].try_into().expect("16 bytes"));
    Ok(Superblock {
        format_version: u32_at(8),
        identity: u128_at(12),
        page_size: u32_at(28),
        created_unix_nanos: u128_at(32),
    })
}

/// FNV-1a, as everywhere else in this crate. Enough to catch a torn write.
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
            let mut p = std::env::temp_dir();
            p.push(format!(
                "adabt-sb-{tag}-{}-{:?}",
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

    const PAGE: u32 = 8192;

    #[test]
    fn a_new_database_gets_a_superblock() {
        let t = Tmp::new("new");
        let sb = open_or_create(&t.0, PAGE).unwrap();
        assert_eq!(sb.format_version, FORMAT_VERSION);
        assert_eq!(sb.page_size, PAGE);
        assert!(sb.identity != 0);
        assert!(path(&t.0).exists());
    }

    #[test]
    fn reopening_returns_the_same_identity() {
        let t = Tmp::new("stable");
        let first = open_or_create(&t.0, PAGE).unwrap();
        let second = open_or_create(&t.0, PAGE).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn two_databases_get_different_identities() {
        // The property the derived and directory caches depend on: two databases
        // built by identical operations must still be distinguishable, or one
        // adopts the other's cache and every indexed query returns wrong rows.
        let (a, b) = (Tmp::new("id-a"), Tmp::new("id-b"));
        assert_ne!(
            open_or_create(&a.0, PAGE).unwrap().identity,
            open_or_create(&b.0, PAGE).unwrap().identity
        );
    }

    #[test]
    fn a_newer_format_is_refused_rather_than_read() {
        let t = Tmp::new("newer");
        let mut sb = open_or_create(&t.0, PAGE).unwrap();
        sb.format_version = FORMAT_VERSION + 7;
        write(&t.0, &sb).unwrap();
        match open_or_create(&t.0, PAGE) {
            Err(Error::IncompatibleFormat { found, supported }) => {
                assert_eq!(found, FORMAT_VERSION + 7);
                assert_eq!(supported, FORMAT_VERSION);
            }
            other => panic!("a future format was not refused: {other:?}"),
        }
    }

    #[test]
    fn a_different_page_size_is_refused() {
        let t = Tmp::new("pagesize");
        open_or_create(&t.0, PAGE).unwrap();
        let e = open_or_create(&t.0, PAGE * 2).unwrap_err().to_string();
        assert!(e.contains("8192-byte pages"), "{e}");
    }

    #[test]
    fn corruption_anywhere_is_refused() {
        let t = Tmp::new("corrupt");
        open_or_create(&t.0, PAGE).unwrap();
        let good = std::fs::read(path(&t.0)).unwrap();
        for at in 0..good.len() {
            let mut damaged = good.clone();
            damaged[at] ^= 0x40;
            std::fs::write(path(&t.0), &damaged).unwrap();
            assert!(
                open_or_create(&t.0, PAGE).is_err(),
                "damage at byte {at} went unnoticed"
            );
        }
    }

    #[test]
    fn a_truncated_superblock_is_refused_at_every_length() {
        let t = Tmp::new("short");
        open_or_create(&t.0, PAGE).unwrap();
        let good = std::fs::read(path(&t.0)).unwrap();
        for cut in 0..good.len() {
            std::fs::write(path(&t.0), &good[..cut]).unwrap();
            assert!(open_or_create(&t.0, PAGE).is_err(), "length {cut} accepted");
        }
    }

    #[test]
    fn a_directory_that_is_not_a_database_says_so() {
        let t = Tmp::new("foreign");
        std::fs::write(path(&t.0), vec![0u8; 56]).unwrap();
        let e = open_or_create(&t.0, PAGE).unwrap_err().to_string();
        assert!(
            e.contains("checksum") || e.contains("not hold an aDaBt"),
            "{e}"
        );
    }

    #[test]
    fn a_pre_superblock_database_keeps_its_identity() {
        // The smallest possible migration, and the reason the mechanism is worth
        // having: the caches are stamped with this value, so inventing a new one
        // would silently invalidate every one of them.
        let t = Tmp::new("legacy");
        let legacy: u128 = 0x0123_4567_89ab_cdef_0123_4567_89ab_cdef;
        std::fs::write(t.0.join(LEGACY_IDENTITY), legacy.to_le_bytes()).unwrap();

        let sb = open_or_create(&t.0, PAGE).unwrap();
        assert_eq!(sb.identity, legacy, "the old identity was thrown away");
        assert!(
            !t.0.join(LEGACY_IDENTITY).exists(),
            "the superseded file was left behind"
        );
    }
}
