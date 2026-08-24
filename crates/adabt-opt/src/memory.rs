//! Workload memory: recognising a workload seen before, and recalling what
//! was already learned about it.
//!
//! # The problem this solves
//!
//! The adaptive driver learns by measuring, and measuring costs time. A
//! database whose workload swings between two shapes — batch loading by
//! night, point lookups by day — pays that learning cost *every time it
//! swings*, rediscovering each time a conclusion it already reached
//! yesterday. Worse, the experiment loop's own caution makes this expensive
//! by design: proving a change is safe takes shadow trials and canary
//! traffic, and none of that work is reusable today.
//!
//! # What a fingerprint is, and what it deliberately is not
//!
//! A `Fingerprint` is a coarse, bucketed description of *shape*: the
//! read/write mix, how concentrated traffic is on a few query shapes, and
//! which fields are being filtered. Bucketed on purpose — a workload that is
//! 71% reads and one that is 73% reads are the same workload for every
//! decision the optimizer makes, and a fingerprint that distinguished them
//! would recall nothing, ever.
//!
//! It is explicitly **not** an identity or a hash of the data. Two different
//! databases running the same access pattern should fingerprint alike; that
//! is the point.
//!
//! # Why recall is a *suggestion*, never an application
//!
//! `recall` returns a configuration that worked before. It does not apply
//! it, and nothing here bypasses the controller's gates — guarantees,
//! prerequisites, conflicts, applicability, constraints, and the sink's own
//! veto all still run, exactly as they do for a decision the driver reached
//! from scratch. That is the same rule the manual driver follows (see
//! `driver.rs`: "nothing a human can ask for bypasses the machinery the
//! optimizer uses"), applied to memory: a remembered configuration is a
//! *hypothesis worth trying first*, not evidence that it is still correct.
//! The workload may look identical and the data may have grown tenfold.

use crate::config::OptimizationConfig;
use adabt_telemetry::Snapshot;
use std::collections::BTreeMap;

/// How many buckets each continuous dimension is quantised into.
///
/// Five is enough to separate "mostly reads" from "mixed" from "mostly
/// writes" without splitting hairs the optimizer cannot act on differently.
const BUCKETS: u8 = 5;

fn bucket(fraction: f64) -> u8 {
    let clamped = fraction.clamp(0.0, 1.0);
    let b = (clamped * BUCKETS as f64) as u8;
    b.min(BUCKETS - 1)
}

/// A coarse description of what a workload is doing, comparable across time.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Fingerprint {
    /// Bucketed fraction of operations that write.
    write_mix: u8,
    /// Bucketed concentration: what share of calls the single busiest query
    /// shape accounts for. Separates "one hot query" from "a broad mix",
    /// which want very different physical designs.
    shape_concentration: u8,
    /// The fields traffic filters on, most-filtered first, capped.
    ///
    /// Names rather than counts: *which* fields are filtered is what decides
    /// which indexes are worth having, and the exact counts move constantly
    /// while the set of hot fields is stable — the property a fingerprint
    /// needs.
    hot_fields: Vec<(String, String)>,
}

/// Hot fields kept in a fingerprint. Beyond a handful, the tail is noise
/// that would make two runs of the same workload fingerprint differently.
const MAX_HOT_FIELDS: usize = 4;

impl Fingerprint {
    /// Derive a fingerprint from what telemetry currently holds.
    pub fn of(telemetry: &Snapshot) -> Fingerprint {
        let total = telemetry.total_calls();
        let busiest = telemetry
            .per_shape
            .values()
            .map(|s| s.calls)
            .max()
            .unwrap_or(0);
        let concentration = if total == 0 {
            0.0
        } else {
            busiest as f64 / total as f64
        };

        // Sorted by count, then by name so that ties do not make two
        // fingerprints of the same workload differ by map iteration order.
        let mut fields: Vec<(&(String, String), &u64)> = telemetry.field_filters.iter().collect();
        fields.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));

        Fingerprint {
            write_mix: bucket(telemetry.write_fraction()),
            shape_concentration: bucket(concentration),
            hot_fields: fields
                .into_iter()
                .take(MAX_HOT_FIELDS)
                .map(|(k, _)| k.clone())
                .collect(),
        }
    }

    /// How alike two fingerprints are, 0.0 to 1.0.
    ///
    /// Not equality: a workload that gains one new filtered field is
    /// substantially the same workload, and requiring an exact match would
    /// make memory useless in practice while looking rigorous.
    pub fn similarity(&self, other: &Fingerprint) -> f64 {
        let mix = 1.0 - (self.write_mix as f64 - other.write_mix as f64).abs() / BUCKETS as f64;
        let conc = 1.0
            - (self.shape_concentration as f64 - other.shape_concentration as f64).abs()
                / BUCKETS as f64;
        let fields = jaccard(&self.hot_fields, &other.hot_fields);
        // Equal thirds: no dimension is known to matter more than another,
        // and inventing weights without measurement would be exactly the
        // "invented numbers dressed up as rigour" this project's planner
        // docs already warn against.
        (mix + conc + fields) / 3.0
    }
}

