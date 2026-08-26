//! The single-field read path, end to end.
//!
//! `RecordCodec::peek_field` decodes one field of an encoded record and
//! touches nothing else; the trait now carries that capability up through
//! the heap so `Database::peek_field`'s fallback stops materialising whole
//! records to answer one question. These tests hold the override to the
//! same contract as a full fetch-and-discard: same value for a present
//! field, same absence for a missing one, same "row gone" for a deleted id
//! — on both schema modes.

use adabt_core::ids::RecordId;
use adabt_core::policy::Policy;
use adabt_core::record::Record;
use adabt_core::schema::{FieldDef, FieldType, Schema, SchemaMode};
use adabt_core::store::LogicalStore;
use adabt_engine::Database;
use std::path::{Path, PathBuf};

struct Tmp(PathBuf);
impl Tmp {
    fn new(tag: &str) -> Self {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "adabt-peek-{tag}-{}-{:?}",
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

/// Level 2 keeps the direct-path machinery out of the way: these reads go
/// through the heap fallback, which is the code under test.
fn db_with(mode: SchemaMode, tag: &str) -> (Tmp, Database) {
    let t = Tmp::new(tag);
    let mut db = Database::open(t.path(), Policy::manual(2)).unwrap();
    let schema = Schema::new(
        mode,
        vec![
            FieldDef::new("name", FieldType::Char(16)),
            FieldDef::new("bio", FieldType::Char(48)),
            FieldDef::new("balance", FieldType::I64),
        ],
    )
    .unwrap();
    db.create_collection("users", schema).unwrap();
    db.insert(
        "users",
        RecordId(1),
        Record::new()
            .with("name", "ada")
            .with("bio", "analytical engine, first of her name")
            .with("balance", 100i64),
    )
    .unwrap();
    // A row that genuinely lacks one declared-ish field (dynamic mode).
    db.insert("users", RecordId(2), Record::new().with("name", "grace"))
        .unwrap();
    (t, db)
}

#[test]
fn peek_matches_fetch_on_both_schema_modes() {
    for (mode, tag) in [(SchemaMode::Dynamic, "dyn"), (SchemaMode::Fixed, "fix")] {
        let (_t, mut db) = db_with(mode, tag);

        // Present field, on a row whose other fields are wide enough that a
        // full decode would be real work.
        assert_eq!(
            db.peek_field("users", RecordId(1), "bio").unwrap(),
            Some(Some(adabt_core::value::Value::Str(
                "analytical engine, first of her name".into()
            ))),
            "{tag:?}: present field"
        );

        // A row that lives but never had this field.
        assert_eq!(
            db.peek_field("users", RecordId(2), "balance").unwrap(),
            Some(None),
            "{tag:?}: absent field on a live row"
        );
    }
}

#[test]
fn a_deleted_row_is_gone_not_absent() {
    let (_t, mut db) = db_with(SchemaMode::Dynamic, "dead");
    db.delete("users", RecordId(2)).unwrap();
    assert_eq!(db.peek_field("users", RecordId(2), "name").unwrap(), None);
    // The survivor still peeks fine after the deletion reshaped pages.
    assert_eq!(
        db.peek_field("users", RecordId(1), "balance").unwrap(),
        Some(Some(adabt_core::value::Value::I64(100)))
    );
}
