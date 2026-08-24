//! Multi-objective scoring.
//!
//! The only place a `CostEstimate` vector is collapsed to a number, and it
//! happens here precisely because this is where the *policy* is in scope. A
//! scalar computed anywhere else would have to assume what the user wants.
//!
//! Nothing here decides eligibility. Guarantees and constraints are hard
//! filters applied by the controller before anything reaches this function; by
//! the time a candidate is scored it is already known to be permitted and
//! affordable, and scoring only ranks what survives.

use crate::cost::{AxisEffects, CostEstimate};
use adabt_core::policy::{Policy, Priorities};

/// A candidate's score, broken down so the reason can be explained.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Score {
    pub total: f64,
    pub speed_term: f64,
    pub resource_term: f64,
    pub freedom_term: f64,
    pub build_penalty: f64,
    pub confidence: f64,
}

impl Score {
    pub fn describe(&self) -> String {
        format!(
            "score {:.2} (speed {:+.2}, resources {:+.2}, freedom {:+.2}, build {:-.2}, confidence {:.0}%)",
            self.total,
            self.speed_term,
            self.resource_term,
            self.freedom_term,
            -self.build_penalty,
            self.confidence * 100.0
        )
    }
}

const GIB: f64 = 1_073_741_824.0;

/// Resource scale used when the policy sets no memory ceiling.
///
/// The axes have to be **commensurate** or the weights mean nothing. A latency
/// term is naturally bounded — a change cannot be more than 100% faster — while
/// a resource term measured in raw bytes is unbounded, so a six-gigabyte saving
/// would swamp any possible speed weighting and a `speed: 10` policy would keep
/// choosing whatever saved the most memory.
///
/// Expressing resources as a *fraction of the budget* fixes that. When the user
/// states a ceiling, that is the budget; otherwise this stands in for one.
const REFERENCE_BUDGET_BYTES: f64 = 8.0 * GIB;

/// Floor for the footprint scale, so a nearly-empty database does not make
/// every trivial saving look enormous.
const MIN_FOOTPRINT_BYTES: f64 = 1_048_576.0;

/// Seconds of build time treated as equivalent to one point of score.
const BUILD_SECS_PER_POINT: f64 = 30.0;

fn priorities(policy: &Policy) -> Priorities {
    policy.priority.clamped()
}

/// Score a candidate under a policy. Higher is better; negative means the
/// change is judged not worth making.
pub fn score(effects: &AxisEffects, est: &CostEstimate, policy: &Policy) -> Score {
    score_against(effects, est, policy, REFERENCE_BUDGET_BYTES as u64)
}

