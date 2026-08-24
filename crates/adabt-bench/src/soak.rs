//! A long adaptive run against a workload that changes underneath it.
//!
//! Every other harness in this crate sets a level and measures. This one sets
//! nothing: the database is in adaptive mode, the workload shifts from phase to
//! phase, and what is being measured is whether the database notices.
//!
//! # Why it runs two databases
//!
//! Alongside the adaptive engine it runs a second one pinned at level 0 — no
//! caches, no indexes, no column store, no views — and puts the *same* queries
//! through both. Any difference in results is a divergence, and the run stops.
//!
//! That is the differential idea from `adabt-testkit` applied over time rather
//! than over an operation sequence. The unit tests check that optimization does
//! not change answers at a moment; this checks it across thousands of queries
//! while the physical layer is being rebuilt underneath, which is the situation
//! the unit tests cannot construct.
//!
//! # What the numbers mean
//!
//! Each phase reports its own latency at the start and at the end. The number
//! that matters is the change between them: it is the database adapting to a
//! workload it had not seen when the phase began. A phase that improves has
//! specialised for its traffic; a phase that does not is telling you the
//! optimizer had nothing to offer it, which is also worth knowing.
//!
//! The final phase deliberately returns to the first phase's traffic. By then
//! the database is carrying structures built for three workloads it is no longer
//! running, and what happens to them — whether anything is ever *retracted* — is
//! the part of the design that is easiest to get wrong and hardest to see.

use adabt_core::ids::RecordId;
use adabt_core::policy::{Mode, Policy, Priorities};
use adabt_core::record::Record;
use adabt_core::store::LogicalStore;
use adabt_engine::Database;
use adabt_ir::plan::LogicalPlan;
use adabt_opt::experiment::Guardrails;
use adabt_testkit::rng::Rng;
use std::path::Path;
use std::time::Instant;

use crate::queries::QueryMix;

/// One stretch of the run, with one kind of traffic.
pub struct Phase {
    pub label: &'static str,
    pub mix: QueryMix,
    pub writes_per_thousand: u32,
}

/// The script. Four workloads, then back to the first.
pub const SCRIPT: [Phase; 5] = [
    Phase {
        label: "identity",
        mix: QueryMix::ByIdentity,
        writes_per_thousand: 0,
    },
    Phase {
        label: "point-filter",
        mix: QueryMix::PointFilter,
        writes_per_thousand: 20,
    },
    Phase {
        label: "range-filter",
        mix: QueryMix::RangeFilter,
        writes_per_thousand: 20,
    },
    Phase {
        label: "aggregate",
        mix: QueryMix::Aggregate,
        writes_per_thousand: 5,
    },
    Phase {
        label: "identity-again",
        mix: QueryMix::ByIdentity,
        writes_per_thousand: 0,
    },
];

pub struct SoakConfig {
    pub size: u64,
    pub ops_per_phase: u64,
    pub seed: u64,
    pub verify: bool,
    /// One query in this many is checked against the reference during quiet
    /// stretches. Every query would make the level-0 reference the bottleneck
    /// and turn a soak into a slow differential test — and the differential
    /// tests already exist, run faster, and shrink their failures.
    pub verify_every: u64,
    /// Queries between optimization cycles.
    pub cycle_every: u64,
    pub priorities: Priorities,
}

/// Queries verified without sampling after the configuration changes.
///
/// Divergence is not uniformly likely across a run. It is likely in the moments
/// just after a structure is built, promoted or dropped, and vanishingly likely
/// during a stretch where nothing has moved. So verification follows the
/// changes rather than a clock: every query for a burst after the config moves,
/// sampled the rest of the time.
const VERIFY_BURST: u64 = 300;

impl Default for SoakConfig {
    fn default() -> Self {
        Self {
            size: 20_000,
            ops_per_phase: 20_000,
            seed: 0x50AC,
            verify: true,
            verify_every: 64,
            cycle_every: 500,
            priorities: Priorities {
                speed: 9,
                resources: 3,
                freedom: 5,
            },
        }
    }
}

