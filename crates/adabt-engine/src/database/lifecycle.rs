//! Opening a database and the plain accessors for its policy, telemetry and
//! enabled features.

use adabt_core::error::Result;
use adabt_core::index_kind::IndexKind;
use adabt_core::policy::{Durability, Mode, Policy};
use adabt_exec::exec::ExecStats;
use adabt_opt::{OptimizationController, Registry};
use adabt_storage::heap::HeapStore;
use adabt_telemetry::{CollectingProbe, Snapshot};
#[cfg(feature = "loom")]
use loom::sync::Arc;
use std::collections::HashMap;
use std::path::Path;
#[cfg(not(feature = "loom"))]
use std::sync::Arc;

use crate::optimizations::register_builtins;

use super::derived::DerivedStructures;
use super::durability::{decode_index_entries, index_cache_key};
use super::experiment_state::Experiments;
use super::query_caches::QueryCaches;
use super::telemetry::Telemetry;
use super::tuning::Tuning;
use super::{Database, Execution, Optimizer};

impl Database {
    pub fn open(dir: &Path, policy: Policy) -> Result<Self> {
        Self::open_shared(
            dir,
            policy,
            Arc::new(adabt_storage::version::VersionTracker::new()),
        )
    }

    /// Open with a version tracker shared across other databases — what every
    /// shard of a [`crate::sharded::ShardedDatabase`] does, so their timestamps
    /// order against each other rather than each starting its own clock at one.
    pub fn open_shared(
        dir: &Path,
        policy: Policy,
        versions: Arc<adabt_storage::version::VersionTracker>,
    ) -> Result<Self> {
        let store = HeapStore::open_shared(dir, policy.guarantees.durability, 1024, versions)?;
        Self::open_with_store(store, policy)
    }

    /// Open a database, replaying the log only up to `target` — a
    /// point-in-time restore. See [`adabt_storage::heap::RecoverTarget`].
    ///
    /// Everything past the store itself — indexes, materialized views, the
    /// unique-constraint sidecar — is loaded exactly as `open_shared` loads
    /// it, because there is nothing target-specific left to decide once the
    /// store has already stopped where it was told to: an index cache stamped
    /// against a state that no longer matches (because the store now holds
    /// less than it did) simply fails its own staleness check and is rebuilt
    /// by scanning, the same as it would after any other kind of divergence.
    pub fn open_at(
        dir: &Path,
        policy: Policy,
        target: adabt_storage::heap::RecoverTarget,
    ) -> Result<Self> {
        let store = HeapStore::open_shared_at(
            dir,
            policy.guarantees.durability,
            1024,
            Arc::new(adabt_storage::version::VersionTracker::new()),
            target,
        )?;
        Self::open_with_store(store, policy)
    }

    fn open_with_store(store: HeapStore, policy: Policy) -> Result<Self> {
        let mut registry = Registry::new();
        register_builtins(&mut registry);
        // Caught here, once, rather than left to `ManualDriver::decide`'s own
        // silent skip: that skip is correct for a name that used to exist and
        // has since been dropped from a build, which a running database must
        // survive, but it is a poor way to learn that a policy typo'd
        // "auto_indx" — the one moment this can still be reported usefully is
        // before anything has been applied on its strength.
        if let Mode::Manual { overrides, .. } = &policy.mode {
            registry.validate_overrides(overrides)?;
        }
        let mut db = Self {
            store,
            policy,
            telemetry: Telemetry::new(),
            derived: DerivedStructures::new(),
            caches: QueryCaches::new(),
            optimizer: Optimizer {
                registry,
                controller: OptimizationController::new(),
                adaptive: adabt_opt::AdaptiveDriver::new(),
            },
            experiments: Experiments::new(),
            unique_constraints: crate::unique::UniqueConstraints::default(),
            tuning: Tuning {
                cluster_fields: HashMap::new(),
                delta_encoding: true,
                thread_per_core: false,
                join_order: false,
                data_partitioning: false,
            },
            execution: Execution {
                last_stats: ExecStats::default(),
                pending_cancel: None,
                slow_query: None,
            },
            next_txn_id: 1,
        };
        db.unique_constraints = crate::unique::read(db.store.dir());
        // Restore the indexes the log says existed.
        //
        // Definitions persist in the log; contents are derived. The cache below
        // is a shortcut, never a source of truth: if it is missing, stale or
        // damaged, every index is rebuilt by scanning the heap and the only
        // thing lost is the time. That is the rebuildability invariant paying
        // for itself — a derived representation can be cached carelessly
        // precisely because being wrong about it is always recoverable.
        let stamp = db.store.state_stamp()?;
        let cached = adabt_storage::derived::read(db.store.dir(), &stamp).unwrap_or_default();
        let defs: Vec<(String, String, String)> = db.store.index_definitions().to_vec();
        for (collection, field, kind) in defs {
            if let Some(k) = IndexKind::parse(&kind) {
                let key = index_cache_key(&collection, &field, k);
                let entries = cached
                    .iter()
                    .find(|(n, _)| *n == key)
                    .and_then(|(_, blob)| decode_index_entries(blob));
                db.create_index_from(&collection, &field, k, entries)?;
            }
        }
        // Restore the clustering declarations the catalog holds. The
        // declarations are state; the placement ranges are not, and rebuild
        // from subsequent keyed inserts.
        for (collection, field) in db.store.declared_cluster_fields() {
            db.tuning.cluster_fields.insert(collection, field);
        }
        // Restore optimizer-controlled global flags persisted via the catalog.
        db.tuning.delta_encoding = db.store.delta_encoding();
        db.tuning.thread_per_core = db.store.thread_per_core();
        crate::column::set_delta_enabled(db.tuning.delta_encoding);
        // Apply whatever the policy asks for, so opening at level 3 gives a
        // level-3 database rather than a level-0 one that drifts up later.
        db.optimize()?;
        Ok(db)
    }

    pub fn policy(&self) -> &Policy {
        &self.policy
    }

    pub fn probe(&self) -> Arc<CollectingProbe> {
        Arc::clone(&self.telemetry.probe)
    }

    pub fn telemetry(&self) -> Snapshot {
        self.telemetry.probe.snapshot()
    }

    pub fn last_exec_stats(&self) -> ExecStats {
        self.execution.last_stats
    }

    pub fn durability(&self) -> Durability {
        self.store.durability()
    }

    pub fn is_thread_per_core(&self) -> bool {
        self.tuning.thread_per_core
    }

    pub fn is_delta_encoding(&self) -> bool {
        self.tuning.delta_encoding
    }

    pub fn is_join_order(&self) -> bool {
        self.tuning.join_order
    }

    pub fn is_data_partitioning(&self) -> bool {
        self.tuning.data_partitioning
    }
}
