//! The M8 → M9 repair: an estimate that measurement refutes must stop arguing
//! for itself, rather than being held off by a cooldown.

use adabt_core::policy::{GuaranteeRequirements, Policy, Priorities};
use adabt_opt::action::{Action, ChangePlan};
use adabt_opt::config::Params;
use adabt_opt::cost::{AxisEffects, CostEstimate};
use adabt_opt::decision::DecisionAction;
use adabt_opt::driver::{DriverInput, OptimizationDriver};
use adabt_opt::model::{Metrics, Observation};
use adabt_opt::optimization::{
    Applicability, OptContext, OptMeta, Optimization, Reversibility, ScopeKind,
};
use adabt_opt::{AdaptiveDriver, CostModel, OptimizationConfig, Registry};
use adabt_telemetry::event::{Event, OpKind, QueryShape};
use adabt_telemetry::{CollectingProbe, Probe, Snapshot};

/// Claims a large latency win it will not deliver — the shape of estimate that
/// caused the M8 oscillation.
struct Optimistic;

const META: OptMeta = OptMeta {
    name: "optimistic",
    summary: "claims a large win",
    scope_kind: ScopeKind::Global,
    min_level: 1,
    axis_effects: AxisEffects::new(8, -1, 0),
    requires_guarantees: GuaranteeRequirements::ANY,
    prerequisites: &[],
    conflicts_with: &[],
    reversibility: Reversibility::Instant,
};

impl Optimization for Optimistic {
    fn meta(&self) -> &OptMeta {
        &META
    }
    fn applicability(&self, _: &OptContext<'_>) -> Applicability {
        Applicability::Applicable
    }
    fn estimate(&self, _: &OptContext<'_>) -> CostEstimate {
        CostEstimate::faster(0.3, 0.25).with_confidence(0.6)
    }
    fn plan_enable(&self, _: &OptContext<'_>, _scope: &str, _params: &Params) -> ChangePlan {
        ChangePlan::new(
            vec![Action::SetPrefetch(true)],
            vec![Action::SetPrefetch(false)],
        )
    }
    fn plan_disable(&self, _: &OptContext<'_>, _scope: &str, _params: &Params) -> ChangePlan {
        ChangePlan::new(vec![Action::SetPrefetch(false)], vec![])
    }
}

fn busy_snapshot() -> Snapshot {
    let p = CollectingProbe::new();
    for _ in 0..2_000 {
        p.record(Event::Op {
            collection: "users",
            kind: OpKind::Get,
            shape: QueryShape(7),
            nanos: 1_000,
            rows: 1,
        });
    }
    p.snapshot()
}

struct Fx {
    collections: Vec<(String, usize)>,
    filtered: Vec<(String, String, u64)>,
    fixed: Vec<String>,
    max_ids: Vec<(String, u64)>,
    indexes: Vec<(String, String, adabt_core::index_kind::IndexKind)>,
}

impl Fx {
    fn new() -> Self {
        Self {
            collections: vec![("users".into(), 50_000)],
            filtered: vec![("users".into(), "country".into(), 5_000)],
            fixed: vec![],
            max_ids: vec![("users".into(), 49_999)],
            indexes: vec![],
        }
    }
    fn ctx<'a>(&'a self, p: &'a Policy, s: &'a Snapshot) -> OptContext<'a> {
        OptContext {
            policy: p,
            telemetry: s,
            collections: &self.collections,
            filtered_fields: &self.filtered,
            fixed_size_collections: &self.fixed,
            max_ids: &self.max_ids,
            existing_indexes: &self.indexes,
            current_bytes: 4_000_000,
        }
    }
}

#[test]
fn an_uncalibrated_driver_proposes_the_optimistic_change() {
    let mut reg = Registry::new();
    reg.register(Box::new(Optimistic));
    let policy = Policy {
        priority: Priorities {
            speed: 10,
            resources: 2,
            freedom: 5,
        },
        ..Policy::conventional()
    };
    let snap = busy_snapshot();
    let fx = Fx::new();
    let ctx = fx.ctx(&policy, &snap);
    let empty = OptimizationConfig::new();

    let mut d = AdaptiveDriver::new();
    let decisions = d.decide(DriverInput {
        registry: &reg,
        current: &empty,
        policy: &policy,
        telemetry: &snap,
        ctx: &ctx,
        under_experiment: &[],
        pinned: &[],
    });
    assert!(
        decisions
            .iter()
            .any(|x| x.optimization == "optimistic" && x.action == DecisionAction::Enable),
        "the driver did not propose the change at all: {decisions:?}"
    );
}

