//! Schema evolution without a rewrite: which schema changes leave every
//! record already on disk decodable, byte for byte, under the new schema.

use adabt_core::schema::{Schema, SchemaMode};

use super::bitmap_len;

/// Whether every record already on disk under `old` still decodes correctly
/// under `new`, with no byte touched — the condition that lets a schema change
/// become a catalog edit instead of the copy-and-swap in
/// `HeapStore::alter_schema`.
///
/// This is not one rule; it is a different rule per mode, because the layout
/// table in the `codec` module docs is a different rule per mode. What follows
/// is the layout argument for each; `mod in_place_eligibility` below holds it
/// to the same standard every other bit-level claim in this module is held to —
/// a written-out record, decoded and checked, not just reasoned about. An
/// earlier version of this function allowed fixed-width appends for `Strict`
/// and `Declared` on exactly this reasoning before that suite caught it:
/// wrong, in a way pure reasoning about "the field is presence-gated" made
/// look safe.
///
/// - **`Fixed`.** The layout is bitmap, then fixed region, then nothing —
///   `RecordCodec::decode_with_header` returns as soon as the fixed fields are
///   read, so there is no offset table whose position depends on
///   `fixed_region_len`. That is what makes *both* directions safe here and
///   nowhere else: appending one nullable field at the tail only adds a
///   presence-gated read past the old data (old records have that bit unset,
///   so it is skipped, unless the new field count crosses into an additional
///   bitmap byte — `bitmap_len` growing means the bitmap itself is now longer
///   than an old record's, which is checked and turned into a loud
///   `Error::Corruption` rather than read past); dropping the last field
///   shrinks the region old records simply have unread trailing bytes beyond.
///   `nullable` on the added field is required for a different reason than
///   safety: decode does not consult `Schema::validate_record` at all, so
///   without it an old record would silently violate a `required` constraint
///   the schema is supposed to guarantee on every row.
/// - **`Strict`.** Fixed fields sit before the offset table, so *any* change
///   to the fixed field set — adding one or dropping one — moves
///   `fixed_region_len` and therefore `table_at`, the position decode looks
///   for that table at. An old record's table is not there any more; this is
///   exactly the bug the `Fixed`-mode reasoning above does not have, and it
///   is why `Strict` cannot take the append path at all, in either field
///   width. Dropping the last field is safe only when that field was
///   *variable*-width: the fixed region — and so `table_at` — is untouched,
///   and the new schema simply reads one fewer table entry than the old
///   record's table has, which is a safe prefix of real bytes. (Appending a
///   variable-width field is the mirror case — `table_at` likewise untouched
///   — but reading one *extra* entry means reading past the old table into
///   whatever data follows, which is not reliably a valid offset. It happens
///   to look safe by the same presence-gating argument that was wrong above,
///   so it is excluded too, not allowed on the strength of that argument.)
/// - **`Declared`.** Never takes either path. Appending fails for the same
///   reason as `Strict`. Dropping the last field would otherwise work the
///   same way `Strict`'s does, except a `Declared` record may carry a real
///   overflow bag (`flags::HAS_OVERFLOW`) immediately after its last declared
///   field; the new schema's table is then one entry short of the old one,
///   and it reads the dropped field's own bytes as the overflow section's
///   leading length-prefixed count — silently wrong, not a decode error.
///   Telling the two cases apart means reading every row, which is the cost
///   this function exists to avoid.
/// - **`Dynamic`.** Has no declared fields to add or drop in the first place.
pub(crate) fn schema_editable_in_place(old: &Schema, new: &Schema) -> bool {
    if old.mode() != new.mode() {
        return false;
    }
    let (of, nf) = (old.fields(), new.fields());
    match old.mode() {
        SchemaMode::Dynamic | SchemaMode::Declared => false,
        SchemaMode::Fixed => {
            if nf.len() == of.len() + 1 && nf[..of.len()] == *of {
                let added = &nf[of.len()];
                added.nullable && bitmap_len(nf.len()) == bitmap_len(of.len())
            } else {
                of.len() == nf.len() + 1 && of[..nf.len()] == *nf
            }
        }
        SchemaMode::Strict => {
            of.len() == nf.len() + 1
                && of[..nf.len()] == *nf
                && of[nf.len()].ty.fixed_width().is_none()
        }
    }
}

/// `schema_editable_in_place` makes a claim about bytes, not about types, so
/// this checks it the same way: encode under one schema, decode under
/// another, on every path the function allows and several it must not.
///
/// This suite exists because the function's first version was wrong — it
/// allowed a fixed-width append for `Strict`/`Declared` on an argument that
/// sounded right (the new field is presence-gated) and missed that the
/// *offset table's own position* moves with `fixed_region_len` regardless.
/// Every branch below is the direct antidote: write real old-format bytes,
/// decode them with a real new-format codec, and check the record, not the
/// reasoning.
#[cfg(test)]
mod in_place_eligibility {
    use super::*;
    use crate::codec::RecordCodec;
    use adabt_core::record::Record;
    use adabt_core::schema::{FieldDef, FieldType};

    fn f(name: &str, ty: FieldType) -> FieldDef {
        FieldDef::new(name, ty)
    }
    fn req(name: &str, ty: FieldType) -> FieldDef {
        FieldDef::new(name, ty).required()
    }

    fn schema(mode: SchemaMode, fields: Vec<FieldDef>) -> Schema {
        Schema::new(mode, fields).unwrap()
    }