fn jaccard(a: &[(String, String)], b: &[(String, String)]) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    let shared = a.iter().filter(|x| b.contains(x)).count();
    let union = a.len() + b.len() - shared;
    if union == 0 {
        1.0
    } else {
        shared as f64 / union as f64
    }
}

/// Similarity at or above which a remembered configuration is offered.
///
/// High deliberately. Recalling a configuration for a workload that merely
/// rhymes with a remembered one would propose changes the current traffic
/// does not justify, and the driver would then have to measure and retract
/// them — strictly worse than never having recalled at all.
pub const RECALL_THRESHOLD: f64 = 0.85;

/// What was learned about one workload shape.
#[derive(Debug, Clone)]
struct Remembered {
    config: OptimizationConfig,
    /// Times a workload matching this fingerprint has been seen. Kept so a
    /// shape observed once does not outrank one confirmed repeatedly.
    seen: u32,
}

/// Configurations that worked, indexed by the workload shape they worked for.
#[derive(Debug, Clone, Default)]
pub struct WorkloadMemory {
    entries: BTreeMap<Fingerprint, Remembered>,
}

impl WorkloadMemory {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Record that `config` is what this database settled on while the
    /// workload looked like `fingerprint`.
    ///
    /// Overwrites rather than merges: the current configuration is the whole
    /// answer for this shape, and merging two configurations would produce
    /// one that was never actually run, let alone measured.
    pub fn remember(&mut self, fingerprint: Fingerprint, config: OptimizationConfig) {
        let entry = self.entries.entry(fingerprint).or_insert(Remembered {
            config: config.clone(),
            seen: 0,
        });
        entry.config = config;
        entry.seen += 1;
    }

