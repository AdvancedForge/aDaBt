//! The experiment loop against one live engine.
//!
//! `experiments.rs` exercises the shadow *primitive* using two databases over
//! identical data, which isolates the comparison but is not how a deployment
//! works. These run the real thing: one engine, one dataset, the candidate built
//! in place and hidden from the planner until it has earned its way out.
//!
//! The claim being tested throughout is the one the whole project rests on —
//! **the answer never changes.** Not while the candidate is being built, not
//! while it is serving one percent of traffic, not after it is promoted, and not
//! after it is thrown away.

use adabt_core::ids::RecordId;
use adabt_core::policy::{Mode, Policy, Priorities};
use adabt_core::record::Record;
use adabt_core::schema::{FieldDef, FieldType, Schema, SchemaMode};
use adabt_core::store::LogicalStore;
use adabt_engine::Database;
use adabt_ir::plan::{LogicalOp, LogicalPlan};
use adabt_ir::Expr;
use adabt_opt::decision::{Decision, DecisionAction};
use adabt_opt::experiment::{Guardrails, Phase};
use std::path::{Path, PathBuf};

struct Tmp(PathBuf);
impl Tmp {
    fn new(tag: &str) -> Self {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "adabt-live-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&p);
        Tmp(p)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}
impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

const COUNTRIES: [&str; 4] = ["NO", "SE", "DK", "FI"];
const N: u64 = 1_000;

fn schema() -> Schema {
    Schema::new(
        SchemaMode::Fixed,
        vec![
            FieldDef::new("id", FieldType::U64).required(),
            FieldDef::new("country", FieldType::Char(8)),
            FieldDef::new("age", FieldType::I64),
        ],
    )
    .unwrap()
}

fn seeded(dir: &Path) -> Database {
    let mut db = Database::open(dir, Policy::manual(0)).unwrap();
    db.create_collection("users", schema()).unwrap();
    for i in 0..N {
        db.insert(
            "users",
            RecordId(i),
            Record::new()
                .with("id", i)
                .with("country", COUNTRIES[(i % 4) as usize])
                .with("age", (i % 70) as i64),
        )
        .unwrap();
    }
    // Warm the telemetry. `auto_index` refuses to build an index for a field
    // nothing has ever filtered on, and rightly so — an experiment is proposed
    // *because* the workload showed something, and starting one against a cold
    // database would be testing the runner on a decision the optimizer would
    // never make.
    for i in 0..40 {
        db.query(&equality_query(COUNTRIES[i % 4])).unwrap();
    }
    db
}

fn equality_query(c: &str) -> LogicalPlan {
    LogicalPlan::new(LogicalOp::scan("users").filter(Expr::eq("country", c)))
}

fn index_decision() -> Decision {
    Decision::new(
        "auto_index",
        "users.country",
        DecisionAction::Enable,
        "test",
    )
}

fn guardrails() -> Guardrails {
    Guardrails {
        // Small enough to reach a verdict in a test, large enough that a single
        // scheduling hiccup cannot decide one.
        min_samples: 10,
        ..Guardrails::default()
    }
}

/// The answers to every query, as a fingerprint that must never change.
fn answers(db: &mut Database) -> Vec<Vec<(RecordId, Record)>> {
    COUNTRIES
        .iter()
        .map(|c| {
            let mut rows = db.query(&equality_query(c)).unwrap();
            rows.sort_by_key(|(id, _)| id.0);
            rows
        })
        .collect()
}

fn drive(db: &mut Database, queries: usize) {
    for i in 0..queries {
        db.query(&equality_query(COUNTRIES[i % 4])).unwrap();
    }
}

/// Run an experiment all the way to a terminal phase, driving enough traffic at
/// each step for the phase to reach a verdict of its own.
fn run_to_verdict(db: &mut Database) -> Phase {
    let mut phase = Phase::Proposed;
    for _ in 0..40 {
        phase = db.advance_experiment().unwrap().unwrap();
        if phase.is_terminal() {
            return phase;
        }
        // At one percent, ten candidate samples cost a thousand queries. The
        // ramp is deliberately expensive at the bottom.
        let needed = match phase {
            Phase::Shadow => 30,
            Phase::Canary(p) => 20 * 100 / p as usize,
            _ => 0,
        };
        drive(db, needed);
    }
    phase
}

#[test]
fn a_candidate_index_is_built_but_the_planner_cannot_see_it() {
    let t = Tmp::new("hidden");
    let mut db = seeded(t.path());
    let plan_before = db.plan(&equality_query("NO")).explain();

    db.begin_experiment(index_decision(), guardrails()).unwrap();
    assert_eq!(db.advance_experiment().unwrap(), Some(Phase::Building));

    // The index exists...
    assert_eq!(
        db.index_specs().len(),
        1,
        "the candidate was never actually built"
    );
    // ...and the planner still plans as though it does not.
    assert_eq!(
        db.plan(&equality_query("NO")).explain(),
        plan_before,
        "the candidate leaked into the plan before it had proved anything"
    );
}

#[test]
fn shadow_compares_two_genuinely_different_paths() {
    // The sharpest test of the mask there is. If hiding failed, both halves of
    // every pair would take the index and the ratio would sit at 1.0; a hash
    // index on an equality filter over a thousand rows is worth several times
    // that when it is only one side of the comparison.
    let t = Tmp::new("mask");
    let mut db = seeded(t.path());
    db.begin_experiment(index_decision(), guardrails()).unwrap();
    db.advance_experiment().unwrap(); // -> Building
    db.advance_experiment().unwrap(); // -> Shadow
    drive(&mut db, 60);

    let e = db.experiment().expect("the experiment ended early");
    assert_eq!(e.shadow.trials, 60, "queries did not reach the shadow");
    assert!(
        e.shadow.is_correct(),
        "the candidate disagreed: {:?}",
        e.shadow.first_divergence
    );
    assert!(
        e.shadow.p50_ratio() < 0.5,
        "the two paths performed alike, which means the mask is not working: {}",
        e.shadow.describe()
    );
}

#[test]
fn a_healthy_index_ramps_through_shadow_and_canary_to_promotion() {
    let t = Tmp::new("promote");
    let mut db = seeded(t.path());
    db.begin_experiment(index_decision(), guardrails()).unwrap();

    let phase = run_to_verdict(&mut db);
    assert_eq!(phase, Phase::Promoted, "{}", db.explain_experiment());

    let e = &db.finished_experiments()[0];
    let path: Vec<&str> = e.experiment.history.iter().map(|p| p.as_str()).collect();
    assert!(
        path.contains(&"shadow"),
        "traffic moved without a shadow: {path:?}"
    );
    let shadow_at = path.iter().position(|p| *p == "shadow").unwrap();
    let canary_at = path.iter().position(|p| *p == "canary").unwrap();
    assert!(shadow_at < canary_at, "canary before shadow: {path:?}");

    // Every canary step was taken. Jumping straight to full traffic would make
    // the ramp decorative.
    let steps: Vec<u8> = e
        .experiment
        .history
        .iter()
        .filter_map(|p| match p {
            Phase::Canary(n) => Some(*n),
            _ => None,
        })
        .collect();
    assert_eq!(steps, vec![1, 10, 50, 90], "{steps:?}");

    // And the index the experiment built is still there, in use.
    assert_eq!(db.index_specs().len(), 1);
    assert!(
        db.plan(&equality_query("NO")).explain().contains("index"),
        "the promoted index is not being planned: {}",
        db.plan(&equality_query("NO")).explain()
    );
}

#[test]
fn the_answer_is_identical_at_every_phase_of_an_experiment() {
    // The property the entire project exists to preserve, checked at each point
    // where the physical layer is in a different state.
    let t = Tmp::new("identical");
    let mut db = seeded(t.path());
    let expected = answers(&mut db);

    db.begin_experiment(index_decision(), guardrails()).unwrap();
    let mut checked = 0;
    for _ in 0..40 {
        let phase = db.advance_experiment().unwrap().unwrap();
        assert_eq!(
            answers(&mut db),
            expected,
            "the answer changed in phase {}",
            phase.as_str()
        );
        checked += 1;
        if phase.is_terminal() {
            break;
        }
        let needed = match phase {
            Phase::Shadow => 30,
            Phase::Canary(p) => 20 * 100 / p as usize,
            _ => 0,
        };
        drive(&mut db, needed);
    }
    assert!(checked >= 6, "only {checked} phases were reached");
    assert_eq!(
        answers(&mut db),
        expected,
        "the answer changed after the run"
    );
}

#[test]
fn a_canary_serves_the_fraction_of_traffic_its_phase_names() {
    let t = Tmp::new("fraction");
    let mut db = seeded(t.path());
    db.begin_experiment(index_decision(), guardrails()).unwrap();
    db.advance_experiment().unwrap(); // Building
    db.advance_experiment().unwrap(); // Shadow
    drive(&mut db, 30);
    db.advance_experiment().unwrap(); // Canary(1)
    assert_eq!(db.experiment().unwrap().phase(), Phase::Canary(1));

    drive(&mut db, 1_000);
    // The counts reach the verdict only when they are folded, which happens on
    // the next advance.
    db.advance_experiment().unwrap();
    let e = db
        .finished_experiments()
        .last()
        .map(|e| &e.experiment)
        .or_else(|| db.experiment().map(|e| &e.experiment))
        .expect("no experiment");
    assert_eq!(
        e.candidate.samples, 10,
        "one percent of a thousand queries is ten, not {}",
        e.candidate.samples
    );
    assert_eq!(
        e.baseline.samples, 990,
        "the other ninety-nine percent did not take the old path"
    );
}

#[test]
fn aborting_an_experiment_removes_what_it_built() {
    let t = Tmp::new("abort");
    let mut db = seeded(t.path());
    let expected = answers(&mut db);

    db.begin_experiment(index_decision(), guardrails()).unwrap();
    db.advance_experiment().unwrap(); // Building
    assert_eq!(db.index_specs().len(), 1);

    db.abort_experiment("operator changed their mind").unwrap();
    assert!(db.experiment().is_none());
    assert_eq!(
        db.index_specs().len(),
        0,
        "the abandoned candidate was left behind"
    );
    assert_eq!(answers(&mut db), expected);

    let e = &db.finished_experiments()[0];
    assert_eq!(e.experiment.phase, Phase::Reverted);
}

#[test]
fn a_change_that_rewrites_the_primary_cannot_be_experimented_on() {
    // Compression and schema freezing leave no old path to compare against, so
    // there is nothing an experiment could measure. Refused with a reason rather
    // than accepted and quietly measured against itself.
    let t = Tmp::new("unshadowable");
    let mut db = seeded(t.path());
    for opt in ["record_compression", "freeze_schema"] {
        let d = Decision::new(opt, "users", DecisionAction::Enable, "test");
        let err = db.begin_experiment(d, guardrails()).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("cannot be experimented on") || msg.contains("nothing to do"),
            "{opt}: {msg}"
        );
    }
    assert!(db.experiment().is_none());
}

