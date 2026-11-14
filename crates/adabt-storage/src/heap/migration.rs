//! Schema changes: the in-place catalog edit when the layout allows it, and
//! the copy-and-adopt migration when it does not.

use adabt_core::error::{Error, Result};
use adabt_core::ids::RecordId;
use adabt_core::record::Record;
use adabt_core::schema::Schema;
use adabt_core::store::LogicalStore;

use super::HeapStore;
use crate::catalog::encode_schema;
use crate::codec::RecordCodec;
use crate::wal::WalOp;

/// Prefix marking a collection as a migration's staging area.
///
/// A leading NUL cannot appear in a name the logical API accepts, so a staging
/// collection can never collide with a user's — and a user can never address
/// one, which is why these are filtered out of `collection_names`.
const MIGRATION_PREFIX: &str = "\u{0}migrating:";

fn migration_name(collection: &str) -> String {
    format!("{MIGRATION_PREFIX}{collection}")
}

pub(super) fn is_migration_name(name: &str) -> bool {
    name.starts_with(MIGRATION_PREFIX)
}

/// Keep only the fields `schema` still recognizes.
///
/// A no-op whenever `schema`'s mode allows extra fields — `Declared` and
/// `Dynamic` already carry an undeclared field through to the encoded
/// overflow bag, which is the existing, correct behaviour for those modes.
/// For `Strict` and `Fixed`, which forbid it, this is what makes "drop field
/// X" mean "the row no longer has X" instead of "the row fails validation."
fn project_onto_schema(schema: &Schema, rec: &Record) -> Record {
    if schema.mode().allows_extra_fields() {
        return rec.clone();
    }
    let mut out = Record::new();
    for (name, v) in rec.iter() {
        if schema.field(name).is_some() {
            out.set(name.to_string(), v.clone());
        }
    }
    out
}