#[derive(Default)]
pub struct PhaseReport {
    pub label: &'static str,
    pub mix: &'static str,
    /// Median over the first tenth of the phase, before it can have adapted.
    pub p50_first_nanos: u64,
    /// Median over the last tenth.
    pub p50_last_nanos: u64,
    pub p99_last_nanos: u64,
    /// Decisions the controller accepted during this phase.
    pub decisions: usize,
    pub experiments_started: usize,
    pub experiments_promoted: usize,
    pub experiments_reverted: usize,
    /// What was switched on when the phase ended.
    pub config: Vec<String>,
}

impl PhaseReport {
    pub fn improvement(&self) -> f64 {
        if self.p50_first_nanos == 0 {
            return 0.0;
        }
        1.0 - self.p50_last_nanos as f64 / self.p50_first_nanos as f64
    }
}

pub struct SoakReport {
    pub phases: Vec<PhaseReport>,
    pub decision_log: String,
    pub experiments: Vec<String>,
    pub divergence: Option<String>,
    pub total_queries: u64,
    pub verified_queries: u64,
    pub elapsed_secs: f64,
}

fn percentile(samples: &mut [u64], p: f64) -> u64 {
    if samples.is_empty() {
        return 0;
    }
    samples.sort_unstable();
    let i = ((p / 100.0) * (samples.len() - 1) as f64).round() as usize;
    samples[i.min(samples.len() - 1)]
}

fn enabled_names(db: &Database) -> Vec<String> {
    let mut v: Vec<String> = db
        .config()
        .entries()
        .map(|(n, scope, _)| {
            if scope == "global" {
                n.to_string()
            } else {
                format!("{n}[{scope}]")
            }
        })
        .collect();
    v.sort();
    v.dedup();
    v
}

/// Load both databases with the same rows.
fn preload(db: &mut Database, size: u64) -> adabt_core::error::Result<()> {
    db.create_collection("users", crate::queries::schema())?;
    for i in 0..size {
        db.insert("users", RecordId(i), crate::queries::record(i))?;
    }
    Ok(())
}