/// Two experiments on the *same* collection would each measure the other.
///
/// This used to be the rule for every pair, because the mask and the
/// "candidates are visible" flag were both global: a second experiment's
/// unproven structures were exposed to the first's canary traffic. Both are
/// now per-experiment, so the remaining requirement is only that the two
/// scopes do not overlap — which this checks is still enforced.
#[test]
fn two_experiments_on_the_same_collection_are_refused() {
    let t = Tmp::new("solo");
    let mut db = seeded(t.path());
    db.begin_experiment(index_decision(), guardrails()).unwrap();
    let err = db
        .begin_experiment(
            Decision::new("auto_index", "users.age", DecisionAction::Enable, "test"),
            guardrails(),
        )
        .unwrap_err();
    assert!(
        err.to_string().contains("same traffic"),
        "the refusal should explain the overlap, not just decline: {err}"
    );
}

/// The feature: two experiments on collections that share no traffic run at
/// the same time, and each one's candidate stays hidden from the other's
/// queries.
#[test]
fn experiments_on_separate_collections_run_together() {
    let t = Tmp::new("concurrent");
    let mut db = seeded(t.path());
    db.create_collection("orders", Schema::dynamic()).unwrap();
    for i in 0..2_000u64 {
        db.insert(
            "orders",
            RecordId(i),
            Record::new()
                .with("region", if i % 2 == 0 { "north" } else { "south" })
                .with("total", (i % 100) as i64),
        )
        .unwrap();
    }

    let a = db.begin_experiment(index_decision(), guardrails()).unwrap();
    let b = db
        .begin_experiment(
            Decision::new(
                "auto_index",
                "orders.region",
                DecisionAction::Enable,
                "test",
            ),
            guardrails(),
        )
        .expect("a disjoint scope must be allowed");

    assert_ne!(a, b);
    assert_eq!(
        db.experiments().count(),
        2,
        "both experiments should be running"
    );

    // Drive both to a verdict together. Neither may see the other's
    // structures, and both must still return correct answers throughout.
    let users_plan = LogicalPlan::new(LogicalOp::scan("users").filter(Expr::eq("country", "NO")));
    let orders_plan =
        LogicalPlan::new(LogicalOp::scan("orders").filter(Expr::eq("region", "north")));
    // Derived, not assumed: `seeded` spreads users over four countries, and
    // a hardcoded count here would fail for a reason that has nothing to do
    // with what is being tested.
    let users_expected = db.query(&users_plan).unwrap().len();
    let orders_expected = db.query(&orders_plan).unwrap().len();
    assert!(users_expected > 0 && orders_expected > 0);

    for round in 0..60 {
        db.advance_experiments().unwrap();
        if db.experiments().count() == 0 {
            break;
        }
        // Drive as much traffic as the *slowest* running phase needs. A 1%
        // canary wants a thousand queries for ten candidate samples, so a flat
        // loop never reaches a verdict — it stalls at the bottom of the ramp
        // with the experiment permanently `Inconclusive`.
        let needed = db
            .experiments()
            .map(|e| match e.phase() {
                Phase::Shadow => 30usize,
                Phase::Canary(p) => 20 * 100 / p as usize,
                _ => 0,
            })
            .max()
            .unwrap_or(0);
        for i in 0..needed.max(1) {
            assert_eq!(
                db.query(&users_plan).unwrap().len(),
                users_expected,
                "users answer changed at round {round}, query {i}"
            );
            assert_eq!(
                db.query(&orders_plan).unwrap().len(),
                orders_expected,
                "orders answer changed at round {round}, query {i}"
            );
        }
    }

    assert_eq!(
        db.experiments().count(),
        0,
        "both experiments should have reached a verdict"
    );
    assert_eq!(
        db.finished_experiments().len(),
        2,
        "both should be recorded as finished"
    );
    // Whatever each decided, the answers never moved — which is the invariant
    // that two concurrent experiments were previously forbidden to protect.
    assert_eq!(db.query(&users_plan).unwrap().len(), users_expected);
    assert_eq!(db.query(&orders_plan).unwrap().len(), orders_expected);
}

