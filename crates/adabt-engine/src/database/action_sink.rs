//! `ActionSink` for `Database`: how the optimization controller's actions
//! are checked and applied.

use adabt_core::error::Result;
use adabt_core::store::LogicalStore;
use adabt_opt::action::{Action, ActionSink};

use super::Database;

impl ActionSink for Database {
    fn can_apply(&mut self, action: &Action) -> bool {
        match action {
            Action::CreateIndex { collection, .. } | Action::DropIndex { collection, .. } => {
                self.store.schema_of(collection).is_ok()
            }
            // Only worth building where a schema guarantees a constant stride.
            Action::SetDirectLookup(true) => !self.fixed_size_collections().is_empty(),
            _ => true,
        }
    }

    fn apply_action(&mut self, action: &Action) -> Result<()> {
        // Recorded as it happens rather than predicted from the plan, so what is
        // masked is exactly what was built. A prediction that drifted from what
        // the engine actually did would mask the wrong structure — and masking
        // the wrong structure means the "baseline" measurement is quietly taken
        // through the candidate.
        self.experiments.record_candidate_action(action);
        match action {
            Action::CreateIndex {
                collection,
                field,
                kind,
            } => self.create_index(collection, field, *kind),
            Action::DropIndex {
                collection,
                field,
                kind,
            } => {
                self.drop_index(collection, field, *kind);
                Ok(())
            }
            Action::SetBufferPoolPages(n) => self.store.set_pool_capacity(*n),
            Action::SetPlanCacheEntries(n) => {
                self.caches.plans.set_capacity(*n);
                Ok(())
            }
            Action::SetResultCacheEntries(n) => {
                self.caches.results.set_capacity(*n);
                Ok(())
            }
            Action::SetDirectLookup(on) => {
                if *on {
                    self.enable_direct_lookup()
                } else {
                    self.disable_direct_lookup();
                    Ok(())
                }
            }
            Action::FreezeSchema { collection } => self.freeze_schema(collection).map(|_| ()),
            Action::SetClusterField { collection, field } => {
                self.declare_cluster_field(collection, field)
            }
            Action::ClearClusterField { collection } => self.clear_cluster_field(collection),
            Action::SetDeltaEncoding(on) => {
                crate::column::set_delta_enabled(*on);
                self.tuning.delta_encoding = *on;
                self.store.set_delta_encoding(*on);
                for store in self.derived.columns.values_mut() {
                    store.set_delta_enabled(*on);
                }
                Ok(())
            }
            Action::SetThreadPerCore(on) => {
                self.tuning.thread_per_core = *on;
                self.store.set_thread_per_core(*on);
                Ok(())
            }
            Action::SetColumnStore(on) => {
                if *on {
                    self.enable_column_store()
                } else {
                    self.disable_column_store();
                    Ok(())
                }
            }
            Action::SetMaterializedViews(on) => {
                // Turning it on builds nothing. Views appear the first time a
                // query asks for one, which is the only moment the engine knows
                // which aggregates are worth keeping — guessing up front would
                // maintain totals nobody reads on every write.
                self.derived.views.set_enabled(*on);
                Ok(())
            }
            Action::SetRecordCompression(on) => {
                self.store.set_compression(*on);
                // Re-encoding what is already stored is the expensive part, and
                // the reason this optimization has a real build cost.
                self.store.recompress_all()?;
                Ok(())
            }
            Action::SetPrefetch(on) => {
                self.store.set_prefetch(*on);
                Ok(())
            }
            Action::SetJoinOrder(on) => {
                self.tuning.join_order = *on;
                Ok(())
            }
            Action::SetDataPartitioning(on) => {
                self.tuning.data_partitioning = *on;
                Ok(())
            }
        }
    }
}
