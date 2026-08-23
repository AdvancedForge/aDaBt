//! The measurement harness.
//!
//! Produces the level x workload matrix that is simultaneously the project's
//! proof of life, its performance regression suite, and the seed data the
//! Phase 6 cost model will be calibrated from. Because of that third use, the
//! report records resource consumption alongside latency: a cost model trained
//! only on speed cannot represent the resource axis at all.

use crate::resources::ResourceSample;
use crate::workload::{workload_schema, Workload, WorkloadKind, COLLECTION};
use adabt_core::store::LogicalStore;
use adabt_telemetry::histogram::Histogram;
use adabt_testkit::ops::{apply, Op, OpOutcome};
use std::collections::BTreeMap;
use std::time::Instant;

#[derive(Debug, Clone)]
pub struct RunReport {
    pub workload: WorkloadKind,
    pub engine: String,
    pub level: u8,
    pub ops: u64,
    pub failures: u64,
    pub wall_secs: f64,
    pub latency: Histogram,
    pub per_op: BTreeMap<&'static str, Histogram>,
    pub resources: ResourceSample,
}

impl RunReport {
    pub fn throughput(&self) -> f64 {
        if self.wall_secs <= 0.0 {
            0.0
        } else {
            self.ops as f64 / self.wall_secs
        }
    }

    /// CPU-seconds per million operations. The headline efficiency number:
    /// unlike raw CPU time it is comparable across runs of different lengths.
    pub fn cpu_per_mop(&self) -> f64 {
        if self.ops == 0 {
            0.0
        } else {
            self.resources.cpu_secs / (self.ops as f64 / 1e6)
        }
    }

    pub fn to_json(&self) -> String {
        let l = &self.latency;
        format!(
            concat!(
                "{{\"workload\":\"{w}\",\"engine\":\"{e}\",\"level\":{lv},",
                "\"ops\":{ops},\"failures\":{fail},\"wall_secs\":{wall:.6},",
                "\"throughput_ops_per_sec\":{tput:.2},",
                "\"latency_ns\":{{\"p50\":{p50},\"p95\":{p95},\"p99\":{p99},",
                "\"p999\":{p999},\"max\":{max},\"mean\":{mean:.1}}},",
                "\"resources\":{{\"peak_rss_bytes\":{rss},\"cpu_secs\":{cpu:.4},",
                "\"cpu_per_mop\":{cpm:.4},\"disk_write_bytes\":{dw},",
                "\"disk_read_bytes\":{dr}}}}}"
            ),
            w = self.workload.as_str(),
            e = self.engine,
            lv = self.level,
            ops = self.ops,
            fail = self.failures,
            wall = self.wall_secs,
            tput = self.throughput(),
            p50 = l.percentile(50.0),
            p95 = l.percentile(95.0),
            p99 = l.percentile(99.0),
            p999 = l.percentile(99.9),
            max = l.max(),
            mean = l.mean(),
            rss = self.resources.peak_rss_bytes,
            cpu = self.resources.cpu_secs,
            cpm = self.cpu_per_mop(),
            dw = self.resources.disk_write_bytes,
            dr = self.resources.disk_read_bytes,
        )
    }
}

pub struct RunConfig {
    pub workload: WorkloadKind,
    pub dataset_size: u64,
    pub ops: u64,
    pub seed: u64,
    pub engine: String,
    pub level: u8,
    /// Wall-clock ceiling on the measured phase. Reached first, the run stops
    /// early and reports the ops it actually completed.
    ///
    /// An op-count budget alone is the wrong bound for a matrix: a full scan
    /// costs four orders of magnitude more than a point get, so a fixed count
    /// that is brisk for one workload runs for hours on another.
    pub max_secs: Option<f64>,
    /// Operations executed before measurement starts, to reach steady state.
    pub warmup_ops: u64,
}

impl Default for RunConfig {
    fn default() -> Self {
        Self {
            workload: WorkloadKind::PointLookup,
            dataset_size: 100_000,
            ops: 200_000,
            seed: 0xADAB7,
            engine: "reference".into(),
            level: 0,
            warmup_ops: 10_000,
            max_secs: Some(10.0),
        }
    }
}