/// Retiring one experiment must not unmask the other's unproven structure.
///
/// This is the concrete hazard that made concurrency unsafe. It is asserted
/// through the planner rather than the mask, because what matters is whether a
/// query can *reach* the structure.
#[test]
fn retiring_one_experiment_leaves_the_others_candidate_hidden() {
    let t = Tmp::new("retire-one");
    let mut db = seeded(t.path());
    db.create_collection("orders", Schema::dynamic()).unwrap();
    for i in 0..2_000u64 {
        db.insert(
            "orders",
            RecordId(i),
            Record::new().with("region", if i % 2 == 0 { "north" } else { "south" }),
        )
        .unwrap();
    }

    let a = db.begin_experiment(index_decision(), guardrails()).unwrap();
    let b = db
        .begin_experiment(
            Decision::new(
                "auto_index",
                "orders.region",
                DecisionAction::Enable,
                "test",
            ),
            guardrails(),
        )
        .unwrap();

    // Get both to the point where their candidates exist but are masked.
    let orders_plan =
        LogicalPlan::new(LogicalOp::scan("orders").filter(Expr::eq("region", "north")));
    for _ in 0..40 {
        db.advance_experiments().unwrap();
        db.query(&orders_plan).unwrap();
    }

    // Abort one. The other's candidate must stay invisible to the planner.
    db.abort_experiment_by_id(a, "test").unwrap();
    assert!(
        !db.experiments().any(|e| e.experiment.id == a),
        "the aborted experiment should no longer be running"
    );

    if db.experiments().any(|e| e.experiment.id == b) {
        let explain = db.explain(&orders_plan);
        assert!(
            !explain.contains("IndexLookup"),
            "aborting experiment #{a} exposed experiment #{b}'s unproven index:\n{explain}"
        );
    }
}

