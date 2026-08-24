//! Cost and benefit estimates.
//!
//! A `CostEstimate` is a **vector, never a scalar**. Collapsing it to one number
//! would make multi-objective optimization impossible: the whole point is that a
//! candidate can be good for speed and bad for memory at the same time, and that
//! which of those matters is the *user's* call, expressed as priorities.
//!
//! Anything that reduces this to a single score belongs in the scoring function,
//! where the policy is in scope — never here.

/// A multiplicative change. `0.5` means halved, `2.0` means doubled.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Ratio(pub f64);

impl Ratio {
    pub const UNCHANGED: Ratio = Ratio(1.0);
    pub fn improvement(self) -> f64 {
        1.0 - self.0
    }
    pub fn percent_change(self) -> f64 {
        (self.0 - 1.0) * 100.0
    }
}

impl Default for Ratio {
    fn default() -> Self {
        Ratio::UNCHANGED
    }
}

/// One-time cost of building the structure a change needs.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct BuildCost {
    pub estimated_secs: f64,
    pub rows_read: u64,
    /// Whether the build can proceed without blocking the workload.
    pub online: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct CostEstimate {
    pub p50_delta: Ratio,
    pub p99_delta: Ratio,
    pub throughput_delta: Ratio,
    pub ram_bytes: i64,
    pub storage_bytes: i64,
    pub cpu_frac: f64,
    pub io_ops: i64,
    pub net_bytes: i64,
    pub build: BuildCost,
    /// Per-write amortised upkeep, as a fraction of the write's own cost.
    pub maintain_cost: f64,
    /// How much the estimator trusts itself, 0 to 1. Low confidence should make
    /// a candidate a *candidate for measurement*, not a candidate for rejection.
    pub confidence: f64,
}

impl CostEstimate {
    /// A change believed to do nothing.
    pub fn neutral() -> Self {
        Self {
            confidence: 1.0,
            ..Default::default()
        }
    }

    pub fn faster(p50: f64, p99: f64) -> Self {
        Self {
            p50_delta: Ratio(p50),
            p99_delta: Ratio(p99),
            throughput_delta: Ratio(1.0 / p50.max(1e-9)),
            confidence: 0.5,
            ..Default::default()
        }
    }

    pub fn with_ram(mut self, bytes: i64) -> Self {
        self.ram_bytes = bytes;
        self
    }
    pub fn with_maintenance(mut self, frac: f64) -> Self {
        self.maintain_cost = frac;
        self
    }
    pub fn with_confidence(mut self, c: f64) -> Self {
        self.confidence = c.clamp(0.0, 1.0);
        self
    }
    pub fn with_build(mut self, build: BuildCost) -> Self {
        self.build = build;
        self
    }

    /// Whether this change is believed to help latency at all.
    pub fn helps_latency(&self) -> bool {
        self.p50_delta.0 < 1.0 || self.p99_delta.0 < 1.0
    }

    /// Whether it costs meaningful resources.
    pub fn costs_resources(&self) -> bool {
        self.ram_bytes > 0 || self.storage_bytes > 0 || self.cpu_frac > 0.0
    }
}

/// Signed effect on each optimization axis, from -10 (much worse) to +10.
///
/// Used for explanation and for coarse filtering. It is a *summary* of the cost
/// estimate for humans, not a substitute for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AxisEffects {
    pub speed: i8,
    pub resources: i8,
    pub freedom: i8,
}

impl AxisEffects {
    pub const fn new(speed: i8, resources: i8, freedom: i8) -> Self {
        Self {
            speed,
            resources,
            freedom,
        }
    }

    pub fn describe(&self) -> String {
        fn part(name: &str, v: i8) -> Option<String> {
            match v {
                0 => None,
                v if v > 0 => Some(format!("+{v} {name}")),
                v => Some(format!("{v} {name}")),
            }
        }
        let parts: Vec<String> = [
            part("speed", self.speed),
            part("resources", self.resources),
            part("freedom", self.freedom),
        ]
        .into_iter()
        .flatten()
        .collect();
        if parts.is_empty() {
            "no axis effect".to_string()
        } else {
            parts.join(", ")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_ratio_reports_improvement_and_percent_change() {
        assert!((Ratio(0.5).improvement() - 0.5).abs() < 1e-9);
        assert!((Ratio(0.5).percent_change() + 50.0).abs() < 1e-9);
        assert_eq!(Ratio::UNCHANGED.improvement(), 0.0);
    }

    #[test]
    fn a_neutral_estimate_neither_helps_nor_costs() {
        let e = CostEstimate::neutral();
        assert!(!e.helps_latency());
        assert!(!e.costs_resources());
        assert_eq!(e.confidence, 1.0);
    }

    #[test]
    fn a_faster_estimate_raises_throughput_as_latency_falls() {
        let e = CostEstimate::faster(0.25, 0.5);
        assert!(e.helps_latency());
        assert!((e.throughput_delta.0 - 4.0).abs() < 1e-6);
    }

    #[test]
    fn cost_and_benefit_are_recorded_independently() {
        // The point of a vector: this candidate is good for latency and bad for
        // memory at the same time, and nothing here decides which wins.
        let e = CostEstimate::faster(0.5, 0.5)
            .with_ram(200 * 1024 * 1024 * 1024)
            .with_maintenance(0.05);
        assert!(e.helps_latency());
        assert!(e.costs_resources());
        assert!(e.maintain_cost > 0.0);
    }

    #[test]
    fn confidence_is_clamped() {
        assert_eq!(CostEstimate::neutral().with_confidence(5.0).confidence, 1.0);
        assert_eq!(
            CostEstimate::neutral().with_confidence(-1.0).confidence,
            0.0
        );
    }

    #[test]
    fn axis_effects_describe_only_what_they_change() {
        assert_eq!(
            AxisEffects::new(3, -2, 0).describe(),
            "+3 speed, -2 resources"
        );
        assert_eq!(AxisEffects::default().describe(), "no axis effect");
    }
}
