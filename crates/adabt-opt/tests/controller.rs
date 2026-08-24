//! Controller gating, and the symmetry between manual and adaptive control.

use adabt_core::index_kind::IndexKind;
use adabt_core::policy::{
    Consistency, Constraints, Durability, GuaranteeRequirements, Guarantees, Mode, Override, Policy,
};
use adabt_opt::action::{Action, ActionSink, ChangePlan};
use adabt_opt::config::{OptimizationConfig, Params};
use adabt_opt::controller::ApplyEnv;
use adabt_opt::cost::{AxisEffects, CostEstimate};
use adabt_opt::decision::{Decision, DecisionAction, Source, Verdict};
use adabt_opt::driver::{DriverInput, ManualDriver, OptimizationDriver};
use adabt_opt::optimization::{
    Applicability, OptContext, OptMeta, Optimization, Reversibility, ScopeKind,
};
use adabt_opt::AdaptiveDriver;
use adabt_opt::{OptimizationController, Registry};
use adabt_telemetry::Snapshot;

#[derive(Default)]
struct RecordingSink {
    actions: Vec<Action>,
    refuse: Option<String>,
}

impl ActionSink for RecordingSink {
    fn apply_action(&mut self, action: &Action) -> adabt_core::error::Result<()> {
        self.actions.push(action.clone());
        Ok(())
    }
    fn can_apply(&mut self, action: &Action) -> bool {
        match &self.refuse {
            Some(s) => !action.describe().contains(s.as_str()),
            None => true,
        }
    }
}

struct Fake {
    meta: OptMeta,
    ram: i64,
    applicable: Applicability,
}

impl Fake {
    fn new(name: &'static str) -> Self {
        Fake {
            meta: OptMeta {
                name,
                summary: "a test optimization",
                scope_kind: ScopeKind::Global,
                min_level: 1,
                axis_effects: AxisEffects::new(2, -1, 0),
                requires_guarantees: GuaranteeRequirements::ANY,
                prerequisites: &[],
                conflicts_with: &[],
                reversibility: Reversibility::Instant,
            },
            ram: 0,
            applicable: Applicability::Applicable,
        }
    }
    fn requiring(mut self, r: GuaranteeRequirements) -> Self {
        self.meta.requires_guarantees = r;
        self
    }
    fn needing(mut self, p: &'static [&'static str]) -> Self {
        self.meta.prerequisites = p;
        self
    }
    fn conflicting(mut self, c: &'static [&'static str]) -> Self {
        self.meta.conflicts_with = c;
        self
    }
    fn using_ram(mut self, bytes: i64) -> Self {
        self.ram = bytes;
        self
    }
    fn not_applicable(mut self, why: &str) -> Self {
        self.applicable = Applicability::NotYet(why.into());
        self
    }
    fn boxed(self) -> Box<dyn Optimization> {
        Box::new(self)
    }
}

impl Optimization for Fake {
    fn meta(&self) -> &OptMeta {
        &self.meta
    }
    fn applicability(&self, _: &OptContext<'_>) -> Applicability {
        self.applicable.clone()
    }
    fn estimate(&self, _: &OptContext<'_>) -> CostEstimate {
        CostEstimate::faster(0.5, 0.5).with_ram(self.ram)
    }
    fn plan_enable(&self, _: &OptContext<'_>, _scope: &str, _params: &Params) -> ChangePlan {
        ChangePlan::new(
            vec![Action::CreateIndex {
                collection: "users".into(),
                field: self.meta.name.into(),
                kind: IndexKind::Hash,
            }],
            vec![Action::DropIndex {
                collection: "users".into(),
                field: self.meta.name.into(),
                kind: IndexKind::Hash,
            }],
        )
    }
    fn plan_disable(&self, _: &OptContext<'_>, _scope: &str, _params: &Params) -> ChangePlan {
        ChangePlan::new(
            vec![Action::DropIndex {
                collection: "users".into(),
                field: self.meta.name.into(),
                kind: IndexKind::Hash,
            }],
            vec![],
        )
    }
}

fn enable(name: &'static str) -> Decision {
    Decision {
        optimization: name,
        scope: "global".into(),
        action: DecisionAction::Enable,
        params: Default::default(),
        trigger: "test".into(),
    }
}

