//! Optimization levels.
//!
//! A level is a **preset**, not a state. It names a region of optimization
//! space in a way a human can hold in their head, resolves into an
//! `OptimizationConfig`, and is then forgotten: nothing in the engine branches
//! on a level.
//!
//! Two consequences follow, and both are deliberate. Explicit user settings
//! override a level rather than being overridden by it. And the adaptive driver
//! may move to a configuration that no level names, which it must be able to do
//! — workloads do not come in round numbers, and levels need not be traversed
//! in order.

use crate::config::{OptimizationConfig, Params};

/// The highest level the vocabulary defines. Levels above what is implemented
/// are accepted and simply enable everything available.
pub const MAX_LEVEL: u8 = 11;

/// What each level is meant to mean, for `--help` and documentation.
pub fn level_description(level: u8) -> &'static str {
    match level {
        0 => "conventional: general-purpose, minimal aggressive optimization",
        1 => "basic: plan and result caching, statistics",
        2 => "automatic physical: index selection, page compression",
        3 => "aggressive execution: prefetching, larger caches",
        4 => "physical layout: column stores, materialized views",
        5 => "workload-aware: workload-specific indexes, hot/cold separation",
        6..=9 => "increasing specialisation: compiled paths, per-workload structures",
        10 => "extreme: direct addressing, generality removed from hot paths",
        11 => "maximum: a workload-specific data appliance",
        _ => "beyond the defined range",
    }
}

/// One optimization turned on by a level, with its default tuning.
pub struct LevelEntry {
    pub optimization: &'static str,
    pub scope: &'static str,
    pub params: &'static [(&'static str, i64)],
}

const fn entry(optimization: &'static str, params: &'static [(&'static str, i64)]) -> LevelEntry {
    LevelEntry {
        optimization,
        scope: "global",
        params,
    }
}

/// Cumulative preset for a level: everything this level and every lower level
/// enables.
///
/// Only optimizations that actually exist appear here. Page compression,
/// A level names only what the engine actually does. Listing an unimplemented
/// optimization would make a level claim something it does not deliver, and the
/// benchmark matrix would then read "no improvement" where the truth is "not
/// built yet". `adabt_engine::optimizations::NOT_YET_IMPLEMENTED` tracks any
/// remaining gap; it is currently empty.
///
/// Cumulative on purpose. A level that dropped a lower level's optimization
/// would make the ladder non-monotonic and the mental model useless — "higher
/// is more specialised" has to stay true even though the *adaptive* driver is
/// free to ignore the ladder entirely.
pub fn level_preset(level: u8) -> Vec<LevelEntry> {
    let mut out = Vec::new();
    if level >= 1 {
        out.push(entry("plan_cache", &[("entries", 512)]));
        out.push(entry("result_cache", &[("entries", 256)]));
    }
    if level >= 2 {
        out.push(entry(
            "auto_index",
            &[("min_rows", 1000), ("min_queries", 8)],
        ));
        // The one optimization that trades resources *down* rather than up.
        out.push(entry("record_compression", &[]));
    }
    if level >= 3 {
        // Bigger caches: the level-3 posture spends memory for latency.
        out.push(entry("plan_cache", &[("entries", 4096)]));
        out.push(entry("result_cache", &[("entries", 4096)]));
        out.push(entry("buffer_pool", &[("pages", 8192)]));
        out.push(entry("prefetch", &[]));
    }
    if level >= 4 {
        out.push(entry("column_store", &[]));
        out.push(entry("delta_encoding", &[]));
        out.push(entry("materialized_view", &[]));
    }
    if level >= 5 {
        // Level 5 is "workload-aware", and a composite index is the clearest
        // example of the idea: it is chosen from which fields this workload
        // constrains *together*, which is a fact about the traffic and not
        // about the schema. Above `auto_index` because it costs more per write
        // and serves a narrower set of queries, so it is worth reaching for
        // only once the single-field indexes exist and are not enough.
        out.push(entry("auto_composite_index", &[]));
        // Same tier, same reasoning: a covering index is chosen from what
        // this workload projects alongside its filters, which is traffic and
        // not schema. Registered separately from `auto_index` because it
        // costs more per write and answers a narrower question.
        out.push(entry("auto_covering_index", &[]));
        out.push(entry("clustered_sort", &[]));
    }
    if level >= 8 {
        // Freezing is what makes direct addressing legal for a collection that
        // did not start out fixed, which is why it sits below it.
        out.push(entry("freeze_schema", &[]));
    }
    if level >= 9 {
        out.push(entry("thread_per_core", &[]));
    }
    if level >= 10 {
        out.push(entry("direct_lookup", &[]));
    }
    out
}

/// Resolve a level into a configuration.
pub fn config_for_level(level: u8) -> OptimizationConfig {
    let mut cfg = OptimizationConfig::new();
    for e in level_preset(level) {
        let mut p = Params::new();
        for (k, v) in e.params {
            p = p.with(*k, *v);
        }
        // Later entries win, which is how level 3 widens the caches level 1 set.
        cfg.enable(e.optimization, e.scope, p);
    }
    cfg
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_zero_enables_nothing() {
        assert!(config_for_level(0).is_empty());
    }

    #[test]
    fn levels_are_cumulative() {
        // Everything on at level N must still be on at level N+1.
        for level in 0..MAX_LEVEL {
            let lo = config_for_level(level);
            let hi = config_for_level(level + 1);
            for (name, scope, _) in lo.entries() {
                assert!(
                    hi.is_enabled(name, scope),
                    "level {} dropped {name}[{scope}] from level {level}",
                    level + 1
                );
            }
        }
    }

    #[test]
    fn a_higher_level_widens_rather_than_duplicates_a_cache() {
        let l1 = config_for_level(1);
        let l3 = config_for_level(3);
        let at = |c: &OptimizationConfig| {
            c.params("plan_cache", "global")
                .unwrap()
                .get("entries")
                .unwrap()
        };
        assert!(at(&l3) > at(&l1), "level 3 should widen the plan cache");
        // And not create a second entry for the same scope.
        assert_eq!(
            l3.entries().filter(|(n, _, _)| *n == "plan_cache").count(),
            1
        );
    }

    #[test]
    fn direct_lookup_appears_only_at_the_extreme_levels() {
        assert!(!config_for_level(5).is_enabled_anywhere("direct_lookup"));
        assert!(config_for_level(10).is_enabled_anywhere("direct_lookup"));
        assert!(config_for_level(11).is_enabled_anywhere("direct_lookup"));
    }

    #[test]
    fn a_level_beyond_the_defined_range_is_accepted() {
        // Clamping or panicking would make the ladder brittle; enabling
        // everything available is the honest reading of "higher than I know".
        let cfg = config_for_level(255);
        assert!(cfg.is_enabled_anywhere("direct_lookup"));
    }

    #[test]
    fn every_level_has_a_description() {
        for l in 0..=MAX_LEVEL {
            assert!(!level_description(l).is_empty());
        }
    }
}