#[test]
fn a_disable_is_not_something_to_experiment_with() {
    let t = Tmp::new("disable");
    let mut db = seeded(t.path());
    let d = Decision::new(
        "auto_index",
        "users.country",
        DecisionAction::Disable,
        "test",
    );
    let err = db.begin_experiment(d, guardrails()).unwrap_err();
    assert!(err.to_string().contains("only an enable"), "{err}");
}

#[test]
fn the_log_distinguishes_a_trial_from_its_verdict() {
    // A structure built for an experiment genuinely was applied, and saying so
    // is correct — but "applied for a trial" and "kept because it earned it"
    // are different facts and the log has to hold both.
    let t = Tmp::new("log");
    let mut db = seeded(t.path());
    db.begin_experiment(index_decision(), guardrails()).unwrap();
    run_to_verdict(&mut db);

    let text = db.explain_optimization("auto_index");
    assert!(text.contains("trialled by experiment #1"), "{text}");
    assert!(text.contains("promoted"), "{text}");
}

#[test]
fn a_reverted_experiment_says_why_in_the_log() {
    let t = Tmp::new("why");
    let mut db = seeded(t.path());
    db.begin_experiment(index_decision(), guardrails()).unwrap();
    db.advance_experiment().unwrap();
    db.abort_experiment("the operator pulled it").unwrap();

    let text = db.explain_optimization("auto_index");
    assert!(text.contains("the operator pulled it"), "{text}");
    assert!(text.contains("reverted"), "{text}");
}