fn disable(name: &'static str) -> Decision {
    Decision {
        optimization: name,
        scope: "global".into(),
        action: DecisionAction::Disable,
        params: Default::default(),
        trigger: "test".into(),
    }
}

struct Fixture {
    collections: Vec<(String, usize)>,
    filtered: Vec<(String, String, u64)>,
    fixed: Vec<String>,
    max_ids: Vec<(String, u64)>,
    indexes: Vec<(String, String, IndexKind)>,
}

impl Fixture {
    fn new() -> Self {
        Self {
            collections: vec![("users".into(), 10_000)],
            filtered: vec![],
            fixed: vec![],
            max_ids: vec![("users".into(), 9_999)],
            indexes: vec![],
        }
    }
    fn ctx<'a>(&'a self, policy: &'a Policy, snap: &'a Snapshot) -> OptContext<'a> {
        OptContext {
            policy,
            telemetry: snap,
            collections: &self.collections,
            filtered_fields: &self.filtered,
            fixed_size_collections: &self.fixed,
            max_ids: &self.max_ids,
            existing_indexes: &self.indexes,
            current_bytes: 0,
        }
    }
}

fn driver_input<'a>(
    reg: &'a Registry,
    current: &'a OptimizationConfig,
    policy: &'a Policy,
    snap: &'a Snapshot,
    ctx: &'a OptContext<'a>,
) -> DriverInput<'a> {
    DriverInput {
        registry: reg,
        current,
        policy,
        telemetry: snap,
        ctx,
        under_experiment: &[],
        pinned: &[],
    }
}

fn run(
    reg: &Registry,
    ctrl: &mut OptimizationController,
    decisions: Vec<Decision>,
    policy: &Policy,
) -> (adabt_opt::controller::ApplyReport, RecordingSink) {
    let snap = Snapshot::default();
    let fx = Fixture::new();
    let ctx = fx.ctx(policy, &snap);
    let mut sink = RecordingSink::default();
    let report = ctrl
        .apply(
            decisions,
            ApplyEnv {
                registry: reg,
                policy,
                ctx: &ctx,
            },
            &mut sink,
            Source::Manual,
        )
        .unwrap();
    (report, sink)
}

#[test]
fn an_applicable_optimization_is_applied_and_its_actions_reach_the_engine() {
    let mut reg = Registry::new();
    reg.register(Fake::new("cache").boxed());
    let mut ctrl = OptimizationController::new();
    let (report, sink) = run(
        &reg,
        &mut ctrl,
        vec![enable("cache")],
        &Policy::conventional(),
    );

    assert!(report.all_applied(), "{report:?}");
    assert_eq!(sink.actions.len(), 1);
    assert!(ctrl.config().is_enabled("cache", "global"));
    assert!(ctrl.explain("cache").contains("applied"));
}

#[test]
fn strict_durability_makes_a_relaxed_optimization_invisible_not_expensive() {
    let mut reg = Registry::new();
    reg.register(
        Fake::new("async_persist")
            .requiring(GuaranteeRequirements {
                max_durability: Some(Durability::Relaxed),
                max_consistency: None,
            })
            .boxed(),
    );
    let mut ctrl = OptimizationController::new();
    let (report, sink) = run(
        &reg,
        &mut ctrl,
        vec![enable("async_persist")],
        &Policy::conventional(),
    );

    assert_eq!(report.rejected[0].1, Verdict::ForbiddenByGuarantees);
    assert!(
        sink.actions.is_empty(),
        "a forbidden change reached the engine"
    );
    assert!(!ctrl.config().is_enabled("async_persist", "global"));
    let e = ctrl.explain("async_persist");
    assert!(e.contains("forbidden by guarantees"), "{e}");
    assert!(
        !e.contains("estimated p50"),
        "a forbidden change must not be priced: {e}"
    );
}

