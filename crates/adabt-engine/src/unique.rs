//! Unique constraints: fields where two records may not share a value.
//!
//! **A logical decision, not a physical one, so it lives at the engine layer
//! rather than in storage.** Enforcing it needs an index to check against
//! efficiently, but the index is a means, not the point — dropping and
//! rebuilding it must never make a duplicate value legal, which is exactly the
//! property `Database::pinned_scopes` exists to protect: the
//! adaptive driver may retract an index it judges unused, but never one a
//! constraint depends on. A dropped constraint releases the pin; the index
//! itself is left in place, since whether it is still worth keeping for
//! ordinary querying is the optimizer's decision, not this one's to make.
//!
//! # What this does not do
//!
//! **A sharded database only enforces a constraint correctly if the field it
//! guards determines the shard.** Each shard checks its own data against its
//! own index; nothing coordinates the check across shards, so two records with
//! the same value can land on different shards and both be accepted. This is
//! the same honest limitation `ShardedDatabase::insert_batch` documents for
//! atomicity, for the same reason: cross-shard coordination is cross-shard
//! transaction machinery, and that is its own milestone, not a detail of this
//! one.

use adabt_core::error::Result;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

const MAGIC: &[u8; 8] = b"aDaBtUnq";
const FORMAT_VERSION: u32 = 1;

/// The set of `(collection, field)` pairs currently constrained.
///
/// A `BTreeSet` rather than a `HashMap`, because there is no value to look up —
/// only membership — and a deterministic iteration order makes the persisted
/// file's bytes deterministic too, which is worth having for anything written
/// to disk.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UniqueConstraints {
    fields: BTreeSet<(String, String)>,
}

impl UniqueConstraints {
    pub fn contains(&self, collection: &str, field: &str) -> bool {
        self.fields
            .contains(&(collection.to_string(), field.to_string()))
    }

    pub fn add(&mut self, collection: &str, field: &str) -> bool {
        self.fields
            .insert((collection.to_string(), field.to_string()))
    }

    pub fn remove(&mut self, collection: &str, field: &str) -> bool {
        self.fields
            .remove(&(collection.to_string(), field.to_string()))
    }

    /// Every constrained field on `collection`.
    pub fn on<'a>(&'a self, collection: &str) -> impl Iterator<Item = &'a str> + 'a {
        let collection = collection.to_string();
        self.fields
            .iter()
            .filter(move |(c, _)| *c == collection)
            .map(|(_, f)| f.as_str())
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.fields.iter().map(|(c, f)| (c.as_str(), f.as_str()))
    }

    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }
}

fn path(dir: &Path) -> PathBuf {
    dir.join("unique.adabt")
}

/// Write the constraint set durably.
///
/// Every call fsyncs. That is deliberate: constraints change rarely — an
/// administrative action, not a hot-path write — so there is nothing to batch,
/// and a constraint that existed in memory but not on disk is one a crash could
/// silently drop, after which the very violation it existed to prevent becomes
/// possible again with no warning.
pub fn write(dir: &Path, constraints: &UniqueConstraints) -> Result<()> {
    let mut body = Vec::new();
    body.extend_from_slice(MAGIC);
    body.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
    body.extend_from_slice(&(constraints.fields.len() as u32).to_le_bytes());
    for (c, f) in &constraints.fields {
        put_str(c, &mut body);
        put_str(f, &mut body);
    }
    let sum = checksum(&body);
    body.extend_from_slice(&sum.to_le_bytes());

    let tmp = dir.join("unique.adabt.tmp");
    {
        use std::io::Write as _;
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(&body)?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, path(dir))?;
    Ok(())
}

/// Read the constraint set. Absent, damaged or foreign-version files all read
/// as "no constraints" — there is nowhere else this data lives, so unlike the
/// derived and directory caches there is no fallback to rebuild from, and a
/// caller that cannot read this file has genuinely lost the declarations. It
/// has not, however, lost any data: every record already written still
/// satisfies whatever constraints were in force when it was written, and the
/// constraint can simply be re-declared.
pub fn read(dir: &Path) -> UniqueConstraints {
    let Ok(bytes) = std::fs::read(path(dir)) else {
        return UniqueConstraints::default();
    };
    (|| -> Option<UniqueConstraints> {
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
        let n = r.u32()? as usize;
        let mut fields = BTreeSet::new();
        for _ in 0..n {
            let c = r.string()?;
            let f = r.string()?;
            fields.insert((c, f));
        }
        if r.pos != body.len() {
            return None;
        }
        Some(UniqueConstraints { fields })
    })()
    .unwrap_or_default()
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
                "adabt-uniq-{tag}-{}-{:?}",
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
    fn a_constraint_set_round_trips() {
        let t = Tmp::new("round");
        let mut c = UniqueConstraints::default();
        c.add("users", "email");
        c.add("users", "handle");
        c.add("orders", "external_id");
        write(&t.0, &c).unwrap();
        assert_eq!(read(&t.0), c);
    }

    #[test]
    fn an_absent_file_reads_as_no_constraints() {
        let t = Tmp::new("absent");
        assert!(read(&t.0).is_empty());
    }

    #[test]
    fn removing_a_constraint_and_reloading_reflects_it() {
        let t = Tmp::new("remove");
        let mut c = UniqueConstraints::default();
        c.add("users", "email");
        write(&t.0, &c).unwrap();
        c.remove("users", "email");
        write(&t.0, &c).unwrap();
        assert!(read(&t.0).is_empty());
    }

    #[test]
    fn corruption_and_truncation_read_as_no_constraints_rather_than_crash() {
        let t = Tmp::new("damaged");
        let mut c = UniqueConstraints::default();
        c.add("users", "email");
        write(&t.0, &c).unwrap();
        let good = std::fs::read(path(&t.0)).unwrap();
        for cut in 0..good.len() {
            std::fs::write(path(&t.0), &good[..cut]).unwrap();
            assert!(read(&t.0).is_empty(), "length {cut} was accepted");
        }
        for at in 0..good.len() {
            let mut damaged = good.clone();
            damaged[at] ^= 0x11;
            std::fs::write(path(&t.0), &damaged).unwrap();
            assert!(read(&t.0).is_empty(), "damage at {at} was accepted");
        }
    }

    #[test]
    fn on_filters_by_collection() {
        let mut c = UniqueConstraints::default();
        c.add("users", "email");
        c.add("orders", "external_id");
        let users: Vec<&str> = c.on("users").collect();
        assert_eq!(users, vec!["email"]);
    }
}
