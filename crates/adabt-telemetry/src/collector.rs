//! Workload observation.
//!
//! Sharded rather than a single lock. The first version was one mutex around
//! one `HashMap`, which was honest for M0 and useless on a real hot path:
//! every operation on every thread serialised on it. Telemetry that perturbs
//! the workload cannot be used to decide how to optimize that workload.
//!
//! Each thread is assigned a shard once and stays there, so the common case is
//! an uncontended lock. Reading merges the shards, which is comparatively
//! expensive and comparatively rare — an optimization cycle runs on the order
//! of seconds, an operation on the order of microseconds.

use crate::event::{Event, OpKind, QueryShape};
use crate::histogram::Histogram;
use crate::probe::Probe;
use crate::sketch::TemperatureSketch;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

/// Shards. A power of two so selection is a mask.
const SHARDS: usize = 16;

static NEXT_SHARD: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    /// Assigned once per thread and never changed, so a thread's writes stay on
    /// one shard and the lock is normally uncontended.
    static MY_SHARD: usize = NEXT_SHARD.fetch_add(1, Ordering::Relaxed) % SHARDS;
}

#[derive(Debug, Default, Clone)]
pub struct OpStats {
    pub calls: u64,
    pub rows: u64,
    pub latency: Option<Histogram>,
}

/// Per-query-shape detail: what the adaptive driver keys on.
#[derive(Debug, Default, Clone)]
pub struct ShapeStats {
    pub calls: u64,
    pub rows: u64,
    pub total_nanos: u64,
    pub latency: Histogram,
}

impl ShapeStats {
    pub fn mean_nanos(&self) -> f64 {
        if self.calls == 0 {
            0.0
        } else {
            self.total_nanos as f64 / self.calls as f64
        }
    }
    /// Total time spent on this shape. The right way to rank shapes: a slow
    /// query run twice matters less than a fast one run a million times.
    pub fn total_cost_nanos(&self) -> u64 {
        self.total_nanos
    }
}

#[derive(Debug, Default, Clone)]
pub struct Snapshot {
    pub per_op: HashMap<OpKind, OpStats>,
    pub per_shape: HashMap<QueryShape, ShapeStats>,
    pub cache_hits: HashMap<&'static str, u64>,
    pub cache_misses: HashMap<&'static str, u64>,
    /// `(collection, field) -> times a query filtered on it`.
    pub field_filters: HashMap<(String, String), u64>,
    /// Of those, how many were equality predicates.
    pub equality_filters: HashMap<(String, String), u64>,
    /// How often each *set* of fields was pinned to literals by one query.
    ///
    /// Keyed by collection and the sorted field list, so the key is the
    /// question a composite index answers: "were these fields constrained at
    /// the same time". Only sets of two or more are recorded — a single field
    /// is already covered by `equality_filters`, and storing it twice would
    /// let the two disagree.
    pub pinned_sets: HashMap<(String, Vec<String>), u64>,
    /// How often each filtered field was paired with a projection.
    ///
    /// Keyed by collection, the single equality-filtered field, and the
    /// sorted projected list — the question a covering index answers: "do
    /// the queries filtering this field keep asking for these fields". The
    /// projected list never contains the filtered field; the index carries
    /// its own key whether the caller remembers to ask or not.
    pub projected_covers: HashMap<(String, String, Vec<String>, bool), u64>,
    /// `(collection, field) -> times the planner chose that index`.
    pub index_usage: HashMap<(String, String), u64>,
    /// `(collection, field)` -> index entries written on the write path.
    /// The cost half of retraction; `index_usage` is the benefit half.
    pub index_maintenance: HashMap<(String, String), u64>,
    pub touches: u64,
    pub temperature: Option<TemperatureSketch>,
    pub opt_changes: Vec<(&'static str, bool)>,
}

impl Snapshot {
    pub fn total_calls(&self) -> u64 {
        self.per_op.values().map(|s| s.calls).sum()
    }

    /// Fraction of operations that mutate data.
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