#[test]
fn relaxing_the_policy_makes_the_same_optimization_available() {
    let mut reg = Registry::new();
    reg.register(
        Fake::new("async_persist")
            .requiring(GuaranteeRequirements {
                max_durability: Some(Durability::Relaxed),
                max_consistency: None,
            })
            .boxed(),
    );
    let mut policy = Policy::conventional();
    policy.guarantees = Guarantees {
        durability: Durability::Relaxed,
        consistency: Consistency::Strict,
    };
    let mut ctrl = OptimizationController::new();
    let (report, _) = run(&reg, &mut ctrl, vec![enable("async_persist")], &policy);
    assert!(report.all_applied(), "{report:?}");
}

#[test]
fn a_hard_ram_ceiling_rejects_an_otherwise_good_optimization() {
    let mut reg = Registry::new();
    reg.register(Fake::new("big_cache").using_ram(8_000_000_000).boxed());
    let mut policy = Policy::conventional();
    policy.constraints = Constraints {
        max_ram_bytes: Some(1_000_000_000),
        ..Constraints::default()
    };
    let mut ctrl = OptimizationController::new();
    let (report, sink) = run(&reg, &mut ctrl, vec![enable("big_cache")], &policy);

    assert_eq!(report.rejected[0].1, Verdict::ExceedsConstraints);
    assert!(sink.actions.is_empty());
    assert!(ctrl.explain("big_cache").contains("ceiling"));
}

#[test]
fn constraints_are_checked_against_what_is_already_committed() {
    let mut reg = Registry::new();
    reg.register(Fake::new("a").using_ram(600_000_000).boxed());
    reg.register(Fake::new("b").using_ram(600_000_000).boxed());
    let mut policy = Policy::conventional();
    policy.constraints = Constraints {
        max_ram_bytes: Some(1_000_000_000),
        ..Constraints::default()
    };
    let mut ctrl = OptimizationController::new();
    let (report, _) = run(&reg, &mut ctrl, vec![enable("a"), enable("b")], &policy);
    assert_eq!(
        report.applied.len(),
        1,
        "both fit, so the ceiling did nothing"
    );
    assert_eq!(report.rejected[0].1, Verdict::ExceedsConstraints);
}

#[test]
fn a_missing_prerequisite_blocks_and_satisfying_it_unblocks() {
    let mut reg = Registry::new();
    reg.register(Fake::new("base").boxed());
    reg.register(Fake::new("advanced").needing(&["base"]).boxed());
    let policy = Policy::conventional();
    let mut ctrl = OptimizationController::new();

    let (report, _) = run(&reg, &mut ctrl, vec![enable("advanced")], &policy);
    assert_eq!(report.rejected[0].1, Verdict::MissingPrerequisite);

    let (report, _) = run(
        &reg,
        &mut ctrl,
        vec![enable("base"), enable("advanced")],
        &policy,
    );
    assert!(report.all_applied(), "{report:?}");
}

#[test]
fn a_conflict_blocks_the_second_optimization() {
    let mut reg = Registry::new();
    reg.register(
        Fake::new("row_store")
            .conflicting(&["column_store"])
            .boxed(),
    );
    reg.register(Fake::new("column_store").boxed());
    let policy = Policy::conventional();
    let mut ctrl = OptimizationController::new();
    let (report, _) = run(
        &reg,
        &mut ctrl,
        vec![enable("row_store"), enable("column_store")],
        &policy,
    );
    assert_eq!(report.applied.len(), 1);
    assert_eq!(report.rejected[0].1, Verdict::Conflicts);
}

#[test]
fn an_inapplicable_optimization_records_why() {
    let mut reg = Registry::new();
    reg.register(
        Fake::new("direct_lookup")
            .not_applicable("ids are not dense")
            .boxed(),
    );
    let mut ctrl = OptimizationController::new();
    let (report, _) = run(
        &reg,
        &mut ctrl,
        vec![enable("direct_lookup")],
        &Policy::conventional(),
    );
    assert_eq!(report.rejected[0].1, Verdict::NotApplicable);
    assert!(ctrl.explain("direct_lookup").contains("ids are not dense"));
}