impl HeapStore {
    /// Replace a collection's schema, in whichever of two ways applies.
    ///
    /// Every record is validated against the new schema *before* anything is
    /// written. A partial freeze would leave a collection whose stored records
    /// its own schema rejects, which no later operation could repair.
    ///
    /// # Two paths, chosen by `codec::schema_editable_in_place`
    ///
    /// Most schema changes cannot avoid touching every row: a field moved,
    /// retyped, or inserted anywhere but the end changes the byte offset of
    /// data records already have on disk, and there is no way to make an old
    /// record answer to a new layout without rewriting it. For that case, see
    /// "why this is done out of place" below.
    ///
    /// But appending one nullable, fixed-width field, or dropping the last
    /// field, changes no existing byte's meaning at all — see
    /// `codec::schema_editable_in_place` for exactly why those two are safe
    /// and nothing wider is. When it applies, this logs one
    /// `WalOp::AlterSchemaInPlace` entry — a catalog edit, not a data
    /// operation — and returns `Ok(0)`: zero rows rewritten is the honest
    /// answer, and a caller checking the return value learns the change was
    /// free without needing a second method to ask.
    ///
    /// # Why the other path is done out of place
    ///
    /// The obvious implementation — log the new schema, then rewrite each
    /// record — is silently unsafe, and the failure is not a lost record but a
    /// wrong one. Recovery must apply a schema change before replaying the
    /// writes that assume it, so after a crash part-way through the rewrite the
    /// new codec meets the bytes the old codec left in the pages. A tag-length
    /// layout read as a fixed one decodes to plausible nonsense and raises no
    /// error: `id` comes back as 7305804385234280967 and nothing complains.
    ///
    /// So the new encoding is built beside the old one under a private name and
    /// adopted in a single log entry. Truncate the log anywhere before that
    /// entry and the original collection is untouched; anywhere after it and the
    /// migration is complete. There is no in-between state to recover from.
    ///
    /// The cost is honest: the collection is stored twice until the flip.
    pub fn alter_schema(&mut self, collection: &str, schema: Schema) -> Result<usize> {
        if is_migration_name(collection) {
            return Err(Error::InvalidOptimization(
                "cannot alter the schema of a migration collection".into(),
            ));
        }
        let old_schema = self.coll(collection)?.codec.schema().clone();
        if crate::codec::schema_editable_in_place(&old_schema, &schema) {
            self.log(WalOp::AlterSchemaInPlace {
                collection: collection.to_string(),
                schema: encode_schema(&schema),
            })?;
            self.apply_alter_schema_in_place(collection, schema)?;
            return Ok(0);
        }
        let rows = self.scan(collection)?;
        // A field the new schema no longer declares is what "drop this field"
        // *means* — dropping is exactly as much a schema change as adding one,
        // so a row carrying it is not a row that fails to fit the new schema,
        // it is a row that needs projecting onto it first. Validating the raw
        // old record instead, as an earlier version of this method did, meant
        // `alter_schema` could add a field but could never drop one outside
        // `Declared`/`Dynamic`'s overflow bag: any `Strict` or `Fixed` row
        // still carrying the dropped field would fail
        // `Schema::validate_record` with `UnknownField` before a single byte
        // was rewritten.
        let projected: Vec<(RecordId, Record)> = rows
            .iter()
            .map(|(id, rec)| (*id, project_onto_schema(&schema, rec)))
            .collect();
        for (id, rec) in &projected {
            schema.validate_record(rec).map_err(|e| {
                Error::InvalidOptimization(format!(
                    "record {id} does not fit the proposed schema: {e}"
                ))
            })?;
        }

        // A stale staging area from an earlier crash is dead weight, not state.
        let staging = migration_name(collection);
        if self.collections.contains_key(&staging) {
            self.drop_collection(&staging)?;
        }

        self.log(WalOp::CreateCollection {
            name: staging.clone(),
            schema: encode_schema(&schema),
        })?;
        self.register_collection(&staging, schema);

        for (id, rec) in &projected {
            let bytes = self.coll(&staging)?.codec.encode(rec)?;
            self.log(WalOp::Insert {
                collection: staging.clone(),
                id: *id,
                bytes: bytes.clone(),
            })?;
            self.apply_put(&staging, *id, &bytes)?;
        }

        // The flip. Everything above this line is reversible by ignoring it.
        self.log(WalOp::AdoptMigration {
            target: collection.to_string(),
            source: staging.clone(),
        })?;
        self.adopt(collection, &staging)?;
        Ok(rows.len())
    }

    /// Rebuild `collection`'s codec from `schema` and nothing else — no page,
    /// no directory entry, no id is touched. Called both from `alter_schema`
    /// directly and from recovery, so it does not itself decide eligibility;
    /// by the time it runs, `codec::schema_editable_in_place` already has.
    pub(super) fn apply_alter_schema_in_place(
        &mut self,
        collection: &str,
        schema: Schema,
    ) -> Result<()> {
        let c = self
            .collections
            .get_mut(collection)
            .ok_or_else(|| Error::NoSuchCollection(collection.to_string()))?;
        c.codec = RecordCodec::new(schema);
        Ok(())
    }

    /// Hand `source`'s identity and records to `target`, discarding the old.
    ///
    /// Idempotent: replaying an adoption whose source is already gone is a
    /// no-op, which is what lets recovery apply it without tracking whether it
    /// already had.
    pub(super) fn adopt(&mut self, target: &str, source: &str) -> Result<()> {
        let Some(mut staged) = self.collections.remove(source) else {
            return Ok(());
        };
        self.by_id.remove(&staged.id);
        // The staged records carry the staging collection's id in their slot
        // prefix, so the id travels with them rather than the name.
        if let Some(old) = self.collections.remove(target) {
            self.by_id.remove(&old.id);
            self.free_pages_of(&old)?;
            // The adopted collection answers for the old one's history too: a
            // write to the old collection after some transaction's snapshot
            // (say, a delete that left it empty, so the staging copy has no
            // writes of its own) must still conflict with that transaction's
            // scan once the name points at the new one.
            staged.newest_write = staged.newest_write.max(old.newest_write);
        }
        self.by_id.insert(staged.id, target.to_string());
        self.collections.insert(target.to_string(), staged);
        Ok(())
    }
}