#[test]
fn the_optimizer_proves_a_derived_change_instead_of_applying_it() {
    // The point of the whole loop: in adaptive mode the database does not put a
    // new index straight into service, it proposes one and then has to show it
    // works.
    let t = Tmp::new("verified");
    let mut db = Database::open(
        t.path(),
        Policy {
            mode: Mode::Adaptive,
            priority: Priorities {
                speed: 10,
                resources: 2,
                freedom: 5,
            },
            ..Policy::conventional()
        },
    )
    .unwrap();
    db.create_collection("users", schema()).unwrap();
    for i in 0..5_000u64 {
        db.insert(
            "users",
            RecordId(i),
            Record::new()
                .with("id", i)
                .with("country", COUNTRIES[(i % 4) as usize])
                .with("age", (i % 70) as i64),
        )
        .unwrap();
    }

    for _ in 0..10 {
        drive(&mut db, 200);
        db.optimize_verified(guardrails()).unwrap();
        if db.experiment().is_some() {
            break;
        }
    }
    let e = db
        .experiment()
        .expect("the optimizer never proposed anything");
    assert_eq!(
        e.phase(),
        Phase::Proposed,
        "a change went into service before it was proved"
    );
    assert_eq!(
        db.index_specs().len(),
        0,
        "the candidate was built before the experiment began"
    );

    // The full ramp is covered elsewhere; what matters here is that the
    // optimizer's own proposal enters the loop and starts collecting evidence
    // rather than going straight into service.
    db.advance_experiment().unwrap(); // -> Building
    db.advance_experiment().unwrap(); // -> Shadow
    drive(&mut db, 20);
    let e = db.experiment().unwrap();
    assert_eq!(e.phase(), Phase::Shadow);
    assert_eq!(e.shadow.trials, 20);
    assert!(e.shadow.is_correct(), "{:?}", e.shadow.first_divergence);
}