pub fn run_soak(dir: &Path, cfg: &SoakConfig) -> adabt_core::error::Result<SoakReport> {
    let adaptive_dir = dir.join("adaptive");
    let reference_dir = dir.join("reference");
    let _ = std::fs::remove_dir_all(&adaptive_dir);
    let _ = std::fs::remove_dir_all(&reference_dir);

    let mut db = Database::open(
        &adaptive_dir,
        Policy {
            mode: Mode::Adaptive,
            priority: cfg.priorities,
            ..Policy::conventional()
        },
    )?;
    preload(&mut db, cfg.size)?;

    // The control. Level 0 is the plainest thing this engine can be: a heap, a
    // buffer pool, and a scan for everything.
    let mut reference = if cfg.verify {
        let mut r = Database::open(&reference_dir, Policy::manual(0))?;
        preload(&mut r, cfg.size)?;
        Some(r)
    } else {
        None
    };

    let guardrails = Guardrails {
        // A soak has to reach verdicts inside a run rather than inside a day.
        min_samples: 200,
        ..Guardrails::default()
    };
    let mut rng = Rng::new(cfg.seed);
    let mut report = SoakReport {
        phases: Vec::new(),
        decision_log: String::new(),
        experiments: Vec::new(),
        divergence: None,
        total_queries: 0,
        verified_queries: 0,
        elapsed_secs: 0.0,
    };
    let started = Instant::now();
    let window = (cfg.ops_per_phase / 10).max(1);
    let mut next_write_id = cfg.size;
    let mut verified = 0u64;
    let mut last_config_change = db.decision_count();
    let mut burst_left = VERIFY_BURST;

    'phases: for phase in SCRIPT.iter() {
        let decisions_before = db.decision_count();
        let promoted_before = db.promoted_count();
        let reverted_before = db.reverted_count();
        let started_before = db.experiments_started();
        let mut first: Vec<u64> = Vec::new();
        let mut last: Vec<u64> = Vec::new();

        for op in 0..cfg.ops_per_phase {
            // A trickle of writes, so nothing can be answered purely from a
            // cache and every derived structure has to be maintained.
            if phase.writes_per_thousand > 0 && rng.below(1000) < phase.writes_per_thousand as u64 {
                let id = RecordId(next_write_id);
                next_write_id += 1;
                let rec: Record = crate::queries::record(id.0);
                db.insert("users", id, rec.clone())?;
                if let Some(r) = reference.as_mut() {
                    r.insert("users", id, rec)?;
                }
                continue;
            }

            let plan: LogicalPlan = phase.mix.next_plan(&mut rng, next_write_id);
            let t = Instant::now();
            let got = db.query(&plan)?;
            let nanos = t.elapsed().as_nanos() as u64;
            report.total_queries += 1;

            let check = burst_left > 0 || op % cfg.verify_every == 0;
            burst_left = burst_left.saturating_sub(1);
            if let (true, Some(r)) = (check, reference.as_mut()) {
                verified += 1;
                let want = r.query(&plan)?;
                if got != want {
                    report.divergence = Some(format!(
                        "phase {} op {op}: the adaptive engine returned {} rows and the \
                         level-0 reference {}\n{}\nconfig: {}",
                        phase.label,
                        got.len(),
                        want.len(),
                        plan.explain(),
                        enabled_names(&db).join(", ")
                    ));
                    break 'phases;
                }
            }

            if op < window {
                first.push(nanos);
            } else if op >= cfg.ops_per_phase - window {
                last.push(nanos);
            }

            if op % cfg.cycle_every == 0 {
                db.optimize_verified(guardrails)?;
            }
            // Anything the controller accepted reopens the verification burst.
            if db.decision_count() != last_config_change {
                last_config_change = db.decision_count();
                burst_left = VERIFY_BURST;
            }
            // Advance more often than a cycle: an experiment in shadow needs to
            // be given the chance to move as soon as it has the samples, or the
            // ramp cannot finish inside a phase.
            if op % (cfg.cycle_every / 5).max(1) == 0 {
                db.advance_experiment()?;
            }
        }

        report.phases.push(PhaseReport {
            label: phase.label,
            mix: phase.mix.as_str(),
            p50_first_nanos: percentile(&mut first, 50.0),
            p50_last_nanos: percentile(&mut last, 50.0),
            p99_last_nanos: percentile(&mut last, 99.0),
            decisions: db.decision_count().saturating_sub(decisions_before),
            experiments_started: db.experiments_started().saturating_sub(started_before),
            experiments_promoted: db.promoted_count().saturating_sub(promoted_before),
            experiments_reverted: db.reverted_count().saturating_sub(reverted_before),
            config: enabled_names(&db),
        });
    }

    report.verified_queries = verified;
    report.elapsed_secs = started.elapsed().as_secs_f64();
    report.decision_log = db.explain_optimizations();
    report.experiments = db
        .finished_experiments()
        .iter()
        .map(|e| e.explain())
        .collect();
    if let Some(live) = db.experiment() {
        report.experiments.push(live.explain());
    }
    Ok(report)
}

pub fn format_soak(r: &SoakReport) -> String {
    let mut s = String::new();
    s.push_str(&format!(
        "{:<16} {:<14} {:>11} {:>11} {:>8} {:>10} {:>6} {:>4} {:>4} {:>4}\n",
        "phase",
        "traffic",
        "p50 first",
        "p50 last",
        "change",
        "p99 last",
        "decis",
        "exp",
        "prm",
        "rev"
    ));
    s.push_str(&"-".repeat(96));
    s.push('\n');
    for p in &r.phases {
        s.push_str(&format!(
            "{:<16} {:<14} {:>9}ns {:>9}ns {:>7.0}% {:>8}ns {:>6} {:>4} {:>4} {:>4}\n",
            p.label,
            p.mix,
            p.p50_first_nanos,
            p.p50_last_nanos,
            p.improvement() * 100.0,
            p.p99_last_nanos,
            p.decisions,
            p.experiments_started,
            p.experiments_promoted,
            p.experiments_reverted,
        ));
    }
    s.push('\n');
    for p in &r.phases {
        s.push_str(&format!(
            "after {:<16} {}\n",
            p.label,
            if p.config.is_empty() {
                "nothing enabled".to_string()
            } else {
                p.config.join(", ")
            }
        ));
    }
    s.push_str(&format!(
        "\n{} queries in {:.1}s, {} checked against the level-0 reference\n",
        r.total_queries, r.elapsed_secs, r.verified_queries
    ));
    match &r.divergence {
        Some(d) => {
            s.push_str("\nDIVERGENCE — the adaptive engine and the level-0 reference disagreed\n");
            s.push_str(d);
            s.push('\n');
        }
        None => s.push_str("no divergence among the queries checked\n"),
    }
    s
}
