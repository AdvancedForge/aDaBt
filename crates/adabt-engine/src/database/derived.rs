//! The derived structures the planner can choose between, and how each is kept
//! in step with the primary as records change.

use adabt_core::ids::RecordId;
use adabt_core::index_kind::IndexKind;
use adabt_core::record::Record;
use adabt_index::Index;
use adabt_telemetry::event::Event;
use adabt_telemetry::{CollectingProbe, Probe};
use std::collections::HashMap;

use crate::column::ColumnStore;
use crate::direct::DirectArray;
use crate::matview::MaterializedViews;

/// Every representation rebuildable from the primary that a query can be
/// served from: indexes, direct arrays, columnar copies and materialized views.
pub(super) struct DerivedStructures {
    pub(super) indexes: HashMap<String, Vec<Box<dyn Index>>>,
    /// Directly-addressed arrays, where the schema and density allow.
    pub(super) direct: HashMap<String, DirectArray>,
    /// Columnar copies, for scans and aggregates over a bounded field set.
    pub(super) columns: HashMap<String, ColumnStore>,
    /// Grouped counts kept up to date on write, so an aggregate costs the number
    /// of groups rather than the number of rows.
    pub(super) views: MaterializedViews,
}

impl DerivedStructures {
    pub(super) fn new() -> Self {
        Self {
            indexes: HashMap::new(),
            direct: HashMap::new(),
            columns: HashMap::new(),
            views: MaterializedViews::new(),
        }
    }

    pub(super) fn index_exists(&self, collection: &str, field: &str, kind: IndexKind) -> bool {
        self.indexes
            .get(collection)
            .is_some_and(|l| l.iter().any(|i| i.field() == field && i.kind() == kind))
    }

    pub(super) fn index_memory_bytes(&self) -> usize {
        self.indexes
            .values()
            .flat_map(|l| l.iter())
            .map(|i| i.memory_bytes())
            .sum()
    }

    /// Whether a write has to read what it is about to overwrite.
    ///
    /// The standing cost of maintaining anything derived from a record's
    /// *contents* rather than its identity: an index has to be told which key to
    /// remove, a view which group to decrement, and neither can be worked out
    /// from the new record. It is why a structure nobody queries is a loss
    /// rather than merely neutral.
    pub(super) fn needs_old_record(&self, collection: &str) -> bool {
        self.indexes.get(collection).is_some_and(|l| !l.is_empty())
            || self.views.watches(collection)
    }

    /// Bring every structure over `collection` up to date with a record that
    /// now holds `rec`, reporting each index's upkeep to `probe`.
    pub(super) fn on_insert(
        &mut self,
        collection: &str,
        id: RecordId,
        rec: &Record,
        probe: &CollectingProbe,
    ) {
        if let Some(list) = self.indexes.get_mut(collection) {
            for i in list {
                i.index_record(id, rec);
                // The price of this index, counted where it is actually paid.
                // See `Event::IndexMaintained`: without this the optimizer can
                // only see what an index is worth, never what it costs.
                probe.record(Event::IndexMaintained {
                    collection,
                    field: i.field(),
                });
            }
        }
        if let Some(d) = self.direct.get_mut(collection) {
            // A record that will not fit the stride means the direct array no
            // longer matches the schema; dropping it is safe because it is
            // derived, and correct because a partial array would lie.
            if d.put(id, rec).is_err() {
                self.direct.remove(collection);
            }
        }
        // A columnar copy is append-structured, so an update in place is not
        // expressible: the old row is tombstoned and the new one appended.
        if let Some(c) = self.columns.get_mut(collection) {
            c.mark_dead(id);
            c.append_row(id, rec);
        }
        self.views.on_insert(collection, rec);
    }

    /// Take a record that held `rec` out of every structure over `collection`,
    /// reporting each index's upkeep to `probe`.
    pub(super) fn on_remove(
        &mut self,
        collection: &str,
        id: RecordId,
        rec: &Record,
        probe: &CollectingProbe,
    ) {
        if let Some(list) = self.indexes.get_mut(collection) {
            for i in list {
                i.unindex_record(id, rec);
                probe.record(Event::IndexMaintained {
                    collection,
                    field: i.field(),
                });
            }
        }
        if let Some(d) = self.direct.get_mut(collection) {
            d.remove(id);
        }
        if let Some(c) = self.columns.get_mut(collection) {
            c.mark_dead(id);
        }
        self.views.on_remove(collection, rec);
    }
}
