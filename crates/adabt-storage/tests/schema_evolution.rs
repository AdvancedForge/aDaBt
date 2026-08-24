//! Schema changes that do not have to touch a row, and the ones that still do.
//!
//! `alter_schema` used to have one cost: read every row, validate it, write it
//! twice under a staging name, flip. That is still correct and still the right
//! answer for most changes — but for a narrow, mode-specific set of them, the
//! byte layout guarantees every existing record still decodes without being
//! touched at all. `codec::schema_editable_in_place` (and its own unit-test
//! module, `codec::in_place_eligibility`) is the byte-level argument for
//! exactly which changes qualify; these tests are the end-to-end evidence that
//! `HeapStore::alter_schema` actually takes the cheap path when it applies, and
//! genuinely falls back — correctly, not just cheaply — when it does not.

use adabt_core::ids::RecordId;
use adabt_core::policy::Durability;
use adabt_core::record::Record;
use adabt_core::schema::{FieldDef, FieldType, Schema, SchemaMode};
use adabt_core::store::LogicalStore;
use adabt_core::value::Value;
use adabt_storage::heap::HeapStore;
use std::path::{Path, PathBuf};

struct Tmp(PathBuf);
impl Tmp {
    fn new(tag: &str) -> Self {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "adabt-schema-evo-{tag}-{}-{:?}",
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

const N: u64 = 50;

fn two_field_schema(mode: SchemaMode) -> Schema {
    Schema::new(
        mode,
        vec![
            FieldDef::new("a", FieldType::I64).required(),
            FieldDef::new("b", FieldType::I64).required(),
        ],
    )
    .unwrap()
}

fn rec2(i: u64) -> Record {
    Record::new().with("a", i as i64).with("b", (i * 2) as i64)
}

fn open(dir: &Path) -> HeapStore {
    HeapStore::open(dir, Durability::Strict, 32).unwrap()
}

#[test]
fn fixed_mode_append_is_a_catalog_edit_not_a_rewrite() {
    // `Fixed` mode's layout is bitmap + fixed region and nothing else — no
    // offset table whose position a growing fixed region could disturb —
    // which is the one mode where a trailing append is unconditionally safe.
    let t = Tmp::new("fixed-append-cheap");
    let mut h = open(t.path());
    h.create_collection("c", two_field_schema(SchemaMode::Fixed))
        .unwrap();
    for i in 0..N {
        h.insert("c", RecordId(i), rec2(i)).unwrap();
    }
    let syncs_before = h.sync_count();

    let widened = Schema::new(
        SchemaMode::Fixed,
        vec![
            FieldDef::new("a", FieldType::I64).required(),
            FieldDef::new("b", FieldType::I64).required(),
            FieldDef::new("c", FieldType::I64),
        ],
    )
    .unwrap();
    let rewritten = h.alter_schema("c", widened.clone()).unwrap();
    assert_eq!(rewritten, 0, "a pure append should rewrite no rows");
    // One WAL entry (and its sync), not one per pre-existing row.
    assert_eq!(
        h.sync_count(),
        syncs_before + 1,
        "an in-place alter should cost exactly one fsync"
    );

    assert_eq!(h.schema_of("c").unwrap(), &widened);
    for i in 0..N {
        let got = h.get("c", RecordId(i)).unwrap().unwrap();
        assert_eq!(got.get("a"), Some(&Value::I64(i as i64)));
        assert_eq!(got.get("b"), Some(&Value::I64((i * 2) as i64)));
        assert_eq!(
            got.get("c"),
            None,
            "old row {i} should not have the new field"
        );
    }

    // A new row can populate the field the old ones could not have had.
    h.insert("c", RecordId(N), rec2(N).with("c", 99i64))
        .unwrap();
    assert_eq!(
        h.get("c", RecordId(N)).unwrap().unwrap().get("c"),
        Some(&Value::I64(99))
    );
}

#[test]
fn fixed_mode_drop_last_is_also_a_catalog_edit() {
    let t = Tmp::new("fixed-drop-cheap");
    let mut h = open(t.path());
    h.create_collection("c", two_field_schema(SchemaMode::Fixed))
        .unwrap();
    for i in 0..N {
        h.insert("c", RecordId(i), rec2(i)).unwrap();
    }

    let narrowed = Schema::new(
        SchemaMode::Fixed,
        vec![FieldDef::new("a", FieldType::I64).required()],
    )
    .unwrap();
    let rewritten = h.alter_schema("c", narrowed.clone()).unwrap();
    assert_eq!(
        rewritten, 0,
        "dropping the last field should rewrite no rows"
    );

    assert_eq!(h.schema_of("c").unwrap(), &narrowed);
    for i in 0..N {
        let got = h.get("c", RecordId(i)).unwrap().unwrap();
        assert_eq!(got.get("a"), Some(&Value::I64(i as i64)));
        assert_eq!(got.get("b"), None, "dropped field should no longer surface");
    }
    assert_eq!(h.count("c").unwrap(), N as usize);
}

#[test]
fn strict_mode_drop_last_variable_field_is_a_catalog_edit() {
    // `Strict`'s offset table sits right after the fixed region, so dropping
    // the last field is only safe when that field never occupied a slot in
    // the *fixed* region — i.e. it was variable-width. Otherwise the table's
    // own position would move (see `strict_mode_drop_last_fixed_field_falls_
    // back_to_copy_and_swap` below).
    let t = Tmp::new("strict-drop-var-cheap");
    let mut h = open(t.path());
    let with_note = Schema::new(
        SchemaMode::Strict,
        vec![
            FieldDef::new("a", FieldType::I64).required(),
            FieldDef::new("note", FieldType::Str { max_len: None }),
        ],
    )
    .unwrap();
    h.create_collection("c", with_note).unwrap();
    for i in 0..N {
        h.insert(
            "c",
            RecordId(i),
            Record::new()
                .with("a", i as i64)
                .with("note", format!("row-{i}")),
        )
        .unwrap();
    }

    let narrowed = Schema::new(
        SchemaMode::Strict,
        vec![FieldDef::new("a", FieldType::I64).required()],
    )
    .unwrap();
    let rewritten = h.alter_schema("c", narrowed.clone()).unwrap();
    assert_eq!(rewritten, 0);
    assert_eq!(h.schema_of("c").unwrap(), &narrowed);
    for i in 0..N {
        let got = h.get("c", RecordId(i)).unwrap().unwrap();
        assert_eq!(got.get("a"), Some(&Value::I64(i as i64)));
        assert_eq!(got.get("note"), None);
    }
}

#[test]
fn strict_mode_append_always_copies_even_when_nullable_and_fixed_width() {
    // The exact case an earlier version of `schema_editable_in_place` got
    // wrong: the new field is nullable, fixed-width, appended at the tail,
    // and does not cross a bitmap byte boundary — every condition the
    // `Fixed`-mode append path checks. It is still unsafe here, because
    // `Strict` has an offset table whose position depends on
    // `fixed_region_len`, and appending a fixed field grows that.
    let t = Tmp::new("strict-append-copies");
    let mut h = open(t.path());
    h.create_collection("c", two_field_schema(SchemaMode::Strict))
        .unwrap();
    for i in 0..N {
        h.insert("c", RecordId(i), rec2(i)).unwrap();
    }

    let widened = Schema::new(
        SchemaMode::Strict,
        vec![
            FieldDef::new("a", FieldType::I64).required(),
            FieldDef::new("b", FieldType::I64).required(),
            FieldDef::new("c", FieldType::I64),
        ],
    )
    .unwrap();
    let rewritten = h.alter_schema("c", widened.clone()).unwrap();
    assert_eq!(
        rewritten, N as usize,
        "Strict-mode append must copy-and-swap, however safe it looks"
    );
    assert_eq!(h.schema_of("c").unwrap(), &widened);
    for i in 0..N {
        let got = h.get("c", RecordId(i)).unwrap().unwrap();
        assert_eq!(got.get("a"), Some(&Value::I64(i as i64)));
        assert_eq!(got.get("b"), Some(&Value::I64((i * 2) as i64)));
        assert_eq!(got.get("c"), None);
    }
}

#[test]
fn strict_mode_drop_last_fixed_field_falls_back_to_copy_and_swap() {
    let t = Tmp::new("strict-drop-fixed-copies");
    let mut h = open(t.path());
    h.create_collection("c", two_field_schema(SchemaMode::Strict))
        .unwrap();
    for i in 0..N {
        h.insert("c", RecordId(i), rec2(i)).unwrap();
    }

    let narrowed = Schema::new(
        SchemaMode::Strict,
        vec![FieldDef::new("a", FieldType::I64).required()],
    )
    .unwrap();
    let rewritten = h.alter_schema("c", narrowed.clone()).unwrap();
    assert_eq!(
        rewritten, N as usize,
        "dropping a trailing *fixed* field in Strict mode must copy-and-swap"
    );
    assert_eq!(h.schema_of("c").unwrap(), &narrowed);
    for i in 0..N {
        assert_eq!(
            h.get("c", RecordId(i)).unwrap().unwrap().get("a"),
            Some(&Value::I64(i as i64))
        );
    }
}

#[test]
fn crossing_a_bitmap_byte_boundary_falls_back_to_copy_and_swap() {
    // Eight required fields: the presence bitmap is exactly one byte
    // (`bitmap_len(8) == 1`). A ninth field pushes it to two, so an old
    // record's bitmap byte is too short for the new schema to trust — even
    // in `Fixed` mode, where an append is otherwise always eligible.
    let fields: Vec<FieldDef> = (0..8)
        .map(|i| FieldDef::new(format!("f{i}"), FieldType::I64).required())
        .collect();
    let schema8 = Schema::new(SchemaMode::Fixed, fields.clone()).unwrap();

    let t = Tmp::new("boundary");
    let mut h = open(t.path());
    h.create_collection("c", schema8).unwrap();
    let mut rec = Record::new();
    for i in 0..8 {
        rec = rec.with(format!("f{i}"), i as i64);
    }
    for i in 0..N {
        h.insert("c", RecordId(i), rec.clone()).unwrap();
    }

    let mut fields9 = fields;
    fields9.push(FieldDef::new("f8", FieldType::I64));
    let schema9 = Schema::new(SchemaMode::Fixed, fields9).unwrap();
    let rewritten = h.alter_schema("c", schema9.clone()).unwrap();
    assert_eq!(
        rewritten, N as usize,
        "crossing a bitmap byte boundary must copy-and-swap every row"
    );
    assert_eq!(h.schema_of("c").unwrap(), &schema9);
    for i in 0..N {
        assert_eq!(h.get("c", RecordId(i)).unwrap(), Some(rec.clone()));
    }
}

#[test]
fn a_non_trailing_change_falls_back_to_copy_and_swap() {
    let t = Tmp::new("mid-insert");
    let mut h = open(t.path());
    h.create_collection("c", two_field_schema(SchemaMode::Fixed))
        .unwrap();
    for i in 0..N {
        h.insert("c", RecordId(i), rec2(i)).unwrap();
    }

    // Inserted between `a` and `b`, not appended after it.
    let reordered = Schema::new(
        SchemaMode::Fixed,
        vec![
            FieldDef::new("a", FieldType::I64).required(),
            FieldDef::new("m", FieldType::I64),
            FieldDef::new("b", FieldType::I64).required(),
        ],
    )
    .unwrap();
    let rewritten = h.alter_schema("c", reordered).unwrap();
    assert_eq!(rewritten, N as usize);
    for i in 0..N {
        let got = h.get("c", RecordId(i)).unwrap().unwrap();
        assert_eq!(got.get("a"), Some(&Value::I64(i as i64)));
        assert_eq!(got.get("b"), Some(&Value::I64((i * 2) as i64)));
        assert_eq!(got.get("m"), None);
    }
}

#[test]
fn declared_mode_never_takes_the_in_place_path() {
    // `Declared` allows an overflow bag; an old record's trailing bytes could
    // be one, and nothing short of reading every row can tell — so neither
    // an append nor a trailing drop is ever eligible here.
    let t = Tmp::new("declared-never");
    let mut h = open(t.path());
    h.create_collection("c", two_field_schema(SchemaMode::Declared))
        .unwrap();
    for i in 0..N {
        h.insert("c", RecordId(i), rec2(i)).unwrap();
    }

    let narrowed = Schema::new(
        SchemaMode::Declared,
        vec![FieldDef::new("a", FieldType::I64).required()],
    )
    .unwrap();
    let rewritten = h.alter_schema("c", narrowed.clone()).unwrap();
    assert_eq!(
        rewritten, N as usize,
        "Declared-mode drop must copy-and-swap, even for the last field"
    );
    assert_eq!(h.schema_of("c").unwrap(), &narrowed);
}

#[test]
fn an_in_place_alter_survives_a_restart_before_and_after_checkpoint() {
    let t = Tmp::new("restart");
    {
        let mut h = open(t.path());
        h.create_collection("c", two_field_schema(SchemaMode::Fixed))
            .unwrap();
        for i in 0..N {
            h.insert("c", RecordId(i), rec2(i)).unwrap();
        }
        let widened = Schema::new(
            SchemaMode::Fixed,
            vec![
                FieldDef::new("a", FieldType::I64).required(),
                FieldDef::new("b", FieldType::I64).required(),
                FieldDef::new("c", FieldType::I64),
            ],
        )
        .unwrap();
        h.alter_schema("c", widened).unwrap();
    }
    // Reopened without a checkpoint: the new schema comes from replaying
    // `WalOp::AlterSchemaInPlace` in pass 1, exercising the new recovery arm.
    {
        let mut h = open(t.path());
        assert_eq!(h.schema_of("c").unwrap().fields().len(), 3);
        for i in 0..N {
            let got = h.get("c", RecordId(i)).unwrap().unwrap();
            assert_eq!(got.get("a"), Some(&Value::I64(i as i64)));
            assert_eq!(got.get("c"), None);
        }
        h.checkpoint().unwrap();
    }
    // Reopened again after a checkpoint: the schema now comes from the
    // persisted catalog instead, the same generic path every collection's
    // schema already takes.
    {
        let mut h = open(t.path());
        assert_eq!(h.schema_of("c").unwrap().fields().len(), 3);
        assert_eq!(h.count("c").unwrap(), N as usize);
    }
}