#[test]
fn measurement_stops_a_refuted_estimate_from_being_proposed_again() {
    // The repair. In M8 this optimization would be proposed forever, because
    // the estimate never learned it was wrong and only a fifty-cycle cooldown
    // held it off.
    let mut model = CostModel::new();
    let flat = Metrics {
        p50_nanos: 1_000,
        p99_nanos: 5_000,
        bytes: 4_000_000,
        operations: 10_000,
    };
    for _ in 0..8 {
        model.record("optimistic", Observation::between(&flat, &flat).unwrap());
    }

    let prior = Optimistic.estimate(&Fx::new().ctx(&Policy::conventional(), &busy_snapshot()));
    assert!(prior.helps_latency(), "the prior should claim a win");

    let corrected = model.calibrate("optimistic", prior);
    assert!(
        !corrected.helps_latency(),
        "after eight measurements of no change the estimate still claims {:.2}x",
        corrected.p50_delta.0
    );

    // And with the corrected estimate, the score no longer justifies applying it.
    let policy = Policy {
        priority: Priorities {
            speed: 10,
            resources: 2,
            freedom: 5,
        },
        ..Policy::conventional()
    };
    let before = adabt_opt::score(&META.axis_effects, &prior, &policy).total;
    let after = adabt_opt::score(&META.axis_effects, &corrected, &policy).total;
    assert!(
        after < before,
        "correction did not reduce the score: {before:.2} -> {after:.2}"
    );
    assert!(after < 0.5, "a refuted change still scores {after:.2}");
}

#[test]
fn the_driver_folds_measurements_in_over_cycles() {
    let mut reg = Registry::new();
    reg.register(Box::new(Optimistic));
    let policy = Policy {
        priority: Priorities {
            speed: 10,
            resources: 2,
            freedom: 5,
        },
        ..Policy::conventional()
    };
    let snap = busy_snapshot();
    let fx = Fx::new();
    let ctx = fx.ctx(&policy, &snap);

    let mut d = AdaptiveDriver::new();
    let mut config = OptimizationConfig::new();

    // Cycle one enables it and registers a pending measurement.
    let decisions = d.decide(DriverInput {
        registry: &reg,
        current: &config,
        policy: &policy,
        telemetry: &snap,
        ctx: &ctx,
        under_experiment: &[],
        pinned: &[],
    });
    for x in &decisions {
        config.enable(x.optimization, x.scope.clone(), x.params.clone());
    }
    assert_eq!(d.pending_measurements(), decisions.len().min(2));

    // Later cycles, with the workload unchanged, fold in "it did nothing".
    for _ in 0..6 {
        d.decide(DriverInput {
            registry: &reg,
            current: &config,
            policy: &policy,
            telemetry: &snap,
            ctx: &ctx,
            under_experiment: &[],
            pinned: &[],
        });
    }
    assert!(
        d.model().observation_count("optimistic") > 0,
        "the driver never measured what it applied"
    );
    let d2 = d.model().describe("optimistic");
    assert!(d2.contains("measurement"), "{d2}");
}

/// A change must go on justifying itself by the standard that admitted it.
///
/// Retraction used to require a score below `-MIN_SCORE` while admission
/// required one above `+MIN_SCORE`, leaving a band between them in which a
/// change could never be reconsidered. Calibration walks changes into exactly
/// that band: correcting an optimistic prior pulls the score toward zero, not
/// below it.
///
/// `docs/diagnosis.md` records the standing measurement of this — the soak's
/// aggregate phase never improves, because `column_store` is applied on a
/// 40%-confidence prior, scores 0.54, and is then never re-examined.
#[test]
fn a_change_whose_prior_measurement_refutes_is_eventually_retracted() {
    let mut reg = Registry::new();
    reg.register(Box::new(Optimistic));
    let policy = Policy {
        priority: Priorities {
            speed: 10,
            resources: 2,
            freedom: 5,
        },
        ..Policy::conventional()
    };
    let snap = busy_snapshot();
    let fx = Fx::new();
    let ctx = fx.ctx(&policy, &snap);

    // A model that has been shown, repeatedly, that this change does nothing.
    let mut model = CostModel::new();
    for _ in 0..10 {
        model.record(
            "optimistic",
            Observation {
                p50_ratio: 1.0,
                p99_ratio: 1.0,
                byte_delta: 0,
            },
        );
    }
    let mut d = AdaptiveDriver::with_model(model);

    // Already applied, on the optimistic prior it was admitted with.
    let mut config = OptimizationConfig::new();
    config.enable("optimistic", "global".to_string(), Default::default());

    let mut retracted = None;
    for _ in 0..8 {
        let decisions = d.decide(DriverInput {
            registry: &reg,
            current: &config,
            policy: &policy,
            telemetry: &snap,
            ctx: &ctx,
            under_experiment: &[],
            pinned: &[],
        });
        if let Some(x) = decisions
            .iter()
            .find(|x| x.optimization == "optimistic" && x.action == DecisionAction::Disable)
        {
            retracted = Some(x.trigger.clone());
            break;
        }
    }
    let why = retracted.unwrap_or_else(|| {
        panic!(
            "a change measurement had refuted ten times over was kept anyway; \
             model says: {}",
            d.model().describe("optimistic")
        )
    });
    assert!(why.contains("bar that admitted it"), "{why}");
}

