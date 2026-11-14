//! The adaptive engine against the best configuration a human could choose,
//! phase by phase.
//!
//! The project's central claim is that an expert tuning by hand adds nothing
//! over the adaptive engine. This module is the measurement that claim stands
//! or falls on.
//!
//! # The three contestants
//!
//! - **adaptive**: the engine in adaptive mode, driven through the soak script
//!   exactly as `adabt-bench soak` drives it, and measured over the last tenth
//!   of each phase — the soak's own definition of "settled". It carries
//!   whatever it built for earlier phases, as a real adaptive deployment would.
//! - **expert**: for each phase separately, the fastest of every candidate
//!   manual configuration on that phase's traffic. The candidates are every
//!   level preset asked for (0–11 by default) and a short list of hand-written
//!   configurations encoding what a knowledgeable operator would pick for that
//!   traffic. The expert is told which phase is coming; the adaptive engine is
//!   not.
//! - **level 0**: the floor, and the reference every other answer is checked
//!   against.
//!
//! # Noise
//!
//! Every cell is run `repeats` times. A cell's figure is the median of its
//! per-repeat p50s, and its spread is the range those p50s cover. Adaptive
//! *matches* the expert when the two ranges overlap, is *behind* when its
//! fastest repeat is slower than the expert's slowest, and *ahead* in the
//! mirror case. No threshold is guessed: the run's own variation is the noise.
//!
//! The expert is chosen as the minimum over many candidates, which biases it
//! towards a lucky draw. That bias favours the expert, so a "matches" verdict
//! is the conservative one to earn.

use adabt_core::error::Error;
use adabt_core::ids::RecordId;
use adabt_core::index_kind::IndexKind;
use adabt_core::policy::{Mode, Override, Policy};
use adabt_core::record::Record;
use adabt_core::store::LogicalStore;
use adabt_engine::Database;
use adabt_ir::plan::LogicalPlan;
use adabt_testkit::rng::Rng;
use std::path::Path;
use std::time::Instant;

use crate::queries::QueryMix;
use crate::soak::{self, Op, Phase, SoakConfig, SCRIPT};

/// What one comparison run measures and how often.
pub struct ExpertConfig {
    /// The adaptive half, and the size, seed and phase length both halves share.
    pub soak: SoakConfig,
    /// Times every cell is measured; the spread across them is the noise.
    pub repeats: usize,
    /// Level presets offered to the expert. Level 0 is always measured.
    pub levels: Vec<u8>,
}

/// Why a comparison stopped early.
#[derive(Debug)]
pub enum Failure {
    /// The engine returned an error.
    Engine(Error),
    /// Two configurations disagreed about an answer.
    Divergence(String),
}

impl From<Error> for Failure {
    fn from(e: Error) -> Self {
        Failure::Engine(e)
    }
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Failure::Engine(e) => write!(f, "{e}"),
            Failure::Divergence(d) => write!(f, "DIVERGENCE: {d}"),
        }
    }
}

/// One configuration's latencies on one phase, one entry per repeat.
#[derive(Clone, Default)]
pub struct Cell {
    pub label: String,
    pub p50s: Vec<u64>,
    pub p99s: Vec<u64>,
    /// What was switched on at the end of the last repeat.
    pub config: Vec<String>,
}

impl Cell {
    /// An empty cell named `label`.
    fn named(label: impl Into<String>) -> Self {
        Cell {
            label: label.into(),
            ..Default::default()
        }
    }

    /// Record one repeat.
    fn push(&mut self, p50: u64, p99: u64, config: Vec<String>) {
        self.p50s.push(p50);
        self.p99s.push(p99);
        self.config = config;
    }

    /// Median p50 across repeats.
    pub fn p50(&self) -> u64 {
        median(&self.p50s)
    }

    /// Median p99 across repeats.
    pub fn p99(&self) -> u64 {
        median(&self.p99s)
    }

    /// Fastest and slowest p50 across repeats.
    pub fn p50_range(&self) -> (u64, u64) {
        let lo = self.p50s.iter().copied().min().unwrap_or(0);
        let hi = self.p50s.iter().copied().max().unwrap_or(0);
        (lo, hi)
    }

    /// The p50 range as a fraction of the median.
    pub fn spread(&self) -> f64 {
        let (lo, hi) = self.p50_range();
        if self.p50() == 0 {
            return 0.0;
        }
        (hi - lo) as f64 / self.p50() as f64
    }
}

