//! The built-in optimization library.
//!
//! Each of these is an independently switchable, reversible, measurable
//! physical strategy. They are registered explicitly by `register_builtins` so
//! the set is greppable and its order deterministic.
//!
//! Only optimizations that are genuinely implemented are registered; anything
//! named elsewhere but not built goes in `NOT_YET_IMPLEMENTED` (currently
//! empty), and the controller rejects a decision naming it as "not registered"
//! rather than quietly succeeding. An optimization that reports success and does nothing is worse
//! than a missing one: it makes the benchmark matrix lie.
//!
//! The optimizations are grouped by what they change, one family per module:
//!
//! - `caches`: `plan_cache`, `result_cache`, `materialized_view`
//! - `paging`: `buffer_pool`, `prefetch`
//! - `auto_index`: `auto_index`
//! - `composite_index`: `auto_composite_index`
//! - `covering_index`: `auto_covering_index`
//! - `encoding`: `record_compression`, `column_store`, `delta_encoding`
//! - `schema`: `freeze_schema`, `direct_lookup`
//! - `placement`: `clustered_sort`
//! - `execution`: `thread_per_core`

use adabt_opt::Registry;

mod auto_index;
mod caches;
mod composite_index;
mod covering_index;
mod encoding;
mod execution;
mod paging;
mod placement;
mod schema;

pub use auto_index::AutoIndexOpt;
pub use caches::{MaterializedViewOpt, PlanCacheOpt, ResultCacheOpt};
pub use composite_index::AutoCompositeIndexOpt;
pub use covering_index::AutoCoveringIndexOpt;
pub use encoding::{ColumnStoreOpt, DeltaEncodingOpt, RecordCompressionOpt};
pub use execution::ThreadPerCoreOpt;
pub use paging::{BufferPoolOpt, PrefetchOpt};
pub use placement::ClusteredSortOpt;
pub use schema::{DirectLookupOpt, FreezeSchemaOpt};

/// Register every implemented optimization, in dependency order.
pub fn register_builtins(registry: &mut Registry) {
    registry.register(Box::new(PlanCacheOpt));
    registry.register(Box::new(ResultCacheOpt));
    registry.register(Box::new(BufferPoolOpt));
    registry.register(Box::new(AutoIndexOpt));
    registry.register(Box::new(AutoCompositeIndexOpt));
    registry.register(Box::new(AutoCoveringIndexOpt));
    registry.register(Box::new(RecordCompressionOpt));
    registry.register(Box::new(ColumnStoreOpt));
    registry.register(Box::new(FreezeSchemaOpt));
    registry.register(Box::new(DirectLookupOpt));
    registry.register(Box::new(PrefetchOpt));
    registry.register(Box::new(MaterializedViewOpt));
    registry.register(Box::new(ClusteredSortOpt));
    registry.register(Box::new(DeltaEncodingOpt));
    registry.register(Box::new(ThreadPerCoreOpt));
}

/// Split a `"collection.field"` scope. Returns `None` for a scope that names no
/// field, which is how a per-field optimization declines a global request.
fn split_scope(scope: &str) -> Option<(String, String)> {
    let (c, f) = scope.split_once('.')?;
    if c.is_empty() || f.is_empty() {
        return None;
    }
    Some((c.to_string(), f.to_string()))
}

/// Documented but unimplemented, so the gap is visible in one place.
/// Optimizations the level table names but the engine does not implement.
///
/// Empty, for now. Kept rather than deleted because the list existing is what
/// makes "not implemented" a declaration instead of a silence — a registry that
/// simply lacked an entry would be indistinguishable from one that had never
/// considered it.
pub const NOT_YET_IMPLEMENTED: &[(&str, &str)] = &[];

#[cfg(test)]
mod tests {
    use super::*;
    use adabt_core::index_kind::IndexKind;
    use adabt_core::policy::Policy;
    use adabt_opt::config::Params;
    use adabt_opt::optimization::{OptContext, Reversibility};
    use adabt_telemetry::Snapshot;

    pub(super) struct Fx {
        pub(super) collections: Vec<(String, usize)>,
        pub(super) filtered: Vec<(String, String, u64)>,
        pub(super) fixed: Vec<String>,
        max_ids: Vec<(String, u64)>,
        pub(super) indexes: Vec<(String, String, IndexKind)>,
        pub(super) snap: Snapshot,
        policy: Policy,
    }

