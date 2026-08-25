//! The name-to-id binding, and why it has to be written down.
//!
//! A collection's `CollectionId` is stored in the first four bytes of every one
//! of its heap slots. Until the catalog existed, that id came from *write-ahead
//! log replay order* — the nth `CreateCollection` entry got id n. Which is fine
//! exactly as long as the log still starts at the beginning.
//!
//! The moment it does not, the surviving entries renumber, every page is
//! attributed to a different collection than the one that wrote it, and nothing
//! notices: the pages are intact, the checksums pass, the records decode. They
//! just belong to the wrong table.
//!
//! These tests are about that binding surviving.

use adabt_core::ids::RecordId;
use adabt_core::policy::Durability;
use adabt_core::record::Record;
use adabt_core::schema::Schema;
use adabt_core::store::LogicalStore;
use adabt_storage::heap::HeapStore;
use std::path::{Path, PathBuf};

struct Tmp(PathBuf);
impl Tmp {
    fn new(tag: &str) -> Self {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "adabt-catp-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&p);
        Tmp(p)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}
impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn rec(tag: &str, i: u64) -> Record {
    Record::new().with("who", tag).with("i", i)
}

/// Three collections, created in a known order, each with distinguishable rows.
fn seeded(dir: &Path) -> HeapStore {
    let mut h = HeapStore::open(dir, Durability::Strict, 32).unwrap();
    for name in ["alpha", "beta", "gamma"] {
        h.create_collection(name, Schema::dynamic()).unwrap();
        for i in 0..200u64 {
            h.insert(name, RecordId(i), rec(name, i)).unwrap();
        }
    }
    h.checkpoint().unwrap();
    h
}

fn assert_intact(h: &mut HeapStore, note: &str) {
    for name in ["alpha", "beta", "gamma"] {
        assert_eq!(h.count(name).unwrap(), 200, "{note}: {name} lost rows");
        for i in 0..200u64 {
            assert_eq!(
                h.get(name, RecordId(i)).unwrap(),
                Some(rec(name, i)),
                "{note}: {name}/{i} is not the record that was written"
            );
        }
    }
}

#[test]
fn a_checkpoint_writes_a_catalog() {
    let t = Tmp::new("written");
    drop(seeded(t.path()));
    assert!(adabt_storage::metadata::path(t.path()).exists());
}

#[test]
fn collection_ids_survive_a_restart_exactly() {
    let t = Tmp::new("ids");
    let before = {
        let h = seeded(t.path());
        h.collection_ids()
    };
    let h = HeapStore::open(t.path(), Durability::Strict, 32).unwrap();
    assert_eq!(
        h.collection_ids(),
        before,
        "a collection was renumbered across a restart"
    );
}

#[test]
fn the_catalog_and_a_full_log_walk_agree() {
    // The catalog is a shortcut for something the log can still compute. While
    // that is true, the two must produce the same answer — and this is the test
    // that keeps it true, because once the log is truncated only one of them
    // will still be able to answer at all.
    let t = Tmp::new("agree");
    let with_catalog = {
        let h = seeded(t.path());
        h.collection_ids()
    };
    adabt_storage::metadata::discard(t.path());
    let mut rebuilt = HeapStore::open(t.path(), Durability::Strict, 32).unwrap();
    assert_eq!(
        rebuilt.collection_ids(),
        with_catalog,
        "replaying the log produced a different binding than the catalog"
    );
    assert_intact(&mut rebuilt, "rebuilt from log");
}

#[test]
fn every_record_still_belongs_to_the_collection_that_wrote_it() {
    let t = Tmp::new("attribution");
    drop(seeded(t.path()));
    let mut h = HeapStore::open(t.path(), Durability::Strict, 32).unwrap();
    assert_intact(&mut h, "after restart");
}

#[test]
fn a_dropped_collections_id_is_never_handed_out_again() {
    // Its records may still be lying in pages waiting to be reclaimed. Reusing
    // the id would make them reappear inside whatever collection got it next.
    let t = Tmp::new("reuse");
    let ids_before = {
        let mut h = seeded(t.path());
        h.drop_collection("beta").unwrap();
        h.create_collection("delta", Schema::dynamic()).unwrap();
        h.checkpoint().unwrap();
        h.collection_ids()
    };
    let beta_id = 1u32; // alpha=0, beta=1, gamma=2
    let delta_id = ids_before
        .iter()
        .find(|(n, _)| n == "delta")
        .map(|(_, id)| *id)
        .unwrap();
    assert_ne!(delta_id, beta_id, "a dropped collection's id was reused");

    let h = HeapStore::open(t.path(), Durability::Strict, 32).unwrap();
    assert_eq!(h.collection_ids(), ids_before);
}

#[test]
fn a_foreign_catalog_is_ignored_rather_than_adopted() {
    // Two databases built by identical operations have identical catalogs except
    // for the identity stamp. Without that stamp, one would adopt the other's
    // name-to-id binding and every page would be misattributed.
    let (a, b) = (Tmp::new("foreign-a"), Tmp::new("foreign-b"));
    drop(seeded(a.path()));
    {
        // Same names, created in a different order, so the bindings genuinely
        // differ and adopting the wrong one would be visible.
        let mut h = HeapStore::open(b.path(), Durability::Strict, 32).unwrap();
        for name in ["gamma", "beta", "alpha"] {
            h.create_collection(name, Schema::dynamic()).unwrap();
            for i in 0..200u64 {
                h.insert(name, RecordId(i), rec(name, i)).unwrap();
            }
        }
        h.checkpoint().unwrap();
    }
    std::fs::copy(
        adabt_storage::metadata::path(b.path()),
        adabt_storage::metadata::path(a.path()),
    )
    .unwrap();

    let mut h = HeapStore::open(a.path(), Durability::Strict, 32).unwrap();
    assert_intact(&mut h, "after a foreign catalog was planted");
}

#[test]
fn a_damaged_catalog_falls_back_to_the_log() {
    let t = Tmp::new("damaged");
    let before = {
        let h = seeded(t.path());
        h.collection_ids()
    };
    let good = std::fs::read(adabt_storage::metadata::path(t.path())).unwrap();
    for at in [0, 12, good.len() / 2, good.len() - 2] {
        let mut damaged = good.clone();
        damaged[at] ^= 0xff;
        std::fs::write(adabt_storage::metadata::path(t.path()), &damaged).unwrap();
        let mut h = HeapStore::open(t.path(), Durability::Strict, 32).unwrap();
        assert_eq!(
            h.collection_ids(),
            before,
            "damage at {at} changed the binding"
        );
        assert_intact(&mut h, "after catalog damage");
    }
}

#[test]
fn writes_after_the_checkpoint_are_replayed_on_top_of_the_catalog() {
    // The catalog reflects a log position. Everything after it is still in the
    // log and must still be applied.
    let t = Tmp::new("after");
    {
        let mut h = seeded(t.path());
        h.create_collection("epsilon", Schema::dynamic()).unwrap();
        for i in 0..50u64 {
            h.insert("epsilon", RecordId(i), rec("epsilon", i)).unwrap();
        }
        // No second checkpoint: the catalog on disk does not know about epsilon.
    }
    let mut h = HeapStore::open(t.path(), Durability::Strict, 32).unwrap();
    assert_eq!(h.count("epsilon").unwrap(), 50);
    assert_intact(&mut h, "with a post-checkpoint collection");
}

/// The version contract, end to end: a catalog this build cannot parse is
/// treated as absent — never misparsed — and the database rebuilds from the
/// log with every record intact. This is why the catalog's format version may
/// bump independently of the superblock's: losing the cache costs a replay,
/// not the data.
#[test]
fn an_unreadable_catalog_version_rebuilds_from_the_log() {
    let t = Tmp::new("version");
    {
        let mut h = seeded(t.path());
        h.insert("alpha", RecordId(200), rec("alpha", 200)).unwrap();
    }
    // Corrupt only the version byte in the authoritative catalog file.
    let p = adabt_storage::metadata::path(t.path());
    assert!(p.exists(), "checkpoint should have written a catalog");
    let mut bytes = std::fs::read(&p).unwrap();
    bytes[8] = bytes[8].wrapping_add(1);
    std::fs::write(&p, &bytes).unwrap();

    let mut h = HeapStore::open(t.path(), Durability::Strict, 32).unwrap();
    // The catalog was unreadable, so everything comes back by replay —
    // including the post-checkpoint insert.
    assert_eq!(h.count("alpha").unwrap(), 201);
    assert_eq!(h.count("beta").unwrap(), 200);
    assert_eq!(h.count("gamma").unwrap(), 200);
    for i in 0..200u64 {
        assert_eq!(
            h.get("alpha", RecordId(i)).unwrap(),
            Some(rec("alpha", i)),
            "alpha/{i} is not the record that was written"
        );
    }
}
