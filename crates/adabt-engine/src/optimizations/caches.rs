//! Optimizations that keep an answer instead of recomputing it: the plan
//! cache, the result cache and materialized views.
//!
//! All three spend memory, or write-path work, to skip repeating a computation
//! whose result is already known.

use crate::caches::{RESULT_CACHE_ENTRIES, RESULT_CACHE_GHOST};
use adabt_core::policy::GuaranteeRequirements;
use adabt_opt::action::{Action, ChangePlan};
use adabt_opt::config::Params;
use adabt_opt::cost::{AxisEffects, CostEstimate};
use adabt_opt::optimization::{
    Applicability, OptContext, OptMeta, Optimization, Reversibility, ScopeKind,
};

// -- plan cache -------------------------------------------------------------

pub struct PlanCacheOpt;

const PLAN_CACHE_META: OptMeta = OptMeta {
    name: "plan_cache",
    summary: "reuse physical plans across queries of the same shape",
    scope_kind: ScopeKind::Global,
    min_level: 1,
    axis_effects: AxisEffects::new(2, -1, 0),
    requires_guarantees: GuaranteeRequirements::ANY,
    prerequisites: &[],
    conflicts_with: &[],
    reversibility: Reversibility::Instant,
};

impl Optimization for PlanCacheOpt {
    fn meta(&self) -> &OptMeta {
        &PLAN_CACHE_META
    }
    fn applicability(&self, _ctx: &OptContext<'_>) -> Applicability {
        Applicability::Applicable
    }
    fn estimate(&self, _ctx: &OptContext<'_>) -> CostEstimate {
        // Planning is a small part of a query's cost, so the benefit is modest
        // and the confidence low until measured.
        CostEstimate::faster(0.95, 0.95)
            .with_ram(512 * 4_096)
            .with_confidence(0.4)
    }
    fn plan_enable(&self, ctx: &OptContext<'_>, _scope: &str, _params: &Params) -> ChangePlan {
        let n = entries(ctx, 512);
        ChangePlan::new(
            vec![Action::SetPlanCacheEntries(n)],
            vec![Action::SetPlanCacheEntries(0)],
        )
    }
    fn plan_disable(&self, _ctx: &OptContext<'_>, _scope: &str, _params: &Params) -> ChangePlan {
        ChangePlan::new(vec![Action::SetPlanCacheEntries(0)], vec![])
    }
}

// -- result cache -----------------------------------------------------------

pub struct ResultCacheOpt;

const RESULT_CACHE_META: OptMeta = OptMeta {
    name: "result_cache",
    summary: "serve repeated identical queries from memory",
    scope_kind: ScopeKind::Global,
    min_level: 1,
    axis_effects: AxisEffects::new(5, -4, 0),
    requires_guarantees: GuaranteeRequirements::ANY,
    prerequisites: &[],
    conflicts_with: &[],
    reversibility: Reversibility::Instant,
};