    /// The best remembered configuration for a workload like `current`, if
    /// any is close enough to be worth trying.
    ///
    /// Returns the *config* and the similarity that justified it, so a caller
    /// can put the reason in the decision log rather than applying something
    /// unexplained.
    pub fn recall(&self, current: &Fingerprint) -> Option<(&OptimizationConfig, f64)> {
        let mut best: Option<(&Fingerprint, &Remembered, f64)> = None;
        for (fp, remembered) in &self.entries {
            let score = current.similarity(fp);
            if score < RECALL_THRESHOLD {
                continue;
            }
            let better = match best {
                None => true,
                // Ties broken by how often the shape has been confirmed, so
                // a one-off does not displace a repeatedly-seen match.
                Some((_, prev, prev_score)) => {
                    score > prev_score || (score == prev_score && remembered.seen > prev.seen)
                }
            };
            if better {
                best = Some((fp, remembered, score));
            }
        }
        best.map(|(_, r, score)| (&r.config, score))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use adabt_telemetry::event::{Event, OpKind, QueryShape};
    use adabt_telemetry::{CollectingProbe, Probe};

    fn snapshot(reads: u32, writes: u32, fields: &[(&str, u32)]) -> adabt_telemetry::Snapshot {
        let p = CollectingProbe::new();
        for _ in 0..reads {
            p.record(Event::Op {
                collection: "users",
                kind: OpKind::Get,
                shape: QueryShape(1),
                nanos: 100,
                rows: 1,
            });
        }
        for _ in 0..writes {
            p.record(Event::Op {
                collection: "users",
                kind: OpKind::Insert,
                shape: QueryShape(2),
                nanos: 100,
                rows: 1,
            });
        }
        for (f, n) in fields {
            for _ in 0..*n {
                p.record(Event::FieldFiltered {
                    collection: "users",
                    field: f,
                    equality: true,
                });
            }
        }
        p.snapshot()
    }

    fn cfg(name: &str) -> OptimizationConfig {
        let mut c = OptimizationConfig::new();
        c.enable(name, "global", Default::default());
        c
    }

    #[test]
    fn the_same_workload_fingerprints_the_same_way_twice() {
        let a = Fingerprint::of(&snapshot(900, 100, &[("country", 50)]));
        let b = Fingerprint::of(&snapshot(900, 100, &[("country", 50)]));
        assert_eq!(a, b);
        assert_eq!(a.similarity(&b), 1.0);
    }

    #[test]
    fn small_drift_does_not_change_the_fingerprint() {
        // The property that makes memory usable at all: 71% and 73% reads are
        // the same workload for every decision the optimizer can make.
        let a = Fingerprint::of(&snapshot(710, 290, &[("country", 50)]));
        let b = Fingerprint::of(&snapshot(730, 270, &[("country", 55)]));
        assert_eq!(a, b, "trivial drift produced a different fingerprint");
    }

    #[test]
    fn a_genuinely_different_workload_fingerprints_differently() {
        let read_heavy = Fingerprint::of(&snapshot(1000, 0, &[("country", 50)]));
        let write_heavy = Fingerprint::of(&snapshot(0, 1000, &[("country", 50)]));
        assert_ne!(read_heavy, write_heavy);
        assert!(
            read_heavy.similarity(&write_heavy) < RECALL_THRESHOLD,
            "similarity {} should be below the recall bar",
            read_heavy.similarity(&write_heavy)
        );
    }

    #[test]
    fn filtering_different_fields_lowers_similarity() {
        let a = Fingerprint::of(&snapshot(900, 100, &[("country", 50)]));
        let b = Fingerprint::of(&snapshot(900, 100, &[("age", 50)]));
        assert!(
            a.similarity(&b) < 1.0,
            "different hot fields should not look identical"
        );
    }

    #[test]
    fn a_remembered_configuration_is_recalled_for_a_matching_workload() {
        let mut mem = WorkloadMemory::new();
        let fp = Fingerprint::of(&snapshot(900, 100, &[("country", 50)]));
        mem.remember(fp.clone(), cfg("auto_index"));

        let later = Fingerprint::of(&snapshot(910, 90, &[("country", 60)]));
        let (recalled, score) = mem.recall(&later).expect("should have recalled");
        assert!(recalled.is_enabled_anywhere("auto_index"));
        assert!(score >= RECALL_THRESHOLD);
    }

    #[test]
    fn an_unrelated_workload_recalls_nothing() {
        let mut mem = WorkloadMemory::new();
        mem.remember(
            Fingerprint::of(&snapshot(1000, 0, &[("country", 50)])),
            cfg("auto_index"),
        );
        let write_heavy = Fingerprint::of(&snapshot(0, 1000, &[("zzz", 50)]));
        assert!(
            mem.recall(&write_heavy).is_none(),
            "recalled a configuration for an unrelated workload"
        );
    }

    #[test]
    fn remembering_the_same_shape_twice_updates_rather_than_duplicates() {
        let mut mem = WorkloadMemory::new();
        let fp = Fingerprint::of(&snapshot(900, 100, &[("country", 50)]));
        mem.remember(fp.clone(), cfg("auto_index"));
        mem.remember(fp.clone(), cfg("column_store"));
        assert_eq!(mem.len(), 1, "one shape must not accumulate entries");
        let (recalled, _) = mem.recall(&fp).unwrap();
        assert!(
            recalled.is_enabled_anywhere("column_store"),
            "the newer configuration should have replaced the older"
        );
        assert!(!recalled.is_enabled_anywhere("auto_index"));
    }

    #[test]
    fn an_empty_memory_recalls_nothing_rather_than_panicking() {
        let mem = WorkloadMemory::new();
        assert!(mem
            .recall(&Fingerprint::of(&snapshot(10, 10, &[])))
            .is_none());
        assert!(mem.is_empty());
    }
}