    impl Fx {
        pub(super) fn new() -> Self {
            Self {
                collections: vec![("users".into(), 10_000)],
                filtered: vec![],
                fixed: vec![],
                max_ids: vec![("users".into(), 9_999)],
                indexes: vec![],
                snap: Snapshot::default(),
                policy: Policy::conventional(),
            }
        }
        /// Record `n` queries that pinned `fields` to literals together.
        ///
        /// Sorted and de-duplicated the way `Database::note_filtered_fields`
        /// does, so the fixture and the real emitter agree about what one
        /// observation looks like.
        pub(super) fn pin(&mut self, collection: &str, fields: &[&str], n: u64) {
            let mut fields: Vec<String> = fields.iter().map(|f| f.to_string()).collect();
            fields.sort();
            fields.dedup();
            *self
                .snap
                .pinned_sets
                .entry((collection.to_string(), fields))
                .or_default() += n;
        }

        pub(super) fn ctx(&self) -> OptContext<'_> {
            OptContext {
                policy: &self.policy,
                telemetry: &self.snap,
                collections: &self.collections,
                filtered_fields: &self.filtered,
                fixed_size_collections: &self.fixed,
                max_ids: &self.max_ids,
                existing_indexes: &self.indexes,
                current_bytes: 0,
            }
        }
    }

    #[test]
    fn every_builtin_registers_and_orders() {
        let mut r = Registry::new();
        register_builtins(&mut r);
        // One per name in the module docs' list of families.
        assert_eq!(r.len(), 15);
        assert!(r.dependency_order().is_ok());
        for n in r.names() {
            assert!(!r.meta(n).unwrap().summary.is_empty(), "{n} has no summary");
        }
    }

    /// Every registered optimization must be reachable from some level.
    ///
    /// A level preset is the only thing that ever *requests* an optimization.
    /// An optimization can therefore be fully written, fully tested, and
    /// registered, and still never run once — which is exactly what happened
    /// to `auto_composite_index` the first time it was added here, and to
    /// `set_log_archive`, `set_pool_capacity`, `WorkloadMemory` and
    /// `best_combination` before it.
    ///
    /// This is that whole class of bug reduced to one assertion. It is not a
    /// style check: an optimization absent from every preset is dead code that
    /// the registry, the dependency order and its own unit tests all report as
    /// healthy.
    ///
    /// If an optimization genuinely should be manual-only, the right move is
    /// to say so here in the exception list, deliberately — not to leave it
    /// out of the presets and let silence stand for intent.
    #[test]
    fn every_registered_optimization_is_reachable_from_some_level() {
        let mut r = Registry::new();
        register_builtins(&mut r);

        let mut reachable: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for level in 0..=adabt_opt::MAX_LEVEL {
            for e in &adabt_opt::level_preset(level) {
                reachable.insert(e.optimization);
            }
        }

        /// Optimizations deliberately reachable only by an explicit manual
        /// override. Empty, and adding to it should require a reason.
        const MANUAL_ONLY: &[&str] = &[];

        let orphaned: Vec<String> = r
            .names()
            .iter()
            .filter(|n| !reachable.contains(*n) && !MANUAL_ONLY.contains(n))
            .map(|n| n.to_string())
            .collect();

        assert!(
            orphaned.is_empty(),
            "these optimizations are registered but no level ever enables them, \
             so they can never run: {orphaned:?}"
        );
    }

    /// And the converse: a level must not name an optimization that does not
    /// exist, or it silently promises something the engine will not do.
    #[test]
    fn every_level_entry_names_a_registered_optimization() {
        let mut r = Registry::new();
        register_builtins(&mut r);
        let registered: std::collections::HashSet<String> =
            r.names().iter().map(|n| n.to_string()).collect();

        for level in 0..=adabt_opt::MAX_LEVEL {
            for e in &adabt_opt::level_preset(level) {
                assert!(
                    registered.contains(e.optimization),
                    "level {level} enables `{}`, which is not registered",
                    e.optimization
                );
            }
        }
    }

    #[test]
    fn every_optimization_plans_a_reversible_change() {
        let mut fx = Fx::new();
        fx.fixed = vec!["users".into()];
        fx.filtered = vec![("users".into(), "country".into(), 50)];
        let ctx = fx.ctx();
        let mut r = Registry::new();
        register_builtins(&mut r);
        for opt in r.iter() {
            let scopes = opt.candidate_scopes(&ctx);
            let scope = scopes
                .first()
                .cloned()
                .unwrap_or_else(|| "global".to_string());
            let p = opt.plan_enable(&ctx, &scope, &Params::default());
            if p.apply.is_empty() {
                continue;
            }
            if opt.meta().reversibility == Reversibility::Destructive {
                // Allowed to have no inverse — and for that reason never
                // applied automatically. See the driver's check.
                continue;
            }
            assert!(
                !p.revert.is_empty(),
                "{} plans a change it cannot undo but is not declared Destructive",
                opt.meta().name
            );
        }
    }

    #[test]
    fn unimplemented_optimizations_are_declared_rather_than_hidden() {
        let mut r = Registry::new();
        register_builtins(&mut r);
        for (name, _) in NOT_YET_IMPLEMENTED {
            assert!(
                !r.contains(name),
                "{name} is listed as unimplemented but is registered"
            );
        }
    }
}