/// The median of `xs`, 0 when empty.
fn median(xs: &[u64]) -> u64 {
    let mut v = xs.to_vec();
    soak::percentile(&mut v, 50.0)
}

/// How adaptive compares with the expert once noise is accounted for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The p50 ranges overlap.
    Matches,
    /// Adaptive's fastest repeat is slower than the expert's slowest.
    Behind,
    /// Adaptive's slowest repeat is faster than the expert's fastest.
    Ahead,
}

impl Verdict {
    /// The verdict as one word for the table.
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Matches => "matches",
            Verdict::Behind => "behind",
            Verdict::Ahead => "ahead",
        }
    }
}

/// Every contestant's figures for one phase.
pub struct PhaseComparison {
    pub label: &'static str,
    pub mix: &'static str,
    pub adaptive: Cell,
    /// Every manual candidate, level 0 first.
    pub candidates: Vec<Cell>,
}

impl PhaseComparison {
    /// Level 0, the floor.
    pub fn floor(&self) -> &Cell {
        &self.candidates[0]
    }

    /// The fastest manual candidate by median p50.
    pub fn expert(&self) -> &Cell {
        self.candidates
            .iter()
            .min_by_key(|c| c.p50())
            .expect("level 0 is always a candidate")
    }

    /// Adaptive median p50 over expert median p50; above 1 means slower.
    pub fn ratio(&self) -> f64 {
        let expert = self.expert().p50();
        if expert == 0 {
            return 0.0;
        }
        self.adaptive.p50() as f64 / expert as f64
    }

    /// Whether adaptive is within the run's own noise of the expert.
    pub fn verdict(&self) -> Verdict {
        let (a_lo, a_hi) = self.adaptive.p50_range();
        let (e_lo, e_hi) = self.expert().p50_range();
        if a_lo > e_hi {
            Verdict::Behind
        } else if a_hi < e_lo {
            Verdict::Ahead
        } else {
            Verdict::Matches
        }
    }
}

pub struct ExpertReport {
    pub phases: Vec<PhaseComparison>,
    /// Answers from manual candidates compared with level 0.
    pub verified_queries: u64,
    pub elapsed_secs: f64,
}

/// A manual configuration offered as the expert's choice.
struct Candidate {
    label: String,
    policy: Policy,
}

/// Run the whole comparison under `dir`.
pub fn run(dir: &Path, cfg: &ExpertConfig) -> Result<ExpertReport, Failure> {
    let started = Instant::now();
    let adaptive = measure_adaptive(dir, cfg)?;
    let mut report = ExpertReport {
        phases: Vec::new(),
        verified_queries: 0,
        elapsed_secs: 0.0,
    };
    for ((index, phase), adaptive) in SCRIPT.iter().enumerate().zip(adaptive) {
        let candidates = measure_candidates(dir, cfg, index, phase, &mut report)?;
        report.phases.push(PhaseComparison {
            label: phase.label,
            mix: phase.mix.as_str(),
            adaptive,
            candidates,
        });
    }
    report.elapsed_secs = started.elapsed().as_secs_f64();
    Ok(report)
}

/// The settled latency of each phase of the soak, once per repeat.
///
/// The soak checks every answer it samples against its own level-0 reference
/// and stops on the first disagreement, which is surfaced here as a failure.
fn measure_adaptive(dir: &Path, cfg: &ExpertConfig) -> Result<Vec<Cell>, Failure> {
    let mut cells: Vec<Cell> = SCRIPT.iter().map(|_| Cell::named("adaptive")).collect();
    for repeat in 0..cfg.repeats {
        eprintln!("adaptive soak, repeat {}/{}", repeat + 1, cfg.repeats);
        let r = soak::run_soak(&dir.join("soak"), &cfg.soak)?;
        if let Some(d) = r.divergence {
            return Err(Failure::Divergence(d));
        }
        for (cell, phase) in cells.iter_mut().zip(&r.phases) {
            cell.push(
                phase.p50_last_nanos,
                phase.p99_last_nanos,
                phase.config.clone(),
            );
        }
    }
    Ok(cells)
}

