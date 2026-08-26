//! Catalog v3 → v4 migration: v3 accepted, defaults, v4 round-trips, downgrade read.

use adabt_storage::metadata::{self, Catalog};
use std::path::PathBuf;

struct Tmp(PathBuf);
impl Tmp {
    fn new(tag: &str) -> Self {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "adabt-cat-upgrade-{tag}-{}-{:?}",
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

const ID: u128 = 0xaaaa_bbbb_cccc_dddd_eeee_ffff_0000_1111;

fn catalog_v4(delta: bool, tpc: bool) -> Catalog {
    Catalog {
        delta_encoding: delta,
        thread_per_core: tpc,
        ..Catalog::default()
    }
}

// Craft a v3 file the old way (no delta/thread bytes) to prove forward-compat.
fn write_v3(dir: &std::path::Path, identity: u128, cat: &Catalog) {
    const MAGIC: &[u8; 8] = b"aDaBtCat";
    const V3: u32 = 3;
    let mut body = Vec::new();
    body.extend_from_slice(MAGIC);
    body.extend_from_slice(&V3.to_le_bytes());
    body.extend_from_slice(&identity.to_le_bytes());
    body.extend_from_slice(&cat.next_collection_id.to_le_bytes());
    body.extend_from_slice(&cat.through_lsn.to_le_bytes());
    body.extend_from_slice(&cat.log_start_lsn.to_le_bytes());
    // v3 has no delta/thread bytes here
    body.extend_from_slice(&(cat.collections.len() as u32).to_le_bytes());
    for c in &cat.collections {
        body.extend_from_slice(&(c.name.len() as u32).to_le_bytes());
        body.extend_from_slice(c.name.as_bytes());
        body.extend_from_slice(&c.id.to_le_bytes());
        body.extend_from_slice(&c.next_record_id.to_le_bytes());
        body.extend_from_slice(&(c.schema.len() as u32).to_le_bytes());
        body.extend_from_slice(&c.schema);
        match &c.cluster_field {
            Some(f) => {
                body.push(1);
                body.extend_from_slice(&(f.len() as u32).to_le_bytes());
                body.extend_from_slice(f.as_bytes());
            }
            None => body.push(0),
        }
    }
    body.extend_from_slice(&(cat.indexes.len() as u32).to_le_bytes());
    for i in &cat.indexes {
        body.extend_from_slice(&(i.collection.len() as u32).to_le_bytes());
        body.extend_from_slice(i.collection.as_bytes());
        body.extend_from_slice(&(i.field.len() as u32).to_le_bytes());
        body.extend_from_slice(i.field.as_bytes());
        body.extend_from_slice(&(i.kind.len() as u32).to_le_bytes());
        body.extend_from_slice(i.kind.as_bytes());
    }
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in &body {
        h ^= *b as u64;
        h = h.wrapping_mul(0x1000_0000_01b3);
    }
    body.extend_from_slice(&h.to_le_bytes());
    let tmp = dir.join("catalog.adabt.tmp");
    std::fs::write(&tmp, &body).unwrap();
    std::fs::rename(&tmp, dir.join("catalog.adabt")).unwrap();
}

#[test]
fn v3_is_accepted_with_defaults() {
    let t = Tmp::new("v3");
    let cat = catalog_v4(true, false);
    write_v3(&t.0, ID, &cat);
    let back = metadata::read(&t.0, ID).expect("v3 must be accepted");
    assert!(back.delta_encoding, "v3 default delta true");
    assert!(!back.thread_per_core, "v3 default tpc false");
    // v3 file has no flags, so whatever we wrote as true/false is lost — defaults win.
    // Prove that: write v3 with tpc=true, read must still be false (default).
    let cat2 = catalog_v4(true, true);
    write_v3(&t.0, ID, &cat2);
    let back2 = metadata::read(&t.0, ID).unwrap();
    assert!(!back2.thread_per_core);
}

#[test]
fn v4_round_trips_with_flags() {
    let t = Tmp::new("v4");
    for (d, pc) in [(true, false), (false, true), (true, true), (false, false)] {
        let cat = catalog_v4(d, pc);
        metadata::write(&t.0, ID, &cat).unwrap();
        let back = metadata::read(&t.0, ID).unwrap();
        assert_eq!(back.delta_encoding, d);
        assert_eq!(back.thread_per_core, pc);
    }
}

#[test]
fn unknown_future_version_is_refused_not_misparsed() {
    let t = Tmp::new("future");
    let cat = catalog_v4(true, false);
    metadata::write(&t.0, ID, &cat).unwrap();
    let mut bytes = std::fs::read(t.0.join("catalog.adabt")).unwrap();
    // Bump version byte past v4
    bytes[8] += 2; // MAGIC len 8, version u32 LE first byte
    std::fs::write(t.0.join("catalog.adabt"), &bytes).unwrap();
    assert!(
        metadata::read(&t.0, ID).is_none(),
        "future version must be refused, not misparsed"
    );
}
