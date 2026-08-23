//! An in-process collecting probe.
//!
//! M0 scope: aggregate counters plus one latency histogram per operation kind,
//! behind a mutex. That is enough to make the benchmark harness and the
//! differential rig useful, and deliberately not more — per-core sharding,
//! per-`QueryShape` breakdowns and decaying temperature sketches arrive in M5,
//! once there is a real hot path whose perturbation can be measured.

use crate::event::{Event, OpKind};
use crate::histogram::Histogram;
use crate::probe::Probe;
use std::collections::HashMap;
use std::sync::Mutex;

#[derive(Debug, Default, Clone)]
pub struct OpStats {
    pub calls: u64,
    pub rows: u64,
    pub latency: Option<Histogram>,
}

#[derive(Debug, Default, Clone)]
pub struct Snapshot {
    pub per_op: HashMap<OpKind, OpStats>,
    pub cache_hits: HashMap<&'static str, u64>,
    pub cache_misses: HashMap<&'static str, u64>,
    pub touches: u64,
    pub opt_changes: Vec<(&'static str, bool)>,
}

impl Snapshot {
    pub fn total_calls(&self) -> u64 {
        self.per_op.values().map(|s| s.calls).sum()
    }

    /// Fraction of operations that mutate data. The read/write ratio is one of
    /// the primary inputs the adaptive driver will key on.
    pub fn write_fraction(&self) -> f64 {
        let total = self.total_calls();
        if total == 0 {
            return 0.0;
        }
        let writes: u64 = self
            .per_op
            .iter()
            .filter(|(k, _)| k.is_write())
            .map(|(_, s)| s.calls)
            .sum();
        writes as f64 / total as f64
    }

    pub fn hit_rate(&self, cache: &str) -> Option<f64> {
        let h = *self.cache_hits.get(cache)? as f64;
        let m = *self.cache_misses.get(cache).unwrap_or(&0) as f64;
        if h + m == 0.0 {
            None
        } else {
            Some(h / (h + m))
        }
    }

    pub fn latency(&self, kind: OpKind) -> Option<&Histogram> {
        self.per_op.get(&kind)?.latency.as_ref()
    }
}

#[derive(Default)]
struct Inner {
    per_op: HashMap<OpKind, (u64, u64, Histogram)>,
    cache_hits: HashMap<&'static str, u64>,
    cache_misses: HashMap<&'static str, u64>,
    touches: u64,
    opt_changes: Vec<(&'static str, bool)>,
}

#[derive(Default)]
pub struct CollectingProbe {
    inner: Mutex<Inner>,
}

impl CollectingProbe {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn snapshot(&self) -> Snapshot {
        let g = self.inner.lock().unwrap();
        Snapshot {
            per_op: g
                .per_op
                .iter()
                .map(|(k, (calls, rows, h))| {
                    (
                        *k,
                        OpStats {
                            calls: *calls,
                            rows: *rows,
                            latency: Some(h.clone()),
                        },
                    )
                })
                .collect(),
            cache_hits: g.cache_hits.clone(),
            cache_misses: g.cache_misses.clone(),
            touches: g.touches,
            opt_changes: g.opt_changes.clone(),
        }
    }

    pub fn reset(&self) {
        *self.inner.lock().unwrap() = Inner::default();
    }
}

impl Probe for CollectingProbe {
    fn record(&self, ev: Event<'_>) {
        let mut g = self.inner.lock().unwrap();
        match ev {
            Event::Op {
                kind, nanos, rows, ..
            } => {
                let e = g
                    .per_op
                    .entry(kind)
                    .or_insert_with(|| (0, 0, Histogram::new()));
                e.0 += 1;
                e.1 += rows;
                e.2.record(nanos);
            }
            Event::Touch { .. } => g.touches += 1,
            Event::CacheProbe { name, hit } => {
                if hit {
                    *g.cache_hits.entry(name).or_default() += 1;
                } else {
                    *g.cache_misses.entry(name).or_default() += 1;
                }
            }
            Event::OptChanged { name, enabled } => g.opt_changes.push((name, enabled)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::QueryShape;

    fn op(p: &CollectingProbe, kind: OpKind, nanos: u64) {
        p.record(Event::Op {
            collection: "c",
            kind,
            shape: QueryShape::UNKNOWN,
            nanos,
            rows: 1,
        });
    }

    #[test]
    fn write_fraction_counts_only_mutating_ops() {
        let p = CollectingProbe::new();
        for _ in 0..8 {
            op(&p, OpKind::Get, 100);
        }
        for _ in 0..2 {
            op(&p, OpKind::Insert, 100);
        }
        assert!((p.snapshot().write_fraction() - 0.2).abs() < 1e-9);
    }

    #[test]
    fn empty_snapshot_reports_no_writes_rather_than_dividing_by_zero() {
        assert_eq!(CollectingProbe::new().snapshot().write_fraction(), 0.0);
    }

    #[test]
    fn cache_hit_rate_is_reported_per_cache() {
        let p = CollectingProbe::new();
        for _ in 0..3 {
            p.record(Event::CacheProbe {
                name: "plan",
                hit: true,
            });
        }
        p.record(Event::CacheProbe {
            name: "plan",
            hit: false,
        });
        let s = p.snapshot();
        assert_eq!(s.hit_rate("plan"), Some(0.75));
        assert_eq!(s.hit_rate("absent"), None);
    }

    #[test]
    fn latency_is_tracked_per_operation_kind() {
        let p = CollectingProbe::new();
        op(&p, OpKind::Get, 100);
        op(&p, OpKind::Scan, 1_000_000);
        let s = p.snapshot();
        assert!(s.latency(OpKind::Get).unwrap().max() < s.latency(OpKind::Scan).unwrap().max());
        assert!(s.latency(OpKind::Delete).is_none());
    }

    #[test]
    fn reset_clears_all_state() {
        let p = CollectingProbe::new();
        op(&p, OpKind::Get, 100);
        p.reset();
        assert_eq!(p.snapshot().total_calls(), 0);
    }
}