#[cfg(test)]
mod resource_axis_tests {
    use super::*;
    use adabt_opt::Registry;

    /// The reason `record_compression` exists.
    ///
    /// Nearly every other optimization spends resources to buy latency (the one
    /// other saver, `freeze_schema`, pays in freedom instead, which a policy may
    /// forbid). A policy of `resources: 10, speed: 3` would otherwise have had
    /// little or nothing to select, which
    /// means the three-axis premise was structurally untestable rather than
    /// merely untested.
    #[test]
    fn at_least_one_optimization_reduces_resources() {
        let mut r = Registry::new();
        register_builtins(&mut r);
        let savers: Vec<&str> = r
            .iter()
            .filter(|o| o.meta().axis_effects.resources > 0)
            .map(|o| o.meta().name)
            .collect();
        assert!(
            !savers.is_empty(),
            "no optimization trades in the resource-saving direction; \
             a resources-priority policy would have nothing to choose"
        );
        assert!(savers.contains(&"record_compression"), "{savers:?}");
    }

    #[test]
    fn the_axes_now_point_in_both_directions() {
        let mut r = Registry::new();
        register_builtins(&mut r);
        let speed_buyers = r.iter().filter(|o| o.meta().axis_effects.speed > 0).count();
        let resource_savers = r
            .iter()
            .filter(|o| o.meta().axis_effects.resources > 0)
            .count();
        assert!(speed_buyers > 0);
        assert!(resource_savers > 0);
    }
}

#[cfg(test)]
mod irreversibility_tests {
    use super::tests::Fx;
    use super::*;
    use adabt_core::policy::{Policy, Priorities};
    use adabt_opt::driver::{DriverInput, OptimizationDriver};
    use adabt_opt::optimization::Reversibility;
    use adabt_opt::{AdaptiveDriver, OptimizationConfig, Registry};
    use adabt_telemetry::event::{Event, OpKind, QueryShape};
    use adabt_telemetry::{CollectingProbe, Probe};

    /// Every safety mechanism in the optimizer — measurement, retraction,
    /// shadow comparison, canary rollback — assumes a bad decision can be taken
    /// back. Where it cannot, the decision is not the optimizer's to make.
    #[test]
    fn the_adaptive_driver_never_proposes_an_irreversible_change() {
        let mut reg = Registry::new();
        register_builtins(&mut reg);

        let probe = CollectingProbe::new();
        for _ in 0..5_000 {
            probe.record(Event::Op {
                collection: "users",
                kind: OpKind::Get,
                shape: QueryShape(1),
                nanos: 1_000,
                rows: 1,
            });
        }
        let snap = probe.snapshot();

        let mut fx = Fx::new();
        fx.collections = vec![("users".into(), 100_000)];
        fx.snap = snap;
        let policy = Policy {
            priority: Priorities {
                speed: 10,
                resources: 10,
                freedom: 0,
            },
            ..Policy::conventional()
        };
        let ctx = fx.ctx();
        let empty = OptimizationConfig::new();

        let mut d = AdaptiveDriver::new();
        for _ in 0..20 {
            let decisions = d.decide(DriverInput {
                registry: &reg,
                current: &empty,
                policy: &policy,
                telemetry: &fx.snap,
                ctx: &ctx,
                under_experiment: &[],
                pinned: &[],
            });
            for x in &decisions {
                let meta = reg.meta(x.optimization).unwrap();
                assert_ne!(
                    meta.reversibility,
                    Reversibility::Destructive,
                    "the driver proposed an irreversible change: {}",
                    x.optimization
                );
            }
        }
        // Even with freedom weighted at zero, which is the policy most likely
        // to want it.
        assert!(reg
            .iter()
            .any(|o| o.meta().reversibility == Reversibility::Destructive));
    }
}