/// Score against an explicit resource scale.
///
/// `footprint` is what the database currently costs; savings are judged as a
/// fraction of it, so the same proportional win counts the same on a small
/// database as on a large one. A stated `max_ram` ceiling overrides it, because
/// a user who names a budget means that budget.
pub fn score_against(
    effects: &AxisEffects,
    est: &CostEstimate,
    policy: &Policy,
    footprint: u64,
) -> Score {
    let p = priorities(policy);

    // Latency improvement, weighted toward the tail: a p99 regression is felt
    // by users in a way a p50 improvement does not compensate for.
    // Clamped for the same reason: an estimate claiming a 50x speedup must not
    // be able to outvote every other consideration on the strength of a guess.
    let p50_gain = (1.0 - est.p50_delta.0).clamp(-1.0, 1.0);
    let p99_gain = (1.0 - est.p99_delta.0).clamp(-1.0, 1.0);
    let speed_term = p.speed as f64 * (0.4 * p50_gain + 0.6 * p99_gain);

    // Resources: bytes *saved* are positive, as a fraction of the budget so the
    // term is comparable to the latency one. CPU is charged separately because
    // a CPU-for-storage trade is exactly the kind this axis must express, and
    // netting them together would hide it.
    // A stated ceiling wins; otherwise judge against what the database costs
    // now, floored so an empty database cannot divide by nothing.
    let budget = policy
        .constraints
        .max_ram_bytes
        .map(|b| b as f64)
        .unwrap_or_else(|| (footprint as f64).max(MIN_FOOTPRINT_BYTES));
    let bytes_saved = (-(est.ram_bytes + est.storage_bytes) as f64 / budget).clamp(-1.0, 1.0);
    let resource_term = p.resources as f64 * (bytes_saved - est.cpu_frac);

    // Freedom comes from the declared axis effect: it is a statement about what
    // the user may still do, which no measurement captures.
    let freedom_term = p.freedom as f64 * (effects.freedom as f64 / 10.0);

    // Building is a one-off, but a change that takes minutes to apply should
    // lose to an equivalent one that is instant.
    let mut build_penalty = est.build.estimated_secs / BUILD_SECS_PER_POINT;
    if !est.build.online {
        // An offline build blocks the workload, which is worse than slow.
        build_penalty *= 3.0;
    }

    // Low confidence shrinks the magnitude of the claim rather than its sign:
    // an uncertain candidate becomes a weaker suggestion, not a rejected one.
    // Phase 7 exists to resolve exactly these by measurement.
    let raw = speed_term + resource_term + freedom_term - build_penalty;
    Score {
        total: raw * est.confidence,
        speed_term,
        resource_term,
        freedom_term,
        build_penalty,
        confidence: est.confidence,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cost::BuildCost;

    fn policy_of(speed: u8, resources: u8, freedom: u8) -> Policy {
        Policy {
            priority: Priorities {
                speed,
                resources,
                freedom,
            },
            ..Policy::conventional()
        }
    }

    /// A big cache: much faster, expensive in memory.
    fn cache() -> (AxisEffects, CostEstimate) {
        (
            AxisEffects::new(6, -5, 0),
            CostEstimate::faster(0.4, 0.4)
                .with_ram(4 * GIB as i64)
                .with_confidence(1.0),
        )
    }

    /// Compression: saves storage, costs CPU and a little latency.
    fn compression() -> (AxisEffects, CostEstimate) {
        let mut e = CostEstimate::faster(1.05, 1.08);
        e.storage_bytes = -(6 * GIB as i64);
        e.cpu_frac = 0.1;
        (AxisEffects::new(-1, 6, 0), e.with_confidence(1.0))
    }

    #[test]
    fn a_speed_priority_prefers_the_cache_and_a_resource_priority_prefers_compression() {
        // The premise of the whole project, reduced to one assertion: the same
        // two candidates, ranked oppositely by two policies.
        let (ce, cest) = cache();
        let (ze, zest) = compression();

        let fast = policy_of(10, 2, 5);
        assert!(
            score(&ce, &cest, &fast).total > score(&ze, &zest, &fast).total,
            "a speed-priority policy should prefer the cache"
        );

        let lean = policy_of(2, 10, 5);
        assert!(
            score(&ze, &zest, &lean).total > score(&ce, &cest, &lean).total,
            "a resource-priority policy should prefer compression"
        );
    }

    #[test]
    fn a_resource_priority_scores_a_memory_hungry_cache_negatively() {
        let (e, est) = cache();
        assert!(score(&e, &est, &policy_of(1, 10, 5)).total < 0.0);
    }

    #[test]
    fn a_speed_priority_scores_compression_negatively() {
        // It costs latency and CPU; under speed priority that is a loss.
        let (e, est) = compression();
        assert!(score(&e, &est, &policy_of(10, 1, 5)).total < 0.0);
    }

    #[test]
    fn a_tail_regression_outweighs_a_median_improvement() {
        let fast_median_slow_tail = CostEstimate {
            p50_delta: crate::cost::Ratio(0.5),
            p99_delta: crate::cost::Ratio(2.0),
            confidence: 1.0,
            ..Default::default()
        };
        let s = score(
            &AxisEffects::default(),
            &fast_median_slow_tail,
            &policy_of(10, 5, 5),
        );
        assert!(s.total < 0.0, "{}", s.describe());
    }

    #[test]
    fn low_confidence_shrinks_a_claim_without_flipping_it() {
        let (e, est) = cache();
        let policy = policy_of(10, 2, 5);
        let sure = score(&e, &est, &policy);
        let unsure = score(&e, &est.with_confidence(0.2), &policy);
        assert!(unsure.total.abs() < sure.total.abs());
        assert_eq!(
            unsure.total > 0.0,
            sure.total > 0.0,
            "confidence changed the sign of the judgment"
        );
    }

    #[test]
    fn an_expensive_build_loses_to_an_equivalent_instant_one() {
        let (e, est) = cache();
        let policy = policy_of(10, 2, 5);
        let instant = score(&e, &est, &policy);
        let slow = score(
            &e,
            &est.with_build(BuildCost {
                estimated_secs: 600.0,
                rows_read: 0,
                online: true,
            }),
            &policy,
        );
        assert!(slow.total < instant.total);
    }

    #[test]
    fn an_offline_build_is_penalised_harder_than_an_online_one() {
        let (e, est) = cache();
        let policy = policy_of(10, 2, 5);
        let build = |online| {
            score(
                &e,
                &est.with_build(BuildCost {
                    estimated_secs: 120.0,
                    rows_read: 0,
                    online,
                }),
                &policy,
            )
            .total
        };
        assert!(build(false) < build(true));
    }

    #[test]
    fn a_freedom_cost_matters_only_when_freedom_is_prioritised() {
        // Direct addressing: fast, but only legal while the schema stays fixed.
        let effects = AxisEffects::new(9, -7, -6);
        let est = CostEstimate::faster(0.4, 0.35).with_confidence(1.0);
        let indifferent = score(&effects, &est, &policy_of(10, 1, 0)).total;
        let protective = score(&effects, &est, &policy_of(10, 1, 10)).total;
        assert!(protective < indifferent);
    }

    #[test]
    fn a_neutral_change_scores_around_zero() {
        let s = score(
            &AxisEffects::default(),
            &CostEstimate::neutral(),
            &policy_of(5, 5, 5),
        );
        assert!(s.total.abs() < 1e-9, "{}", s.describe());
    }

    #[test]
    fn a_score_explains_its_own_terms() {
        let (e, est) = cache();
        let d = score(&e, &est, &policy_of(10, 3, 5)).describe();
        assert!(d.contains("speed"), "{d}");
        assert!(d.contains("resources"), "{d}");
        assert!(d.contains("confidence"), "{d}");
    }
}