impl Optimization for ResultCacheOpt {
    fn meta(&self) -> &OptMeta {
        &RESULT_CACHE_META
    }
    fn applicability(&self, _ctx: &OptContext<'_>) -> Applicability {
        // Structurally always possible. Whether it is a good *idea* under a
        // write-heavy workload is a scoring judgment, and it belongs in the
        // estimate below rather than here.
        //
        // Putting it here was a bug worth recording: a bulk load makes the
        // workload look 100% writes, so a database loaded and then queried
        // would refuse its own result cache forever on the strength of its
        // loading phase. Applicability answers "is this possible", not "is this
        // wise" — conflating the two lets a transient workload permanently
        // veto an optimization.
        Applicability::Applicable
    }
    /// Judged on what the cache did, or would have done, to this traffic.
    ///
    /// Whether a result cache pays is a fact about the traffic — does the same
    /// query come back before a write makes its answer stale — and no prior
    /// can know it. The engine measures it either way: a cache that is on
    /// counts its hits, and one that is off runs a ghost of itself that counts
    /// the hits it would have had. The memory side is measured the same way,
    /// as the bytes the cache holds or would hold, not its capacity.
    ///
    /// The prior this replaces claimed a fixed 50% for a fixed 1 MiB and, on
    /// the soak's point-filter phase (an 85% would-be hit rate on ~40 KB),
    /// never cleared the bar; see `docs/benchmarks/expert-vs-adaptive.md`.
    fn estimate(&self, ctx: &OptContext<'_>) -> CostEstimate {
        let evidence = CacheEvidence::of(ctx);
        let p50_gain = HIT_SAVING * evidence.hit_rate;
        // The tail is made of misses for as long as more than one query in a
        // hundred misses: a cache moves the 99th percentile only once it
        // answers nearly everything.
        let p99_gain = HIT_SAVING * ((evidence.hit_rate - 0.99) / 0.01).clamp(0.0, 1.0);
        CostEstimate::faster(1.0 - p50_gain, 1.0 - p99_gain)
            .with_ram(evidence.resident_bytes as i64)
            .with_maintenance(0.02)
            .with_confidence(evidence.confidence())
    }
    fn plan_enable(&self, ctx: &OptContext<'_>, _scope: &str, _params: &Params) -> ChangePlan {
        // The size the ghost simulates, so the cache that is turned on is the
        // one whose hit rate was measured.
        let n = entries(ctx, RESULT_CACHE_ENTRIES);
        ChangePlan::new(
            vec![Action::SetResultCacheEntries(n)],
            vec![Action::SetResultCacheEntries(0)],
        )
    }
    fn plan_disable(&self, _ctx: &OptContext<'_>, _scope: &str, _params: &Params) -> ChangePlan {
        ChangePlan::new(vec![Action::SetResultCacheEntries(0)], vec![])
    }
}

/// Share of a query's cost that a hit still pays: the key lookup and the copy
/// of the cached rows.
///
/// A tenth is generous for the multi-row answers a result cache earns its keep
/// on. The expert comparison measured hits at about 2 µs against executions of
/// 0.3–1.6 ms, about a hundredth. Erring high is the right side to err on,
/// because calibration corrects it once the cache has been measured on.
const HIT_SAVING: f64 = 0.9;

/// Probes at which the measured hit rate earns half of [`MAX_CACHE_CONFIDENCE`].
///
/// A hit rate measured over `n` probes has a standard error of at most
/// `0.5 / sqrt(n)`: five points at 100, the precision at which the difference
/// between "worth it" and "not" stops being noise.
const HALF_TRUST_PROBES: f64 = 100.0;

/// Ceiling on confidence in the cache estimate.
///
/// The hit rate is measured, not guessed, but what a hit saves is still
/// [`HIT_SAVING`]'s assumption, so the estimate never claims certainty.
const MAX_CACHE_CONFIDENCE: f64 = 0.9;

/// What telemetry has seen of the result cache, or of its ghost while it is
/// off.
struct CacheEvidence {
    /// Fraction of the queries that reached the cache which it answered, or
    /// would have answered.
    hit_rate: f64,
    /// Queries the hit rate rests on.
    probes: u64,
    /// Bytes the cache holds, or would hold.
    resident_bytes: u64,
}

impl CacheEvidence {
    fn of(ctx: &OptContext<'_>) -> Self {
        let t = ctx.telemetry;
        // Every query that reaches the cache records a probe under
        // `result_cache` (a miss, while it is off), and while it is off the
        // ghost also records whether it would have hit. Real hits plus ghost
        // hits over real probes is therefore the hit rate in either state,
        // and across a switch between them while telemetry remembers both.
        let probes = t.probes("result_cache");
        let hits = t.cache_hits.get("result_cache").copied().unwrap_or(0)
            + t.cache_hits.get(RESULT_CACHE_GHOST).copied().unwrap_or(0);
        let hit_rate = if probes == 0 {
            0.0
        } else {
            (hits as f64 / probes as f64).min(1.0)
        };
        Self {
            hit_rate,
            probes,
            resident_bytes: t
                .cache_resident_bytes
                .get("result_cache")
                .copied()
                .unwrap_or(0),
        }
    }