#[test]
fn a_change_that_cannot_be_proved_is_still_applied_normally() {
    // `optimize_verified` is not a veto on everything unshadowable. A buffer
    // pool resize has no old path to compare against and never will; refusing
    // to apply it would make verification a reason to stop optimizing.
    let t = Tmp::new("unprovable");
    let mut db = seeded(t.path());
    db.optimize_verified(guardrails()).unwrap();
    db.set_level(3).unwrap();
    assert!(
        db.config().is_enabled_anywhere("plan_cache"),
        "an unshadowable change was withheld: {}",
        db.explain_optimizations()
    );
}

#[test]
fn shadow_queries_do_not_double_count_in_telemetry() {
    // A shadow answers one logical query twice. Counting both would inflate
    // every statistic the optimizer reads back, and the optimizer would then be
    // reacting to the act of measuring rather than to the workload.
    let t = Tmp::new("telemetry");
    let mut db = seeded(t.path());
    db.begin_experiment(index_decision(), guardrails()).unwrap();
    db.advance_experiment().unwrap(); // Building
    db.advance_experiment().unwrap(); // Shadow

    let before = db.telemetry().total_calls();
    drive(&mut db, 50);
    let after = db.telemetry().total_calls();
    assert_eq!(
        after - before,
        0,
        "shadow trials were counted as workload traffic"
    );
}

/// A candidate under experiment must survive the retraction reaper.
///
/// The soak run that found this is worth describing, because neither component
/// is wrong on its own.
///
/// An experiment builds its candidate where the planner cannot see it — that is
/// the whole mechanism, and without it every query would take the new path and
/// there would be nothing to compare against. The adaptive driver drops an index
/// the planner never chooses — also correct, and the reason the optimizer is not
/// a ratchet that only ever adds.
///
/// Together they annihilate. The candidate is hidden, so the planner never picks
/// it, so its use count reads zero, so the reaper drops it as dead weight. The
/// experiment then finishes, promotes, and writes a success into the decision
/// log for a structure that was deleted several hundred queries earlier. The log
/// says the database has an index. It does not.
///
/// The soak log showed exactly that sequence: `#4 enable auto_index`, `#5
/// disable auto_index — the planner never chose users.country over 1668
/// filtered queries`, `#7 enable auto_index — experiment #2 promoted`.
#[test]
fn a_candidate_under_experiment_is_not_retracted_for_going_unused() {
    let t = Tmp::new("reaper");
    let mut db = seeded(t.path());

    // Start the experiment before switching to adaptive. The other order lets
    // the driver build the index outright first, after which the experiment has
    // nothing left to build and is refused — a different failure, and not the
    // one under test.
    db.begin_experiment(index_decision(), guardrails()).unwrap();
    db.advance_experiment().unwrap(); // -> Building
    assert_eq!(db.index_specs().len(), 1, "the candidate was not built");
    db.advance_experiment().unwrap(); // -> Shadow

    // Get as far as a canary. This matters: shadow trials are deliberately not
    // counted in telemetry — one logical query answered twice would double every
    // statistic — so the reaper cannot see them at all. It is the canary, where
    // the baseline queries *are* real counted traffic and the masked candidate
    // reads zero uses against them, that the two mechanisms collide.
    drive(&mut db, 60);
    db.advance_experiment().unwrap(); // -> Canary(1)
    assert!(
        matches!(db.experiment().map(|e| e.phase()), Some(Phase::Canary(_))),
        "never reached a canary: {}",
        db.explain_experiment()
    );

    db.set_policy(Policy {
        mode: Mode::Adaptive,
        priority: Priorities {
            speed: 10,
            resources: 2,
            freedom: 5,
        },
        ..Policy::conventional()
    })
    .unwrap();
    assert!(
        db.experiment().is_some(),
        "switching to adaptive ended the experiment"
    );

    // Enough filtered traffic for the reaper to form an opinion, and enough
    // optimization cycles for it to act on one. The planner cannot choose the
    // candidate throughout, because it is masked.
    for _ in 0..12 {
        drive(&mut db, 200);
        db.optimize().unwrap();
    }

    assert_eq!(
        db.index_specs().len(),
        1,
        "the candidate was retracted mid-experiment:\n{}",
        db.explain_optimization("auto_index")
    );
    assert!(
        db.experiment().is_some(),
        "the experiment did not survive its own candidate being judged"
    );
}
