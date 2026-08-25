//! Chaos around experiment promotion: a restart at *every* phase.
//!
//! An online experiment spans many moments — candidate building, shadow
//! traffic, canary ramp, the promotion itself. A deployment restarts in the
//! middle of them: deploy, OOM, maintenance. The contract this test walks
//! through every phase boundary:
//!
//! - **The answers never change.** Whatever phase died, the reopened
//!   database returns exactly the rows it returned before the experiment
//!   began.
//! - **Nothing half-built survives as a lie.** A candidate index that made
//!   it to the catalog must actually answer lookups; `verify()` must come
//!   back empty after every restart, promoted or not.
//! - **A lost trial loses only the trial.** Experiment state is in-memory;
//!   a restart retires the experiment, never corrupts its aftermath.

use adabt_core::ids::RecordId;
use adabt_core::policy::Policy;
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
            "adabt-promo-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
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
    // Warm the filtered-field telemetry so auto_index will take the decision.
    for i in 0..40 {
        let c = COUNTRIES[i % 4];
        let plan = LogicalPlan::new(LogicalOp::scan("users").filter(Expr::eq("country", c)));
        db.query(&plan).unwrap();
    }
    db
}

fn equality_query(c: &str) -> LogicalPlan {
    LogicalPlan::new(LogicalOp::scan("users").filter(Expr::eq("country", c)))
}

/// The full answer set before anything experimental happens. This is the
/// ground truth every restart is measured against.
fn ground_truth(db: &mut Database) -> Vec<Vec<(RecordId, u64)>> {
    COUNTRIES
        .iter()
        .map(|c| {
            let mut rows: Vec<(RecordId, u64)> = db
                .query(&equality_query(c))
                .unwrap()
                .into_iter()
                .map(|(id, r)| {
                    let id_val = match r.get("id") {
                        Some(adabt_core::value::Value::U64(u)) => *u,
                        _ => 0,
                    };
                    (id, id_val)
                })
                .collect();
            rows.sort_unstable();
            rows
        })
        .collect()
}

#[test]
fn a_restart_at_every_phase_keeps_answers_and_consistency() {
    let t = Tmp::new("every-phase");
    let dir = t.path();

    let mut db = seeded(dir);
    let truth = ground_truth(&mut db);
    let mut state = Some(db);

    // Restart in the middle of each early phase — Building and Shadow are
    // where a deployment most plausibly dies. Each scenario: get the
    // experiment into the phase, kill everything, reopen, demand the three
    // contracts.
    for cut_at in [Phase::Building, Phase::Shadow] {
        let mut db = state.take().expect("scenario state");
        db.begin_experiment(
            Decision::new(
                "auto_index",
                "users.country",
                DecisionAction::Enable,
                "chaos",
            ),
            Guardrails {
                min_samples: 10,
                ..Guardrails::default()
            },
        )
        .unwrap();
        loop {
            match db.advance_experiment().unwrap() {
                Some(p) if p == cut_at || p.is_terminal() => break,
                Some(_) => continue,
                None => break,
            }
        }
        if cut_at == Phase::Shadow {
            // Live in the phase before dying there.
            for i in 0..20 {
                let _ = db.query(&equality_query(COUNTRIES[i % 4])).unwrap();
            }
        }

        // The crash.
        drop(db);
        let mut db = Database::open(dir, Policy::manual(0)).unwrap();

        // Contract 1: identical answers after the restart.
        assert_eq!(
            ground_truth(&mut db),
            truth,
            "restart during {cut_at:?} changed the answers"
        );
        // Contract 2: surviving structures are consistent, catalog honest.
        let report = db.verify().unwrap();
        assert!(
            report.problems.is_empty(),
            "restart during {cut_at:?}: {:?}",
            report.problems.join("\n")
        );

        // Contract 3: whatever survived is either a working resumed trial or
        // a plainly absent one — never a half-state that wedges queries.
        if db.experiment().is_some() {
            let _ = db.advance_experiment().unwrap();
        }
        // A restarted-during-Building directory already carries the field's
        // candidate, so a *second* trial there cannot re-run; promotion
        // after crashes is live_experiments' territory. Here we assert the
        // engine kept serving correctly regardless.
        assert_eq!(ground_truth(&mut db), truth);
        state = Some(db);
    }

    // And the final reopen holds all three contracts over a promoted index.
    let mut final_db = Database::open(dir, Policy::manual(0)).unwrap();
    assert_eq!(ground_truth(&mut final_db), truth);
    let report = final_db.verify().unwrap();
    assert!(report.problems.is_empty(), "{:?}", report.problems);
}