#[test]
fn the_engine_can_refuse_a_change_it_cannot_make() {
    let mut reg = Registry::new();
    reg.register(Fake::new("cache").boxed());
    let policy = Policy::conventional();
    let snap = Snapshot::default();
    let fx = Fixture::new();
    let ctx = fx.ctx(&policy, &snap);
    let mut sink = RecordingSink {
        refuse: Some("create hash index".into()),
        ..Default::default()
    };
    let mut ctrl = OptimizationController::new();
    let report = ctrl
        .apply(
            vec![enable("cache")],
            ApplyEnv {
                registry: &reg,
                policy: &policy,
                ctx: &ctx,
            },
            &mut sink,
            Source::Manual,
        )
        .unwrap();
    assert_eq!(report.rejected[0].1, Verdict::NotApplicable);
    assert!(
        sink.actions.is_empty(),
        "a refused change was partly applied"
    );
}

#[test]
fn disabling_replays_the_exact_inverse_of_what_was_applied() {
    let mut reg = Registry::new();
    reg.register(Fake::new("cache").boxed());
    let policy = Policy::conventional();
    let mut ctrl = OptimizationController::new();

    let (_, sink_on) = run(&reg, &mut ctrl, vec![enable("cache")], &policy);
    let (_, sink_off) = run(&reg, &mut ctrl, vec![disable("cache")], &policy);

    assert!(!ctrl.config().is_enabled("cache", "global"));
    assert_eq!(sink_on.actions.len(), 1);
    assert_eq!(sink_off.actions.len(), 1);
    match (&sink_on.actions[0], &sink_off.actions[0]) {
        (Action::CreateIndex { field: a, .. }, Action::DropIndex { field: b, .. }) => {
            assert_eq!(a, b, "the inverse targeted something else")
        }
        other => panic!("expected create then drop, got {other:?}"),
    }
}

#[test]
fn every_decision_is_logged_including_rejections() {
    let mut reg = Registry::new();
    reg.register(Fake::new("ok").boxed());
    reg.register(Fake::new("nope").not_applicable("no").boxed());
    let mut ctrl = OptimizationController::new();
    run(
        &reg,
        &mut ctrl,
        vec![enable("ok"), enable("nope")],
        &Policy::conventional(),
    );
    assert_eq!(ctrl.log().len(), 2);
    let all = ctrl.explain_all();
    assert!(all.contains("applied"), "{all}");
    assert!(all.contains("not applicable"), "{all}");
}

#[test]
fn a_rejected_optimization_is_remembered_as_rejected() {
    let mut reg = Registry::new();
    reg.register(Fake::new("nope").not_applicable("no").boxed());
    let mut ctrl = OptimizationController::new();
    run(
        &reg,
        &mut ctrl,
        vec![enable("nope")],
        &Policy::conventional(),
    );
    assert!(ctrl.log().was_rejected("nope", "global"));
}

#[test]
fn the_manual_driver_turns_a_level_into_decisions() {
    let mut reg = Registry::new();
    for n in [
        "plan_cache",
        "result_cache",
        "auto_index",
        "page_compression",
    ] {
        reg.register(Fake::new(n).boxed());
    }
    let mut driver = ManualDriver;
    let policy = Policy::manual(2);
    let snap = Snapshot::default();
    let fx = Fixture::new();
    let ctx = fx.ctx(&policy, &snap);
    let empty = OptimizationConfig::new();
    let decisions = driver.decide(driver_input(&reg, &empty, &policy, &snap, &ctx));
    let names: Vec<&str> = decisions.iter().map(|d| d.optimization).collect();
    assert!(names.contains(&"plan_cache"), "{names:?}");
    assert!(names.contains(&"auto_index"), "{names:?}");
    assert!(decisions.iter().all(|d| d.trigger.contains("level 2")));
}

#[test]
fn the_manual_driver_settles_and_stops_proposing() {
    let mut reg = Registry::new();
    for n in ["plan_cache", "result_cache"] {
        reg.register(Fake::new(n).boxed());
    }
    let policy = Policy::manual(1);
    let mut driver = ManualDriver;
    let mut ctrl = OptimizationController::new();

    let snap = Snapshot::default();
    let fx = Fixture::new();
    let ctx = fx.ctx(&policy, &snap);
    let first = driver.decide(driver_input(&reg, ctrl.config(), &policy, &snap, &ctx));
    assert!(!first.is_empty());
    run(&reg, &mut ctrl, first, &policy);

    let second = driver.decide(driver_input(&reg, ctrl.config(), &policy, &snap, &ctx));
    assert!(second.is_empty(), "manual mode kept proposing: {second:?}");
}