/// Every candidate configuration on one phase, checked against level 0.
fn measure_candidates(
    dir: &Path,
    cfg: &ExpertConfig,
    index: usize,
    phase: &Phase,
    report: &mut ExpertReport,
) -> Result<Vec<Cell>, Failure> {
    let mut reference: Option<Vec<Answer>> = None;
    let mut cells = Vec::new();
    for candidate in candidates(phase, &cfg.levels) {
        eprintln!("{}: {}", phase.label, candidate.label);
        let mut cell = Cell::named(&candidate.label);
        for _ in 0..cfg.repeats {
            let sample = measure_manual(&dir.join("manual"), cfg, index, phase, &candidate)?;
            // The first candidate is level 0; its first repeat is the reference.
            let want = reference.get_or_insert_with(|| sample.answers.clone());
            report.verified_queries += sample.answers.len() as u64;
            check_answers(phase, &candidate.label, &sample.answers, want)?;
            cell.push(sample.p50, sample.p99, sample.config);
        }
        cells.push(cell);
    }
    Ok(cells)
}

/// A sampled query and what it returned.
#[derive(Clone)]
struct Answer {
    plan: LogicalPlan,
    rows: Vec<(RecordId, Record)>,
}

/// Fail on the first answer that differs from level 0's.
fn check_answers(
    phase: &Phase,
    label: &str,
    got: &[Answer],
    want: &[Answer],
) -> Result<(), Failure> {
    for (g, w) in got.iter().zip(want) {
        if g.rows != w.rows {
            return Err(Failure::Divergence(format!(
                "phase {} under {label}: {} rows where level 0 returned {}\n{}",
                phase.label,
                g.rows.len(),
                w.rows.len(),
                g.plan.explain()
            )));
        }
    }
    Ok(())
}

/// One repeat of one manual configuration on one phase.
struct Sample {
    p50: u64,
    p99: u64,
    config: Vec<String>,
    answers: Vec<Answer>,
}

/// Load a fresh database under `candidate`, warm it on the phase's traffic,
/// apply the configuration, and measure as many queries as the soak measures.
///
/// Warm-up comes before the configuration is applied because several
/// optimizations choose their scope from telemetry — which field to index,
/// hash or b-tree — and have nothing to go on in an empty database. The
/// traffic stream is seeded by phase alone, so every candidate and every
/// repeat answers the same queries in the same order.
fn measure_manual(
    dir: &Path,
    cfg: &ExpertConfig,
    index: usize,
    phase: &Phase,
    candidate: &Candidate,
) -> Result<Sample, Failure> {
    let _ = std::fs::remove_dir_all(dir);
    let mut db = Database::open(dir, candidate.policy.clone())?;
    soak::preload(&mut db, cfg.soak.size)?;
    let window = measured_window(cfg);
    let mut traffic = Traffic {
        phase,
        rng: Rng::new(phase_seed(cfg.soak.seed, index)),
        next_write_id: cfg.soak.size,
    };
    traffic.drive(&mut db, window / 2, None)?;
    db.optimize()?;
    traffic.drive(&mut db, window / 2, None)?;
    let mut measured = traffic.drive(&mut db, window, Some(cfg.soak.verify_every))?;
    Ok(Sample {
        p50: soak::percentile(&mut measured.latencies, 50.0),
        p99: soak::percentile(&mut measured.latencies, 99.0),
        config: soak::enabled_names(&db),
        answers: measured.answers,
    })
}

/// Operations measured per repeat: the soak's last tenth of a phase.
fn measured_window(cfg: &ExpertConfig) -> u64 {
    (cfg.soak.ops_per_phase / 10).max(1)
}

/// A seed for phase `index` that every candidate shares.
fn phase_seed(seed: u64, index: usize) -> u64 {
    seed ^ (index as u64 + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15)
}

/// One phase's traffic, replayable from its seed.
struct Traffic<'a> {
    phase: &'a Phase,
    rng: Rng,
    next_write_id: u64,
}

/// What a stretch of traffic measured.
struct Driven {
    latencies: Vec<u64>,
    answers: Vec<Answer>,
}

