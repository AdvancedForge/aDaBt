//! End-to-end online experiments: shadow, ramp, promote or revert.

use adabt_core::ids::RecordId;
use adabt_core::index_kind::IndexKind;
use adabt_core::policy::Policy;
use adabt_core::record::Record;
use adabt_core::schema::{FieldDef, FieldType, Schema, SchemaMode};
use adabt_core::store::LogicalStore;
use adabt_engine::shadow::{trial, ShadowPair, ShadowReport};
use adabt_engine::Database;
use adabt_ir::plan::{LogicalOp, LogicalPlan};
use adabt_ir::{CmpOp, Expr};
use adabt_opt::decision::{Decision, DecisionAction};
use adabt_opt::experiment::{Assessment, Experiment, Guardrails, Measurement, Phase};
use std::path::{Path, PathBuf};

struct Tmp(PathBuf);
impl Tmp {
    fn new(tag: &str) -> Self {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "adabt-exp-{tag}-{}-{:?}",
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

const COUNTRIES: [&str; 4] = ["NO", "SE", "DK", "FI"];

fn seeded(dir: &Path, n: u64) -> Database {
    let mut db = Database::open(dir, Policy::manual(0)).unwrap();
    db.create_collection("users", schema()).unwrap();
    for i in 0..n {
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
    db
}

/// Two databases over the same data: one with the candidate index, one without.
///
/// A real deployment would run both representations inside one engine; two
/// engines over identical data isolates the comparison from everything else and
/// is enough to exercise the machinery honestly.
struct IndexCandidate {
    baseline: Database,
    candidate: Database,
}

impl ShadowPair for IndexCandidate {
    fn baseline(
        &mut self,
        plan: &LogicalPlan,
    ) -> adabt_core::error::Result<Vec<(RecordId, Record)>> {
        self.baseline.query(plan)
    }
    fn candidate(
        &mut self,
        plan: &LogicalPlan,
    ) -> adabt_core::error::Result<Vec<(RecordId, Record)>> {
        self.candidate.query(plan)
    }
}

fn candidate_pair(a: &Tmp, b: &Tmp, n: u64, field: &str, kind: IndexKind) -> IndexCandidate {
    let baseline = seeded(a.path(), n);
    let mut candidate = seeded(b.path(), n);
    candidate.create_index("users", field, kind).unwrap();
    IndexCandidate {
        baseline,
        candidate,
    }
}

fn equality_query(c: &str) -> LogicalPlan {
    LogicalPlan::new(LogicalOp::scan("users").filter(Expr::eq("country", c)))
}

#[test]
fn shadow_execution_confirms_a_real_index_agrees_and_is_faster() {
    let (a, b) = (Tmp::new("agree-a"), Tmp::new("agree-b"));
    let mut pair = candidate_pair(&a, &b, 4_000, "country", IndexKind::Hash);
    let mut report = ShadowReport::default();

    for i in 0..80 {
        let q = equality_query(COUNTRIES[i % 4]);
        trial(&mut pair, &q, &mut report).unwrap();
    }

    assert!(
        report.is_correct(),
        "the index disagreed with the baseline: {:?}",
        report.first_divergence
    );
    assert!(
        report.p50_ratio() < 1.0,
        "the index was not faster under shadow: {}",
        report.describe()
    );
    assert!(
        report.improvement_is_credible(50, 0.2),
        "{}",
        report.describe()
    );
}

#[test]
fn shadow_execution_shows_a_useless_index_is_no_faster() {
    // A hash index cannot serve a range, so the candidate is correct but
    // pointless — exactly the case that cost the M7 matrix memory for nothing.
    let (a, b) = (Tmp::new("useless-a"), Tmp::new("useless-b"));
    let mut pair = candidate_pair(&a, &b, 4_000, "age", IndexKind::Hash);
    let mut report = ShadowReport::default();

    for i in 0..60 {
        let q = LogicalPlan::new(LogicalOp::scan("users").filter(Expr::cmp(
            "age",
            CmpOp::Gt,
            (i % 60) as i64,
        )));
        trial(&mut pair, &q, &mut report).unwrap();
    }

    assert!(
        report.is_correct(),
        "a useless index should still be correct"
    );
    assert!(
        !report.improvement_is_credible(20, 0.2),
        "an index the planner cannot use appeared to help: {}",
        report.describe()
    );
}

/// A deliberately broken candidate: drops rows the baseline returns.
struct BrokenCandidate {
    inner: IndexCandidate,
}

impl ShadowPair for BrokenCandidate {
    fn baseline(
        &mut self,
        plan: &LogicalPlan,
    ) -> adabt_core::error::Result<Vec<(RecordId, Record)>> {
        self.inner.baseline(plan)
    }
    fn candidate(
        &mut self,
        plan: &LogicalPlan,
    ) -> adabt_core::error::Result<Vec<(RecordId, Record)>> {
        let mut rows = self.inner.candidate(plan)?;
        rows.pop();
        Ok(rows)
    }
}

#[test]
fn a_broken_candidate_is_caught_and_aborts_the_experiment() {
    let (a, b) = (Tmp::new("broken-a"), Tmp::new("broken-b"));
    let mut pair = BrokenCandidate {
        inner: candidate_pair(&a, &b, 2_000, "country", IndexKind::Hash),
    };
    let mut report = ShadowReport::default();
    for i in 0..10 {
        trial(&mut pair, &equality_query(COUNTRIES[i % 4]), &mut report).unwrap();
    }
    assert!(!report.is_correct());

    // And the experiment machinery treats that as fatal on the first instance.
    let mut e = Experiment::new(
        1,
        Decision::new(
            "auto_index",
            "users.country",
            DecisionAction::Enable,
            "test",
        ),
        Guardrails::default(),
    );
    e.baseline = Measurement {
        samples: 10_000,
        p50_nanos: 1_000,
        p99_nanos: 5_000,
        errors: 0,
        ram_bytes: 0,
    };
    e.candidate = e.baseline;
    for _ in 0..report.divergences {
        e.record_divergence();
    }
    match e.assess() {
        Assessment::Abort(why) => assert!(why.contains("divergence"), "{why}"),
        other => panic!("a divergent candidate was not aborted: {other:?}"),
    }
    e.advance();
    assert_eq!(e.phase, Phase::Reverted);
}

#[test]
fn a_confirmed_candidate_ramps_through_shadow_to_promotion() {
    let (a, b) = (Tmp::new("ramp-a"), Tmp::new("ramp-b"));
    let mut pair = candidate_pair(&a, &b, 4_000, "country", IndexKind::Hash);
    let mut report = ShadowReport::default();
    for i in 0..80 {
        trial(&mut pair, &equality_query(COUNTRIES[i % 4]), &mut report).unwrap();
    }
    assert!(report.is_correct());

    // Feed the measured evidence into the lifecycle.
    let mut e = Experiment::new(
        2,
        Decision::new(
            "auto_index",
            "users.country",
            DecisionAction::Enable,
            "measured",
        ),
        Guardrails {
            min_samples: 50,
            ..Guardrails::default()
        },
    );
    e.baseline = Measurement {
        samples: report.trials,
        p50_nanos: report.baseline_p50(),
        p99_nanos: report.baseline_p99(),
        errors: 0,
        ram_bytes: 0,
    };
    e.candidate = Measurement {
        samples: report.trials,
        p50_nanos: report.candidate_p50(),
        p99_nanos: report.candidate_p99(),
        errors: 0,
        ram_bytes: 0,
    };
    assert_eq!(e.assess(), Assessment::Healthy, "{}", report.describe());

    for _ in 0..12 {
        e.advance();
        if e.phase.is_terminal() {
            break;
        }
    }
    assert_eq!(e.phase, Phase::Promoted, "path: {:?}", e.history);

    // Shadow must have come before any traffic moved.
    let shadow_at = e.history.iter().position(|p| *p == Phase::Shadow).unwrap();
    let first_canary = e
        .history
        .iter()
        .position(|p| matches!(p, Phase::Canary(_)))
        .unwrap();
    assert!(shadow_at < first_canary);
}

#[test]
fn shadow_reads_are_taken_against_a_stable_snapshot() {
    // The property M10 existed to provide: a scan under a snapshot returns the
    // same rows however much the data churns, so a divergence means the
    // candidate is wrong rather than that the data moved.
    let t = Tmp::new("stable");
    let mut db = seeded(t.path(), 500);
    let snap = db.snapshot();
    let before = db.scan_at("users", &snap).unwrap();

    for i in 0..500u64 {
        db.update(
            "users",
            RecordId(i),
            Record::new()
                .with("id", i)
                .with("country", "ZZ")
                .with("age", 1i64),
        )
        .unwrap();
    }

    let after = db.scan_at("users", &snap).unwrap();
    assert_eq!(
        before, after,
        "the snapshot moved, so any shadow comparison over it would be meaningless"
    );
    assert_ne!(
        db.scan("users").unwrap(),
        before,
        "the underlying data did not actually change, so this proves nothing"
    );
}