    /// Grows with the evidence: nothing on no probes, half of
    /// [`MAX_CACHE_CONFIDENCE`] at [`HALF_TRUST_PROBES`].
    fn confidence(&self) -> f64 {
        let n = self.probes as f64;
        MAX_CACHE_CONFIDENCE * n / (n + HALF_TRUST_PROBES)
    }
}

// -- materialized views -----------------------------------------------------

pub struct MaterializedViewOpt;

const MATERIALIZED_VIEW_META: OptMeta = OptMeta {
    name: "materialized_view",
    summary: "keep grouped counts up to date on write instead of recomputing them",
    scope_kind: ScopeKind::Global,
    min_level: 4,
    // A large speed effect on the queries it covers — a handful of numbers
    // instead of every row — paid for on the write path, where every insert and
    // delete now adjusts a total. The freedom axis is untouched: nothing about
    // the data or the schema is constrained by keeping a count of it.
    axis_effects: AxisEffects::new(7, -2, 0),
    requires_guarantees: GuaranteeRequirements::ANY,
    prerequisites: &[],
    conflicts_with: &[],
    reversibility: Reversibility::RebuildRequired,
};

/// Below this, recomputing the aggregate is cheap enough that maintaining it on
/// every write costs more than it saves.
const MIN_ROWS_FOR_VIEW: usize = 2_000;

impl Optimization for MaterializedViewOpt {
    fn meta(&self) -> &OptMeta {
        &MATERIALIZED_VIEW_META
    }

    fn applicability(&self, ctx: &OptContext<'_>) -> Applicability {
        let biggest = ctx.collections.iter().map(|(_, n)| *n).max().unwrap_or(0);
        if biggest < MIN_ROWS_FOR_VIEW {
            return Applicability::NotYet(format!(
                "the largest collection holds {biggest} rows, below the \
                 {MIN_ROWS_FOR_VIEW} at which maintaining a total beats recomputing it"
            ));
        }
        Applicability::Applicable
    }

    fn estimate(&self, ctx: &OptContext<'_>) -> CostEstimate {
        // The saving is enormous where it applies and nil where it does not, and
        // which of those is true depends on how much of the workload aggregates
        // — something only telemetry knows. Hence the low confidence: this is an
        // estimate that expects to be corrected by measurement.
        let read_share = 1.0 - ctx.telemetry.write_fraction();
        CostEstimate::faster(1.0 - 0.5 * read_share, 1.0 - 0.5 * read_share)
            .with_maintenance(0.03)
            .with_confidence(0.35)
    }

    fn plan_enable(&self, _ctx: &OptContext<'_>, _scope: &str, _params: &Params) -> ChangePlan {
        ChangePlan::new(
            vec![Action::SetMaterializedViews(true)],
            vec![Action::SetMaterializedViews(false)],
        )
    }

    fn plan_disable(&self, _ctx: &OptContext<'_>, _scope: &str, _params: &Params) -> ChangePlan {
        ChangePlan::new(
            vec![Action::SetMaterializedViews(false)],
            vec![Action::SetMaterializedViews(true)],
        )
    }
}

