//! Unique constraints: declaring, dropping, listing and enforcing them.

use adabt_core::error::{Error, Result};
use adabt_core::ids::RecordId;
use adabt_core::index_kind::IndexKind;
use adabt_core::record::Record;
use adabt_core::store::LogicalStore;
use adabt_core::value::Value;
use std::collections::{HashMap, HashSet};

use super::Database;

impl Database {
    // -- unique constraints -------------------------------------------------

    /// Declare that no two records in `collection` may share a value for
    /// `field`, and refuse if any already do.
    ///
    /// Nulls are exempt, matching the usual convention: an absent value is not
    /// a duplicate of another absent value. A backing hash index is created if
    /// one does not already exist — enforcement needs to find a conflict by
    /// value, not by scanning — and it is pinned, so the adaptive driver may
    /// never retract it: dropping the index would not slow the database down,
    /// it would make the constraint stop being enforced, which is a category of
    /// mistake no amount of cost-benefit scoring should be able to make.
    pub fn add_unique_constraint(&mut self, collection: &str, field: &str) -> Result<()> {
        if self.store.schema_of(collection).is_err() {
            return Err(Error::NoSuchCollection(collection.to_string()));
        }
        if self.unique_constraints.contains(collection, field) {
            return Ok(());
        }
        // Existing data first: a constraint that let its own violation stand
        // would be a promise broken at the moment it was made.
        let mut seen: HashMap<Value, RecordId> = HashMap::new();
        for (id, rec) in self.store.scan(collection)? {
            let Some(value) = rec.get(field).filter(|v| !v.is_null()).cloned() else {
                continue;
            };
            if let Some(&other) = seen.get(&value) {
                return Err(Error::UniqueViolation {
                    collection: collection.to_string(),
                    field: field.to_string(),
                    value: format!("{value:?} (already held by record {other}, and by {id})"),
                });
            }
            seen.insert(value, id);
        }
        if !self
            .derived
            .index_exists(collection, field, IndexKind::Hash)
        {
            self.create_index(collection, field, IndexKind::Hash)?;
        }
        self.unique_constraints.add(collection, field);
        crate::unique::write(self.store.dir(), &self.unique_constraints)?;
        Ok(())
    }

    /// Stop enforcing a constraint. The backing index is left in place — whether
    /// it is still worth keeping for ordinary querying is the optimizer's
    /// decision, not this one's.
    pub fn drop_unique_constraint(&mut self, collection: &str, field: &str) -> Result<bool> {
        let removed = self.unique_constraints.remove(collection, field);
        if removed {
            crate::unique::write(self.store.dir(), &self.unique_constraints)?;
        }
        Ok(removed)
    }

    pub fn has_unique_constraint(&self, collection: &str, field: &str) -> bool {
        self.unique_constraints.contains(collection, field)
    }

    pub fn unique_constraints(&self) -> Vec<(String, String)> {
        self.unique_constraints
            .iter()
            .map(|(c, f)| (c.to_string(), f.to_string()))
            .collect()
    }

    /// Refuse `rec` if writing it to `collection` would violate a unique
    /// constraint. `exclude` is the id being replaced, if any — a record is
    /// always allowed to keep, or overwrite, its own value.
    pub(super) fn check_unique_constraints(
        &self,
        collection: &str,
        exclude: Option<RecordId>,
        rec: &Record,
    ) -> Result<()> {
        if self.unique_constraints.is_empty() {
            return Ok(());
        }
        for field in self.unique_constraints.on(collection) {
            let Some(value) = rec.get(field).filter(|v| !v.is_null()) else {
                continue;
            };
            let conflict = self
                .holders_of(collection, field, value)
                .into_iter()
                .any(|id| Some(id) != exclude);
            if conflict {
                return Err(Error::UniqueViolation {
                    collection: collection.to_string(),
                    field: field.to_string(),
                    value: format!("{value:?}"),
                });
            }
        }
        Ok(())
    }

    /// Refuse a transaction's write-set if, once every write in it has been
    /// applied, two records would share a value under a unique constraint.
    ///
    /// Judged against the state the commit produces, not one write at a time
    /// against the state before it: a value the transaction moves from one
    /// record to another is not a conflict, and two writes in the same
    /// transaction claiming one value are, whichever order they would be
    /// applied in. Checked before anything is written, because once the store
    /// has logged the write-set there is no applying "some" of it.
    ///
    /// `writes` are `(collection, id, new record)`, `None` for a delete; puts
    /// are already normalised.
    pub(super) fn check_unique_constraints_for_write_set(
        &self,
        writes: &[(&str, RecordId, Option<&Record>)],
    ) -> Result<()> {
        if self.unique_constraints.is_empty() {
            return Ok(());
        }
        // Every record the transaction writes gets a new value (or none), so
        // whatever it held before can no longer collide with anything.
        let rewritten: HashSet<(&str, RecordId)> =
            writes.iter().map(|&(c, id, _)| (c, id)).collect();
        let mut claimed: HashMap<(&str, &str, &Value), RecordId> = HashMap::new();
        for &(collection, id, rec) in writes {
            let Some(rec) = rec else { continue };
            for field in self.unique_constraints.on(collection) {
                let Some(value) = rec.get(field).filter(|v| !v.is_null()) else {
                    continue;
                };
                if let Some(other) = claimed.insert((collection, field, value), id) {
                    return Err(Error::UniqueViolation {
                        collection: collection.to_string(),
                        field: field.to_string(),
                        value: format!(
                            "{value:?} (written to both {other} and {id} by one transaction)"
                        ),
                    });
                }
                let kept_elsewhere = self
                    .holders_of(collection, field, value)
                    .into_iter()
                    .any(|holder| holder != id && !rewritten.contains(&(collection, holder)));
                if kept_elsewhere {
                    return Err(Error::UniqueViolation {
                        collection: collection.to_string(),
                        field: field.to_string(),
                        value: format!("{value:?}"),
                    });
                }
            }
        }
        Ok(())
    }

    /// The records currently holding `value` in `field`, found through the
    /// hash index every unique constraint is backed by.
    fn holders_of(&self, collection: &str, field: &str, value: &Value) -> Vec<RecordId> {
        self.derived
            .indexes
            .get(collection)
            .and_then(|list| list.iter().find(|i| i.field() == field))
            .map(|idx| idx.lookup(value))
            .unwrap_or_default()
    }
}