impl Traffic<'_> {
    /// Issue `ops` operations, timing every query and keeping every
    /// `check_every`-th query's answer when asked to.
    fn drive(
        &mut self,
        db: &mut Database,
        ops: u64,
        check_every: Option<u64>,
    ) -> Result<Driven, Failure> {
        let mut out = Driven {
            latencies: Vec::new(),
            answers: Vec::new(),
        };
        for _ in 0..ops {
            let plan = match soak::next_op(self.phase, &mut self.rng, &mut self.next_write_id) {
                Op::Write(id, rec) => {
                    db.insert("users", id, rec)?;
                    continue;
                }
                Op::Query(plan) => plan,
            };
            let t = Instant::now();
            let rows = db.query(&plan)?;
            out.latencies.push(t.elapsed().as_nanos() as u64);
            let queries = out.latencies.len() as u64;
            if check_every.is_some_and(|n| queries.is_multiple_of(n.max(1))) {
                out.answers.push(Answer { plan, rows });
            }
        }
        Ok(out)
    }
}

/// Level 0, then every other requested level, then the hand-written picks.
fn candidates(phase: &Phase, levels: &[u8]) -> Vec<Candidate> {
    let mut out = vec![level_candidate(0)];
    out.extend(
        levels
            .iter()
            .filter(|&&l| l != 0)
            .map(|&l| level_candidate(l)),
    );
    out.extend(
        hand_written(phase.mix)
            .into_iter()
            .map(|(label, overrides)| Candidate {
                label: format!("hand: {label}"),
                policy: Policy {
                    mode: Mode::Manual {
                        level: 0,
                        overrides,
                    },
                    ..Policy::conventional()
                },
            }),
    );
    out
}

/// The preset for `level`, unmodified.
fn level_candidate(level: u8) -> Candidate {
    Candidate {
        label: format!("level {level}"),
        policy: Policy::manual(level),
    }
}

/// What an operator who knew the traffic would switch on, laid over level 0.
///
/// Each entry is a defensible choice for the phase, not a search: the point is
/// to give the expert the configurations a knowledgeable human would actually
/// try, including ones no level preset names.
fn hand_written(mix: QueryMix) -> Vec<(&'static str, Vec<Override>)> {
    let caches = || {
        vec![
            Override::toggle("plan_cache", true),
            Override::toggle("result_cache", true),
        ]
    };
    let index = |field: &str, kind: IndexKind| {
        Override::scoped("auto_index", format!("users.{field}"), true)
            .with_param("kind", kind.as_ordinal())
    };
    let with = |extra: Vec<Override>| {
        let mut v = caches();
        v.extend(extra);
        v
    };
    match mix {
        QueryMix::ByIdentity => vec![
            (
                "direct lookup",
                vec![
                    Override::toggle("plan_cache", true),
                    Override::scoped("direct_lookup", "users", true),
                ],
            ),
            (
                "direct lookup only",
                vec![Override::scoped("direct_lookup", "users", true)],
            ),
        ],
        QueryMix::PointFilter | QueryMix::RepeatedQuery => vec![
            (
                "caches + hash index on country",
                with(vec![index("country", IndexKind::Hash)]),
            ),
            (
                "caches + bitmap index on country",
                with(vec![index("country", IndexKind::Bitmap)]),
            ),
        ],
        QueryMix::RangeFilter => vec![
            (
                "caches + b-tree on age",
                with(vec![index("age", IndexKind::BTree)]),
            ),
            (
                "caches + b-tree on age + clustered by age",
                with(vec![
                    index("age", IndexKind::BTree),
                    Override::scoped("clustered_sort", "users.age", true),
                ]),
            ),
        ],
        QueryMix::Aggregate => vec![
            (
                "caches + column store + views",
                with(vec![
                    Override::scoped("column_store", "users", true),
                    Override::toggle("materialized_view", true),
                ]),
            ),
            (
                "caches + column store + views + b-tree on age",
                with(vec![
                    Override::scoped("column_store", "users", true),
                    Override::toggle("materialized_view", true),
                    index("age", IndexKind::BTree),
                ]),
            ),
        ],
    }
}

/// The summary table, one row per phase.
pub fn format_summary(r: &ExpertReport) -> String {
    let mut s = format!(
        "{:<15} {:>11} {:>11} {:>23} {:>11} {:>23} {:>7} {:>8}  {}\n",
        "phase",
        "level0 p50",
        "expert p50",
        "expert range",
        "adapt p50",
        "adaptive range",
        "ratio",
        "verdict",
        "expert config"
    );
    s.push_str(&"-".repeat(140));
    s.push('\n');
    for p in &r.phases {
        let e = p.expert();
        s.push_str(&format!(
            "{:<15} {:>11} {:>11} {:>23} {:>11} {:>23} {:>6.2}x {:>8}  {}\n",
            p.label,
            nanos(p.floor().p50()),
            nanos(e.p50()),
            range(e),
            nanos(p.adaptive.p50()),
            range(&p.adaptive),
            p.ratio(),
            p.verdict().as_str(),
            e.label,
        ));
    }
    s.push_str(&format!(
        "\np99 (median of repeats): {}\n",
        r.phases
            .iter()
            .map(|p| format!(
                "{} expert {} / adaptive {}",
                p.label,
                nanos(p.expert().p99()),
                nanos(p.adaptive.p99())
            ))
            .collect::<Vec<_>>()
            .join("; ")
    ));
    s
}