/// Cache size from the level preset, falling back to a default.
fn entries(ctx: &OptContext<'_>, default: usize) -> usize {
    let _ = ctx;
    default
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::optimizations::tests::Fx;
    use adabt_telemetry::{CollectingProbe, Event, Probe as _};

    /// Telemetry in which `probes` queries reached the result cache while it
    /// was off, `ghost_hits` of which it would have answered, holding
    /// `resident` bytes.
    fn ghost(probes: u64, ghost_hits: u64, resident: u64) -> adabt_telemetry::Snapshot {
        let probe = CollectingProbe::new();
        for i in 0..probes {
            probe.record(Event::CacheProbe {
                name: "result_cache",
                hit: false,
            });
            probe.record(Event::CacheProbe {
                name: RESULT_CACHE_GHOST,
                hit: i < ghost_hits,
            });
        }
        probe.record(Event::CacheResident {
            name: "result_cache",
            bytes: resident,
        });
        probe.snapshot()
    }

    #[test]
    fn with_no_evidence_the_result_cache_claims_nothing() {
        let fx = Fx::new();
        let e = ResultCacheOpt.estimate(&fx.ctx());
        assert!(!e.helps_latency(), "{e:?}");
        assert_eq!(e.confidence, 0.0);
    }

    #[test]
    fn the_claimed_gain_follows_the_measured_hit_rate() {
        let mut fx = Fx::new();
        fx.snap = ghost(1_000, 850, 40_000);
        let repeated = ResultCacheOpt.estimate(&fx.ctx());
        fx.snap = ghost(1_000, 50, 40_000);
        let distinct = ResultCacheOpt.estimate(&fx.ctx());

        assert!((repeated.p50_delta.0 - (1.0 - HIT_SAVING * 0.85)).abs() < 1e-9);
        assert!(distinct.p50_delta.0 > 0.9, "{distinct:?}");
        // Fifteen misses in a hundred are the tail; the cache cannot move it.
        assert_eq!(repeated.p99_delta.0, 1.0);
    }

    #[test]
    fn the_memory_charged_is_what_the_cache_would_hold_not_its_capacity() {
        let mut fx = Fx::new();
        fx.snap = ghost(1_000, 850, 40_000);
        assert_eq!(ResultCacheOpt.estimate(&fx.ctx()).ram_bytes, 40_000);
    }

    #[test]
    fn confidence_grows_with_the_probes_the_hit_rate_rests_on() {
        let mut fx = Fx::new();
        fx.snap = ghost(10, 9, 1_000);
        let thin = ResultCacheOpt.estimate(&fx.ctx()).confidence;
        fx.snap = ghost(HALF_TRUST_PROBES as u64, 90, 1_000);
        let half = ResultCacheOpt.estimate(&fx.ctx()).confidence;
        fx.snap = ghost(5_000, 4_500, 1_000);
        let thick = ResultCacheOpt.estimate(&fx.ctx()).confidence;

        assert!(thin < half && half < thick, "{thin} {half} {thick}");
        assert!((half - MAX_CACHE_CONFIDENCE / 2.0).abs() < 1e-9);
        assert!(thick < MAX_CACHE_CONFIDENCE);
    }

    #[test]
    fn a_cache_that_is_on_is_judged_by_its_own_hits() {
        // While the cache is on the ghost is silent; the real hits are the
        // evidence, and keeping the cache must not depend on the ghost.
        let probe = CollectingProbe::new();
        for i in 0..1_000 {
            probe.record(Event::CacheProbe {
                name: "result_cache",
                hit: i < 800,
            });
        }
        let mut fx = Fx::new();
        fx.snap = probe.snapshot();
        let e = ResultCacheOpt.estimate(&fx.ctx());
        assert!((e.p50_delta.0 - (1.0 - HIT_SAVING * 0.8)).abs() < 1e-9);
    }

    #[test]
    fn a_write_heavy_workload_is_judged_by_its_hits_without_vetoing_the_cache() {
        // Writes invalidate answers, which the ghost sees as misses. A bulk
        // load therefore lowers the estimate through the measured hit rate,
        // and must still not make the cache impossible.
        let probe = CollectingProbe::new();
        for _ in 0..200 {
            probe.record(Event::Op {
                collection: "users",
                kind: adabt_telemetry::OpKind::Insert,
                shape: adabt_telemetry::QueryShape::UNKNOWN,
                nanos: 1,
                rows: 1,
            });
        }
        let mut fx = Fx::new();
        fx.snap = probe.snapshot();
        assert!(ResultCacheOpt.applicability(&fx.ctx()).is_applicable());
        assert!(!ResultCacheOpt.estimate(&fx.ctx()).helps_latency());
    }
}