#[test]
fn lowering_the_level_disables_what_it_no_longer_includes() {
    let mut reg = Registry::new();
    for n in [
        "plan_cache",
        "result_cache",
        "auto_index",
        "page_compression",
    ] {
        reg.register(Fake::new(n).boxed());
    }
    let mut driver = ManualDriver;
    let mut ctrl = OptimizationController::new();

    let snap = Snapshot::default();
    let fx = Fixture::new();

    let high = Policy::manual(2);
    let ctx_high = fx.ctx(&high, &snap);
    let d = driver.decide(driver_input(&reg, ctrl.config(), &high, &snap, &ctx_high));
    run(&reg, &mut ctrl, d, &high);
    assert!(ctrl.config().is_enabled_anywhere("auto_index"));

    let low = Policy::manual(1);
    let ctx_low = fx.ctx(&low, &snap);
    let d = driver.decide(driver_input(&reg, ctrl.config(), &low, &snap, &ctx_low));
    assert!(d.iter().any(|x| x.action == DecisionAction::Disable));
    run(&reg, &mut ctrl, d, &low);
    assert!(!ctrl.config().is_enabled_anywhere("auto_index"));
    assert!(ctrl.config().is_enabled_anywhere("plan_cache"));
}

#[test]
fn an_explicit_override_beats_the_level() {
    let mut reg = Registry::new();
    for n in ["plan_cache", "result_cache"] {
        reg.register(Fake::new(n).boxed());
    }
    let policy = Policy {
        mode: Mode::Manual {
            level: 1,
            overrides: vec![Override::toggle("result_cache", false)],
        },
        ..Policy::conventional()
    };
    let mut driver = ManualDriver;
    let snap = Snapshot::default();
    let fx = Fixture::new();
    let ctx = fx.ctx(&policy, &snap);
    let empty = OptimizationConfig::new();
    let decisions = driver.decide(driver_input(&reg, &empty, &policy, &snap, &ctx));
    let names: Vec<&str> = decisions.iter().map(|d| d.optimization).collect();
    assert!(names.contains(&"plan_cache"));
    assert!(!names.contains(&"result_cache"), "override was ignored");
}

#[test]
fn both_drivers_feed_the_same_controller_through_the_same_trait() {
    let mut reg = Registry::new();
    reg.register(Fake::new("plan_cache").boxed());
    reg.register(Fake::new("result_cache").boxed());

    let drivers: Vec<Box<dyn OptimizationDriver>> =
        vec![Box::new(ManualDriver), Box::new(AdaptiveDriver::default())];
    for mut d in drivers {
        let policy = Policy::manual(1);
        let mut ctrl = OptimizationController::new();
        let snap = Snapshot::default();
        let fx = Fixture::new();
        let ctx = fx.ctx(&policy, &snap);
        let decisions = d.decide(driver_input(&reg, ctrl.config(), &policy, &snap, &ctx));
        let mut sink = RecordingSink::default();
        ctrl.apply(
            decisions,
            ApplyEnv {
                registry: &reg,
                policy: &policy,
                ctx: &ctx,
            },
            &mut sink,
            d.source(),
        )
        .unwrap();
    }
}

#[test]
fn the_adaptive_driver_waits_for_evidence_before_acting() {
    // Acting on a handful of operations would be reacting to startup rather
    // than to a workload.
    let mut reg = Registry::new();
    reg.register(Fake::new("cache").boxed());
    let policy = Policy::conventional();
    let snap = Snapshot::default();
    let fx = Fixture::new();
    let ctx = fx.ctx(&policy, &snap);
    let empty = OptimizationConfig::new();

    let mut d = AdaptiveDriver::new();
    let decisions = d.decide(driver_input(&reg, &empty, &policy, &snap, &ctx));
    assert!(
        decisions.is_empty(),
        "acted on an empty workload: {decisions:?}"
    );
    assert_eq!(d.cycles(), 1, "it must still be reachable through the seam");
}