    /// Shapes ranked by total time spent, most expensive first.
    pub fn hottest_shapes(&self, n: usize) -> Vec<(QueryShape, &ShapeStats)> {
        let mut v: Vec<(QueryShape, &ShapeStats)> =
            self.per_shape.iter().map(|(s, st)| (*s, st)).collect();
        v.sort_by(|a, b| {
            b.1.total_cost_nanos()
                .cmp(&a.1.total_cost_nanos())
                .then(a.0.cmp(&b.0))
        });
        v.truncate(n);
        v
    }

    /// Field sets pinned together, most frequent first.
    ///
    /// The signal a composite index is chosen from. Ranked rather than
    /// filtered here so the caller decides what threshold is worth acting on
    /// — this crate measures and does not judge.
    pub fn most_pinned_sets(&self) -> Vec<(String, Vec<String>, u64)> {
        let mut v: Vec<(String, Vec<String>, u64)> = self
            .pinned_sets
            .iter()
            .map(|((c, f), n)| (c.clone(), f.clone(), *n))
            .collect();
        // By count, then by the key, so equal counts rank deterministically
        // rather than by hash order — a proposal that changed between runs on
        // identical traffic would make every experiment irreproducible.
        v.sort_by(|a, b| b.2.cmp(&a.2).then(a.0.cmp(&b.0)).then(a.1.cmp(&b.1)));
        v
    }

    /// Filtered-field/projection pairs ranked by how often they co-occurred.
    ///
    /// Same ordering discipline as `most_pinned_sets`: count first, then the
    /// key, so identical traffic produces identical rankings and an
    /// experiment can be replayed.
    pub fn most_projected_covers(&self) -> Vec<(String, String, Vec<String>, bool, u64)> {
        let mut v: Vec<(String, String, Vec<String>, bool, u64)> = self
            .projected_covers
            .iter()
            .map(|((c, f, p, e), n)| (c.clone(), f.clone(), p.clone(), *e, *n))
            .collect();
        v.sort_by(|a, b| {
            b.4.cmp(&a.4)
                .then(a.0.cmp(&b.0))
                .then(a.1.cmp(&b.1))
                .then(a.2.cmp(&b.2))
                .then(a.3.cmp(&b.3))
        });
        v
    }

    /// Fields ranked by how often queries filtered on them.
    pub fn most_filtered_fields(&self) -> Vec<(String, String, u64)> {
        let mut v: Vec<(String, String, u64)> = self
            .field_filters
            .iter()
            .map(|((c, f), n)| (c.clone(), f.clone(), *n))
            .collect();
        v.sort_by(|a, b| b.2.cmp(&a.2).then((&a.0, &a.1).cmp(&(&b.0, &b.1))));
        v
    }

    /// Fraction of filters on this field that were equality predicates.
    ///
    /// Near 1 wants a hash index; near 0 wants an ordered one; `None` means the
    /// field has not been filtered at all.
    pub fn equality_fraction(&self, collection: &str, field: &str) -> Option<f64> {
        let key = (collection.to_string(), field.to_string());
        let total = *self.field_filters.get(&key)? as f64;
        if total == 0.0 {
            return None;
        }
        Some(self.equality_filters.get(&key).copied().unwrap_or(0) as f64 / total)
    }

    /// Times the planner chose this index. Zero means it is pure overhead.
    pub fn index_use_count(&self, collection: &str, field: &str) -> u64 {
        self.index_usage
            .get(&(collection.to_string(), field.to_string()))
            .copied()
            .unwrap_or(0)
    }

    /// Index entries written for this field on the write path.
    ///
    /// Paired with `index_use_count`, this is what lets a retraction
    /// decision be arithmetic rather than a boolean: an index chosen 50
    /// times and maintained 50,000 times is losing badly, and neither
    /// number alone says so.
    pub fn index_maintenance_count(&self, collection: &str, field: &str) -> u64 {
        self.index_maintenance
            .get(&(collection.to_string(), field.to_string()))
            .copied()
            .unwrap_or(0)
    }