/// Load the dataset, warm up, then measure.
pub fn run<S: LogicalStore>(
    store: &mut S,
    cfg: &RunConfig,
) -> adabt_core::error::Result<RunReport> {
    store.create_collection(COLLECTION, workload_schema())?;

    let loader = Workload::new(cfg.workload, cfg.dataset_size, 0, cfg.seed);
    let preloaded: Vec<_> = loader.preload().collect();
    for (id, rec) in preloaded {
        store.insert(COLLECTION, id, rec)?;
    }

    // Warmup runs on its own generator so the measured phase is not correlated
    // with it; a shared stream would let the warmup prefetch exactly what the
    // measured phase asks for and flatter every cache.
    let mut warm = Workload::new(
        cfg.workload,
        cfg.dataset_size,
        cfg.warmup_ops,
        cfg.seed ^ 0x5EED,
    );
    for _ in 0..cfg.warmup_ops {
        let _ = apply(store, &warm.next_op());
    }

    let mut wl = Workload::new(cfg.workload, cfg.dataset_size, cfg.ops, cfg.seed);
    let mut latency = Histogram::new();
    let mut per_op: BTreeMap<&'static str, Histogram> = BTreeMap::new();
    let mut failures = 0u64;

    let res_start = ResourceSample::now();
    let start = Instant::now();
    let mut executed = 0u64;
    for i in 0..cfg.ops {
        // Checking the clock every operation would show up in the measurement
        // for cheap ops, so the deadline is tested on a coarse stride.
        if let Some(limit) = cfg.max_secs {
            if i % 256 == 0 && start.elapsed().as_secs_f64() >= limit {
                break;
            }
        }
        let op: Op = wl.next_op();
        let name = op.name();
        let t = Instant::now();
        let outcome = apply(store, &op);
        let ns = t.elapsed().as_nanos() as u64;
        latency.record(ns);
        per_op.entry(name).or_default().record(ns);
        if matches!(outcome, OpOutcome::Failed(_)) {
            failures += 1;
        }
        executed += 1;
    }
    let wall_secs = start.elapsed().as_secs_f64();
    let resources = ResourceSample::now().since(&res_start);

    Ok(RunReport {
        workload: cfg.workload,
        engine: cfg.engine.clone(),
        level: cfg.level,
        ops: executed,
        failures,
        wall_secs,
        latency,
        per_op,
        resources,
    })
}

/// Fixed-width table, so a matrix run is readable in a terminal without tools.
pub fn format_table(reports: &[RunReport]) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "{:<18} {:>3} {:>10} {:>12} {:>10} {:>10} {:>10} {:>9} {:>10}\n",
        "workload", "lvl", "ops", "ops/sec", "p50 ns", "p99 ns", "p999 ns", "cpu/Mop", "peak RSS"
    ));
    out.push_str(&"-".repeat(100));
    out.push('\n');
    for r in reports {
        out.push_str(&format!(
            "{:<18} {:>3} {:>10} {:>12.0} {:>10} {:>10} {:>10} {:>9.3} {:>9.1}M\n",
            r.workload.as_str(),
            r.level,
            r.ops,
            r.throughput(),
            r.latency.percentile(50.0),
            r.latency.percentile(99.0),
            r.latency.percentile(99.9),
            r.cpu_per_mop(),
            r.resources.peak_rss_bytes as f64 / 1e6,
        ));
    }
    out
}

/// Per-operation-kind breakdown for a single run.
///
/// Aggregate latency hides the thing worth seeing: a workload mixing point gets
/// with full scans has a bimodal distribution, and its p99 describes the scans
/// rather than anything a user would recognise as typical.
pub fn format_per_op(r: &RunReport) -> String {
    let mut out = format!("\nper-operation breakdown ({})\n", r.workload.as_str());
    out.push_str(&format!(
        "{:<10} {:>10} {:>8} {:>10} {:>10} {:>10}\n",
        "op", "count", "share", "p50 ns", "p99 ns", "max ns"
    ));
    out.push_str(&"-".repeat(62));
    out.push('\n');
    for (name, h) in &r.per_op {
        out.push_str(&format!(
            "{:<10} {:>10} {:>7.1}% {:>10} {:>10} {:>10}\n",
            name,
            h.count(),
            100.0 * h.count() as f64 / r.ops.max(1) as f64,
            h.percentile(50.0),
            h.percentile(99.0),
            h.max(),
        ));
    }
    out
}
#[cfg(test)]
mod tests {
    use super::*;
    use adabt_testkit::reference::ReferenceStore;

    fn small(workload: WorkloadKind) -> RunConfig {
        RunConfig {
            workload,
            dataset_size: 500,
            ops: 2_000,
            warmup_ops: 100,
            max_secs: None,
            ..Default::default()
        }
    }