    /// Encode `rec` under `old`, decode it under `new`, and assert the two
    /// codecs agree on every field `new` still declares — the empirical
    /// half of what `schema_editable_in_place` promises.
    fn assert_reads_back(old: &Schema, new: &Schema, rec: &Record) {
        assert!(
            schema_editable_in_place(old, new),
            "test setup expected this pair to be eligible"
        );
        let old_codec = RecordCodec::new(old.clone());
        let new_codec = RecordCodec::new(new.clone());
        let bytes = old_codec.encode(rec).unwrap();
        let got = new_codec
            .decode(&bytes)
            .unwrap_or_else(|e| panic!("old bytes did not decode under the new schema: {e}"));
        for field in new.fields() {
            assert_eq!(
                got.get(&field.name),
                rec.get(&field.name),
                "field {} mismatched after decoding old bytes under the new schema",
                field.name
            );
        }
    }

    #[test]
    fn fixed_mode_append_reads_old_rows_with_the_new_field_absent() {
        let old = schema(
            SchemaMode::Fixed,
            vec![req("a", FieldType::I64), req("b", FieldType::I64)],
        );
        let new = schema(
            SchemaMode::Fixed,
            vec![
                req("a", FieldType::I64),
                req("b", FieldType::I64),
                f("c", FieldType::I64),
            ],
        );
        let rec = Record::new().with("a", 1i64).with("b", 2i64);
        assert_reads_back(&old, &new, &rec);
        assert_eq!(
            RecordCodec::new(new.clone())
                .decode(&RecordCodec::new(old).encode(&rec).unwrap())
                .unwrap()
                .get("c"),
            None
        );
    }

    #[test]
    fn fixed_mode_drop_last_reads_old_rows_with_the_field_ignored() {
        let old = schema(
            SchemaMode::Fixed,
            vec![
                req("a", FieldType::I64),
                req("b", FieldType::I64),
                req("c", FieldType::I64),
            ],
        );
        let new = schema(
            SchemaMode::Fixed,
            vec![req("a", FieldType::I64), req("b", FieldType::I64)],
        );
        let rec = Record::new()
            .with("a", 1i64)
            .with("b", 2i64)
            .with("c", 3i64);
        assert_reads_back(&old, &new, &rec);
    }

    #[test]
    fn strict_mode_drop_last_variable_field_reads_old_rows() {
        let old = schema(
            SchemaMode::Strict,
            vec![
                req("a", FieldType::I64),
                f("note", FieldType::Str { max_len: None }),
            ],
        );
        let new = schema(SchemaMode::Strict, vec![req("a", FieldType::I64)]);
        let rec = Record::new().with("a", 1i64).with("note", "hello");
        assert_reads_back(&old, &new, &rec);
    }

    #[test]
    fn strict_mode_never_allows_a_fixed_width_append() {
        let old = schema(SchemaMode::Strict, vec![req("a", FieldType::I64)]);
        let new = schema(
            SchemaMode::Strict,
            vec![req("a", FieldType::I64), f("b", FieldType::I64)],
        );
        assert!(!schema_editable_in_place(&old, &new));
    }

    #[test]
    fn strict_mode_never_allows_a_variable_width_append() {
        let old = schema(SchemaMode::Strict, vec![req("a", FieldType::I64)]);
        let new = schema(
            SchemaMode::Strict,
            vec![
                req("a", FieldType::I64),
                f("b", FieldType::Str { max_len: None }),
            ],
        );
        assert!(!schema_editable_in_place(&old, &new));
    }

    #[test]
    fn strict_mode_never_allows_dropping_a_trailing_fixed_field() {
        // The mirror bug of the append case: dropping the last field also
        // moves `table_at` when that field is fixed-width, because
        // `fixed_region_len` shrinks by exactly its width.
        let old = schema(
            SchemaMode::Strict,
            vec![req("a", FieldType::I64), f("b", FieldType::I64)],
        );
        let new = schema(SchemaMode::Strict, vec![req("a", FieldType::I64)]);
        assert!(!schema_editable_in_place(&old, &new));
    }

    #[test]
    fn declared_mode_never_takes_the_in_place_path() {
        let old = schema(
            SchemaMode::Declared,
            vec![
                req("a", FieldType::I64),
                f("note", FieldType::Str { max_len: None }),
            ],
        );
        let append = schema(
            SchemaMode::Declared,
            vec![
                req("a", FieldType::I64),
                f("note", FieldType::Str { max_len: None }),
                f("b", FieldType::I64),
            ],
        );
        let drop = schema(SchemaMode::Declared, vec![req("a", FieldType::I64)]);
        assert!(!schema_editable_in_place(&old, &append));
        assert!(!schema_editable_in_place(&old, &drop));
    }

    #[test]
    fn dynamic_mode_never_takes_the_in_place_path() {
        assert!(!schema_editable_in_place(
            &Schema::dynamic(),
            &Schema::dynamic()
        ));
    }

    #[test]
    fn crossing_a_bitmap_byte_boundary_is_rejected_even_though_it_is_an_append() {
        let fields: Vec<FieldDef> = (0..8)
            .map(|i| req(&format!("f{i}"), FieldType::I64))
            .collect();
        let old = schema(SchemaMode::Fixed, fields.clone());
        let mut nine = fields;
        nine.push(f("f8", FieldType::I64));
        let new = schema(SchemaMode::Fixed, nine);
        assert!(!schema_editable_in_place(&old, &new));
    }

    #[test]
    fn appending_a_required_field_is_rejected_regardless_of_layout_safety() {
        let old = schema(SchemaMode::Fixed, vec![req("a", FieldType::I64)]);
        let new = schema(
            SchemaMode::Fixed,
            vec![req("a", FieldType::I64), req("b", FieldType::I64)],
        );
        assert!(!schema_editable_in_place(&old, &new));
    }
}