    pub fn filter_count(&self, collection: &str, field: &str) -> u64 {
        self.field_filters
            .get(&(collection.to_string(), field.to_string()))
            .copied()
            .unwrap_or(0)
    }
}

#[derive(Default)]
struct Shard {
    per_op: HashMap<OpKind, (u64, u64, Histogram)>,
    per_shape: HashMap<QueryShape, ShapeStats>,
    cache_hits: HashMap<&'static str, u64>,
    cache_misses: HashMap<&'static str, u64>,
    field_filters: HashMap<(String, String), u64>,
    equality_filters: HashMap<(String, String), u64>,
    pinned_sets: HashMap<(String, Vec<String>), u64>,
    projected_covers: HashMap<(String, String, Vec<String>, bool), u64>,
    index_usage: HashMap<(String, String), u64>,
    index_maintenance: HashMap<(String, String), u64>,
    touches: u64,
    temperature: TemperatureSketch,
    opt_changes: Vec<(&'static str, bool)>,
}

pub struct CollectingProbe {
    shards: Vec<Mutex<Shard>>,
}

impl Default for CollectingProbe {
    fn default() -> Self {
        Self::new()
    }
}

impl CollectingProbe {
    pub fn new() -> Self {
        Self {
            shards: (0..SHARDS).map(|_| Mutex::new(Shard::default())).collect(),
        }
    }

    #[inline]
    fn shard(&self) -> &Mutex<Shard> {
        &self.shards[MY_SHARD.with(|s| *s)]
    }

    pub fn shard_count(&self) -> usize {
        SHARDS
    }

    /// Merge every shard into one view.
    pub fn snapshot(&self) -> Snapshot {
        let mut out = Snapshot {
            temperature: Some(TemperatureSketch::default()),
            ..Default::default()
        };
        for shard in &self.shards {
            let g = shard.lock().unwrap_or_else(|e| e.into_inner());
            for (kind, (calls, rows, hist)) in &g.per_op {
                let e = out.per_op.entry(*kind).or_default();
                e.calls += calls;
                e.rows += rows;
                match &mut e.latency {
                    Some(h) => h.merge(hist),
                    None => e.latency = Some(hist.clone()),
                }
            }
            for (shape, st) in &g.per_shape {
                let e = out.per_shape.entry(*shape).or_default();
                e.calls += st.calls;
                e.rows += st.rows;
                e.total_nanos += st.total_nanos;
                e.latency.merge(&st.latency);
            }
            for (k, v) in &g.cache_hits {
                *out.cache_hits.entry(k).or_default() += v;
            }
            for (k, v) in &g.cache_misses {
                *out.cache_misses.entry(k).or_default() += v;
            }
            for (k, v) in &g.field_filters {
                *out.field_filters.entry(k.clone()).or_default() += v;
            }
            for (k, v) in &g.index_usage {
                *out.index_usage.entry(k.clone()).or_default() += v;
            }
            for (k, v) in &g.index_maintenance {
                *out.index_maintenance.entry(k.clone()).or_default() += v;
            }
            for (k, v) in &g.equality_filters {
                *out.equality_filters.entry(k.clone()).or_default() += v;
            }
            for (k, v) in &g.pinned_sets {
                *out.pinned_sets.entry(k.clone()).or_default() += v;
            }
            for (k, v) in &g.projected_covers {
                *out.projected_covers.entry(k.clone()).or_default() += v;
            }
            out.touches += g.touches;
            if let Some(t) = &mut out.temperature {
                t.merge(&g.temperature);
            }
            out.opt_changes.extend(g.opt_changes.iter().copied());
        }
        out
    }

    pub fn reset(&self) {
        for shard in &self.shards {
            *shard.lock().unwrap_or_else(|e| e.into_inner()) = Shard::default();
        }
    }