/// Every candidate's figures and what each side had switched on.
pub fn format_detail(r: &ExpertReport) -> String {
    let mut s = String::new();
    for p in &r.phases {
        s.push_str(&format!("\n== {} ({}) ==\n", p.label, p.mix));
        s.push_str(&format!(
            "{:<48} {:>11} {:>11} {:>8}\n",
            "configuration", "p50", "p99", "spread"
        ));
        let mut rows: Vec<&Cell> = p.candidates.iter().collect();
        rows.push(&p.adaptive);
        rows.sort_by_key(|c| c.p50());
        for c in rows {
            s.push_str(&format!(
                "{:<48} {:>11} {:>11} {:>7.0}%\n",
                c.label,
                nanos(c.p50()),
                nanos(c.p99()),
                c.spread() * 100.0
            ));
        }
        s.push_str(&format!(
            "expert   enabled: {}\n",
            listed(&p.expert().config)
        ));
        s.push_str(&format!(
            "adaptive enabled: {}\n",
            listed(&p.adaptive.config)
        ));
    }
    s.push_str(&format!(
        "\n{} manual answers checked against level 0; adaptive answers checked by the soak; {:.1}s\n",
        r.verified_queries, r.elapsed_secs
    ));
    s
}

/// A latency in the most readable unit.
fn nanos(n: u64) -> String {
    match n {
        0..=9_999 => format!("{n}ns"),
        10_000..=9_999_999 => format!("{:.1}us", n as f64 / 1e3),
        _ => format!("{:.2}ms", n as f64 / 1e6),
    }
}

/// A cell's p50 range across repeats.
fn range(c: &Cell) -> String {
    let (lo, hi) = c.p50_range();
    format!("{}–{}", nanos(lo), nanos(hi))
}

/// Enabled optimizations, or a word saying there are none.
fn listed(config: &[String]) -> String {
    if config.is_empty() {
        "nothing".to_string()
    } else {
        config.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(p50s: &[u64]) -> Cell {
        Cell {
            label: "x".into(),
            p50s: p50s.to_vec(),
            p99s: p50s.to_vec(),
            config: Vec::new(),
        }
    }

    fn comparison(adaptive: &[u64], expert: &[u64]) -> PhaseComparison {
        PhaseComparison {
            label: "p",
            mix: "m",
            adaptive: cell(adaptive),
            candidates: vec![cell(&[1_000_000]), cell(expert)],
        }
    }

    #[test]
    fn overlapping_ranges_match() {
        assert_eq!(
            comparison(&[100, 120, 140], &[90, 110, 130]).verdict(),
            Verdict::Matches
        );
    }

    #[test]
    fn disjoint_ranges_decide_the_verdict() {
        assert_eq!(
            comparison(&[200, 210, 220], &[90, 100, 110]).verdict(),
            Verdict::Behind
        );
        assert_eq!(
            comparison(&[50, 55, 60], &[90, 100, 110]).verdict(),
            Verdict::Ahead
        );
    }

    #[test]
    fn the_expert_is_the_fastest_candidate_by_median() {
        let c = comparison(&[100], &[90, 100, 110]);
        assert_eq!(c.expert().p50(), 100);
        assert!((c.ratio() - 1.0).abs() < 1e-9);
    }

    #[test]
    fn every_hand_written_override_names_a_registered_optimization() {
        let mut registry = adabt_opt::Registry::new();
        adabt_engine::optimizations::register_builtins(&mut registry);
        for phase in SCRIPT.iter() {
            for c in candidates(phase, &[0, 11]) {
                if let Mode::Manual { overrides, .. } = &c.policy.mode {
                    registry.validate_overrides(overrides).unwrap();
                }
            }
        }
    }
}