    #[test]
    fn a_run_produces_a_populated_report() {
        let mut s = ReferenceStore::new();
        let r = run(&mut s, &small(WorkloadKind::PointLookup)).unwrap();
        assert_eq!(r.ops, 2_000);
        assert_eq!(r.latency.count(), 2_000);
        assert!(r.throughput() > 0.0);
        assert!(r.wall_secs > 0.0);
    }

    #[test]
    fn read_only_workloads_do_not_fail_operations() {
        let mut s = ReferenceStore::new();
        let r = run(&mut s, &small(WorkloadKind::PointLookup)).unwrap();
        assert_eq!(
            r.failures, 0,
            "point lookups over a preloaded set must all succeed"
        );
    }

    #[test]
    fn per_op_breakdown_is_recorded() {
        let mut s = ReferenceStore::new();
        let r = run(&mut s, &small(WorkloadKind::ReadWrite8020)).unwrap();
        assert!(r.per_op.contains_key("get"));
        assert!(r.per_op.contains_key("update"));
        let total: u64 = r.per_op.values().map(|h| h.count()).sum();
        assert_eq!(total, r.ops);
    }

    #[test]
    fn scans_are_slower_than_point_lookups() {
        let mut s = ReferenceStore::new();
        let r = run(&mut s, &small(WorkloadKind::RangeScan)).unwrap();
        let scan = r.per_op.get("scan").expect("range_scan must issue scans");
        let get = r.per_op.get("get").expect("range_scan must issue gets");
        assert!(
            scan.percentile(50.0) > get.percentile(50.0),
            "a full scan should cost more than a point get"
        );
    }

    #[test]
    fn json_output_is_wellformed_and_carries_both_axes() {
        let mut s = ReferenceStore::new();
        let j = run(&mut s, &small(WorkloadKind::PointLookup))
            .unwrap()
            .to_json();
        assert!(j.starts_with('{') && j.ends_with('}'));
        assert_eq!(j.matches('{').count(), j.matches('}').count());
        for key in [
            "\"p99\"",
            "\"peak_rss_bytes\"",
            "\"cpu_per_mop\"",
            "\"throughput_ops_per_sec\"",
        ] {
            assert!(j.contains(key), "missing {key} in {j}");
        }
    }

    #[test]
    fn runs_are_reproducible_for_a_fixed_seed() {
        let cfg = small(WorkloadKind::ZipfSkew);
        let mut a = ReferenceStore::new();
        let mut b = ReferenceStore::new();
        let (ra, rb) = (run(&mut a, &cfg).unwrap(), run(&mut b, &cfg).unwrap());
        // Timings vary; the executed work must not.
        assert_eq!(ra.failures, rb.failures);
        assert_eq!(
            ra.per_op
                .iter()
                .map(|(k, h)| (*k, h.count()))
                .collect::<Vec<_>>(),
            rb.per_op
                .iter()
                .map(|(k, h)| (*k, h.count()))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn table_has_a_row_per_report() {
        let mut s = ReferenceStore::new();
        let reports = vec![run(&mut s, &small(WorkloadKind::PointLookup)).unwrap()];
        let t = format_table(&reports);
        assert!(t.contains("point_lookup"));
        assert_eq!(t.lines().count(), 3); // header, rule, one row
    }

    #[test]
    fn the_deadline_stops_a_run_early_and_reports_real_ops() {
        let mut s = ReferenceStore::new();
        let cfg = RunConfig {
            workload: WorkloadKind::RangeScan,
            dataset_size: 20_000,
            ops: 100_000_000,
            warmup_ops: 0,
            max_secs: Some(0.5),
            ..Default::default()
        };
        let r = run(&mut s, &cfg).unwrap();
        assert!(r.ops < cfg.ops, "deadline did not cut the run short");
        assert!(r.ops > 0, "deadline cut the run before any work happened");
        assert_eq!(
            r.latency.count(),
            r.ops,
            "report must count ops actually executed"
        );
        assert!(
            r.wall_secs < 5.0,
            "overran the deadline badly: {}s",
            r.wall_secs
        );
    }

    #[test]
    fn an_absent_deadline_runs_the_full_op_count() {
        let mut s = ReferenceStore::new();
        let r = run(&mut s, &small(WorkloadKind::PointLookup)).unwrap();
        assert_eq!(r.ops, 2_000);
    }
}