    /// Forget a fraction of what has been counted so far.
    ///
    /// **Cumulative counters answer "was this ever useful", and the optimizer
    /// needs to know "is this useful now".** Without forgetting, an index that
    /// served a workload perfectly for an hour keeps that record forever: its
    /// lifetime use ratio stays high long after the traffic that justified it
    /// has gone, and the retraction logic — which is the only thing stopping the
    /// optimizer from being a ratchet — can never fire for it.
    ///
    /// That is not a theoretical concern. A soak run drove 25,000 point lookups
    /// through a database still carrying two indexes built for a filtering
    /// workload that had ended, and retracted nothing, because by the numbers
    /// both indexes had been extremely useful.
    ///
    /// Decay happens here rather than on the hot path: the optimizer calls it
    /// once per cycle, from one thread, so the recording path keeps its
    /// per-shard counters and its lack of contention.
    ///
    /// Integer arithmetic, so a count reaches zero rather than approaching it —
    /// which matters, because "nothing has used this at all lately" is a much
    /// safer retraction test than any ratio.
    pub fn decay(&self, numerator: u64, denominator: u64) {
        debug_assert!(numerator < denominator && denominator > 0);
        let scale = |v: &mut u64| *v = *v * numerator / denominator;
        for shard in &self.shards {
            let mut g = shard.lock().unwrap_or_else(|e| e.into_inner());
            for (calls, rows, _) in g.per_op.values_mut() {
                scale(calls);
                scale(rows);
            }
            for st in g.per_shape.values_mut() {
                scale(&mut st.calls);
                scale(&mut st.rows);
                scale(&mut st.total_nanos);
            }
            for v in g.cache_hits.values_mut() {
                scale(v);
            }
            for v in g.cache_misses.values_mut() {
                scale(v);
            }
            for v in g.field_filters.values_mut() {
                scale(v);
            }
            for v in g.equality_filters.values_mut() {
                scale(v);
            }
            for v in g.index_usage.values_mut() {
                scale(v);
            }
            // Decayed alongside `index_usage`, and it must be: comparing a
            // decayed benefit against an undecayed cost would make every
            // index look worse the longer it existed.
            for v in g.index_maintenance.values_mut() {
                scale(v);
            }
            // Latency histograms and the temperature sketch are left alone: the
            // first is a distribution rather than a count, and the second does
            // its own decaying.
        }
    }
}

impl Probe for CollectingProbe {
    fn record(&self, ev: Event<'_>) {
        let mut g = self.shard().lock().unwrap_or_else(|e| e.into_inner());
        match ev {
            Event::Op {
                kind,
                nanos,
                rows,
                shape,
                ..
            } => {
                let e = g
                    .per_op
                    .entry(kind)
                    .or_insert_with(|| (0, 0, Histogram::new()));
                e.0 += 1;
                e.1 += rows;
                e.2.record(nanos);
                // Only real shapes get a breakdown; UNKNOWN would collapse every
                // non-query operation into one meaningless bucket.
                if shape != QueryShape::UNKNOWN {
                    let s = g.per_shape.entry(shape).or_default();
                    s.calls += 1;
                    s.rows += rows;
                    s.total_nanos += nanos;
                    s.latency.record(nanos);
                }
            }
            Event::Touch { id, .. } => {
                g.touches += 1;
                g.temperature.observe(id.0);
            }
            Event::FieldsPinnedTogether { collection, fields } => {
                if fields.len() > 1 {
                    let key = (collection.to_string(), fields.to_vec());
                    *g.pinned_sets.entry(key).or_default() += 1;
                }
            }
            Event::FieldsProjectedTogether {
                collection,
                filtered,
                fields,
                equality,
            } => {
                if !fields.is_empty() && !filtered.is_empty() {
                    let key = (
                        collection.to_string(),
                        filtered.to_string(),
                        fields.to_vec(),
                        equality,
                    );
                    *g.projected_covers.entry(key).or_default() += 1;
                }
            }
            Event::FieldFiltered {
                collection,
                field,
                equality,
            } => {
                let key = (collection.to_string(), field.to_string());
                *g.field_filters.entry(key.clone()).or_default() += 1;
                if equality {
                    *g.equality_filters.entry(key).or_default() += 1;
                }
            }
            Event::IndexUsed { collection, field } => {
                *g.index_usage
                    .entry((collection.to_string(), field.to_string()))
                    .or_default() += 1;
            }
            Event::IndexMaintained { collection, field } => {
                *g.index_maintenance
                    .entry((collection.to_string(), field.to_string()))
                    .or_default() += 1;
            }
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
    use adabt_core::ids::RecordId;

    fn op(p: &CollectingProbe, kind: OpKind, nanos: u64) {
        p.record(Event::Op {
            collection: "c",
            kind,
            shape: QueryShape::UNKNOWN,
            nanos,
            rows: 1,
        });
    }

    fn shaped(p: &CollectingProbe, shape: u64, nanos: u64, rows: u64) {
        p.record(Event::Op {
            collection: "c",
            kind: OpKind::Scan,
            shape: QueryShape(shape),
            nanos,
            rows,
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
    fn shapes_are_ranked_by_total_time_not_by_call_count() {
        // A fast query run a million times matters more than a slow one run
        // twice, and ranking by count alone would get that backwards.
        let p = CollectingProbe::new();
        for _ in 0..1_000 {
            shaped(&p, 1, 1_000, 1); // 1ms total
        }
        for _ in 0..2 {
            shaped(&p, 2, 100_000, 1); // 0.2ms total
        }
        let s = p.snapshot();
        let hottest = s.hottest_shapes(2);
        assert_eq!(
            hottest[0].0,
            QueryShape(1),
            "ranked by count instead of cost"
        );
        assert_eq!(hottest[0].1.calls, 1_000);
        assert_eq!(hottest[1].0, QueryShape(2));
    }

    #[test]
    fn unknown_shapes_are_not_given_a_breakdown() {
        let p = CollectingProbe::new();
        for _ in 0..100 {
            op(&p, OpKind::Get, 100);
        }
        assert!(
            p.snapshot().per_shape.is_empty(),
            "UNKNOWN would collapse every non-query op into one bucket"
        );
    }

    #[test]
    fn filtered_fields_are_ranked_by_frequency() {
        let p = CollectingProbe::new();
        for _ in 0..50 {
            p.record(Event::FieldFiltered {
                collection: "users",
                field: "country",
                equality: true,
            });
        }
        for _ in 0..5 {
            p.record(Event::FieldFiltered {
                collection: "users",
                field: "age",
                equality: true,
            });
        }
        let s = p.snapshot();
        let ranked = s.most_filtered_fields();
        assert_eq!(ranked[0], ("users".into(), "country".into(), 50));
        assert_eq!(s.filter_count("users", "age"), 5);
        assert_eq!(s.filter_count("users", "nope"), 0);
    }

    #[test]
    fn record_touches_feed_the_temperature_sketch() {
        let p = CollectingProbe::new();
        for _ in 0..5_000 {
            p.record(Event::Touch {
                collection: "c",
                id: RecordId(7),
            });
        }
        p.record(Event::Touch {
            collection: "c",
            id: RecordId(9_999),
        });
        let s = p.snapshot();
        let t = s.temperature.unwrap();
        assert!(t.estimate(7) > t.estimate(9_999) * 100);
    }

    #[test]
    fn shards_merge_into_one_consistent_view() {
        let p = std::sync::Arc::new(CollectingProbe::new());
        let mut handles = Vec::new();
        for _ in 0..8 {
            let p = std::sync::Arc::clone(&p);
            handles.push(std::thread::spawn(move || {
                for _ in 0..1_000 {
                    p.record(Event::Op {
                        collection: "c",
                        kind: OpKind::Get,
                        shape: QueryShape(5),
                        nanos: 10,
                        rows: 1,
                    });
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        let s = p.snapshot();
        assert_eq!(s.total_calls(), 8_000, "events were lost across shards");
        assert_eq!(s.per_shape[&QueryShape(5)].calls, 8_000);
    }

    #[test]
    fn concurrent_recording_does_not_serialise_on_one_lock() {
        // Not a timing assertion — a structural one. Threads must land on
        // different shards, or the sharding is decorative.
        let p = CollectingProbe::new();
        assert!(p.shard_count() > 1);
        let seen: std::sync::Arc<Mutex<Vec<usize>>> = Default::default();
        let mut handles = Vec::new();
        for _ in 0..8 {
            let seen = std::sync::Arc::clone(&seen);
            handles.push(std::thread::spawn(move || {
                let idx = MY_SHARD.with(|s| *s);
                seen.lock().unwrap().push(idx);
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        let v = seen.lock().unwrap();
        let distinct: std::collections::HashSet<_> = v.iter().collect();
        assert!(
            distinct.len() > 1,
            "every thread landed on one shard: {v:?}"
        );
    }

    #[test]
    fn reset_clears_all_state() {
        let p = CollectingProbe::new();
        op(&p, OpKind::Get, 100);
        shaped(&p, 1, 10, 1);
        p.reset();
        let s = p.snapshot();
        assert_eq!(s.total_calls(), 0);
        assert!(s.per_shape.is_empty());
    }
}