#[test]
fn a_pinned_scope_is_never_retracted_however_it_scores() {
    // Some structures are load-bearing for correctness rather than for speed —
    // an index backing a unique constraint is the case this exists for. Dropping
    // one does not make the database slower, it makes it wrong, so no amount of
    // cost-benefit arithmetic may reach that conclusion.
    let mut reg = Registry::new();
    reg.register(Box::new(Optimistic));
    let policy = Policy {
        priority: Priorities {
            speed: 10,
            resources: 2,
            freedom: 5,
        },
        ..Policy::conventional()
    };
    let snap = busy_snapshot();
    let fx = Fx::new();
    let ctx = fx.ctx(&policy, &snap);

    let mut config = OptimizationConfig::new();
    config.enable("optimistic", "global".to_string(), Default::default());

    let mut d = AdaptiveDriver::new();
    for _ in 0..20 {
        let decisions = d.decide(DriverInput {
            registry: &reg,
            current: &config,
            policy: &policy,
            telemetry: &snap,
            ctx: &ctx,
            under_experiment: &[],
            pinned: &[("optimistic", "global".to_string())],
        });
        assert!(
            !decisions
                .iter()
                .any(|x| x.optimization == "optimistic" && x.action == DecisionAction::Disable),
            "a pinned scope was retracted: {decisions:?}"
        );
    }
}

/// M31: retraction became arithmetic against the policy's weights instead of
/// a bare "was it used at all".
///
/// The gap this closes: telemetry measured only the *benefit* of an index
/// (`IndexUsed`), so an index the planner still picked occasionally could
/// never be retracted no matter how much the write path paid to maintain it.
/// `Event::IndexMaintained` supplies the cost half, recorded where the cost
/// is actually paid.
mod cost_benefit_retraction {
    use adabt_core::policy::Priorities;
    use adabt_telemetry::event::Event;
    use adabt_telemetry::{CollectingProbe, Probe};

    fn snapshot_with(uses: u32, writes: u32) -> adabt_telemetry::Snapshot {
        let probe = CollectingProbe::new();
        for _ in 0..uses {
            probe.record(Event::IndexUsed {
                collection: "users",
                field: "country",
            });
        }
        for _ in 0..writes {
            probe.record(Event::IndexMaintained {
                collection: "users",
                field: "country",
            });
        }
        probe.snapshot()
    }

    #[test]
    fn maintenance_is_counted_separately_from_use() {
        let s = snapshot_with(3, 700);
        assert_eq!(s.index_use_count("users", "country"), 3);
        assert_eq!(s.index_maintenance_count("users", "country"), 700);
        // An untouched field reports zero rather than absent, so the
        // arithmetic downstream never has to special-case a missing entry.
        assert_eq!(s.index_maintenance_count("users", "nope"), 0);
    }

    #[test]
    fn a_resources_heavy_policy_tolerates_fewer_writes_per_use_than_a_speed_heavy_one() {
        // The property that makes this "arithmetic against the policy's
        // weights" rather than a constant: the same index is a keep under one
        // policy and a loss under another, and the priorities say which.
        let speed = Priorities {
            speed: 10,
            resources: 1,
            freedom: 5,
        }
        .clamped();
        let resources = Priorities {
            speed: 1,
            resources: 10,
            freedom: 5,
        }
        .clamped();
        let tolerance = |p: Priorities| 50.0 * (1.0 + p.speed as f64) / (1.0 + p.resources as f64);
        assert!(
            tolerance(speed) > tolerance(resources),
            "a speed-first policy must tolerate a more expensive index: {} vs {}",
            tolerance(speed),
            tolerance(resources)
        );
        // And the band is wide enough to actually separate real cases rather
        // than being a rounding difference.
        assert!(tolerance(speed) / tolerance(resources) > 10.0);
    }
}
