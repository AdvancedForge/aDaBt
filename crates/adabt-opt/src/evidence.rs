//! What the recent workload asked for, in the terms estimates are made in.
//!
//! An estimate that does not look at the workload claims the same gain on
//! every workload. On the expert comparison that is what kept a database
//! serving nothing but identity lookups carrying a column store, delta
//! encoding, read-ahead and per-core scan workers: each claimed 10–50% on
//! traffic none of them could touch, at 30–50% confidence, and that cleared
//! the bar. Calibration could not repair it in time, because it trusts one
//! measurement an eighth as much as eight.
//!
//! Each optimization helps one kind of work. This sorts the recent operations
//! by that kind, so an estimate can scale its claimed gain by the share of the
//! workload it could actually affect, and claim nothing where that share is
//! zero.
//!
//! # What telemetry can and cannot tell apart
//!
//! Every query is recorded under the same operation kind, so the kinds below
//! are inferred from what else a query left behind:
//!
//! - **Filtered reads** left one `FieldFiltered` event per field they
//!   constrained. Their sum is the number of filtered queries, exactly for
//!   single-field predicates and counting a multi-field one once per field.
//!   (The most-filtered field's count would be a lower bound instead, but it
//!   undercounts whenever two kinds of filtered query are mixed, which is
//!   every change of workload.)
//! - Of those, the ones **an index served** left an `IndexUsed`, and the ones
//!   **the result cache answered** left a hit; neither scanned. The rest did.
//!   The ones an index served and the cache did not answer **fetched each
//!   match by identity**, as a lookup does. What the cache answers is taken
//!   as its hit rate over the reads that reached it, so reads that bypassed
//!   it during an experiment do not hide its share.
//! - **Unfiltered reads** are what remains, and those that returned at most
//!   one row per call are **identity lookups**; the others read the whole
//!   collection.
//!
//! Where the inference can be wrong, it errs toward claiming less for
//! identity lookups and whole-collection reads: an over-counted filter takes
//! from the unfiltered reads, and an unfiltered grouping returning one row
//! counts as a lookup. A query answered by a materialized view counts as a
//! scan.

use adabt_core::index_kind::IndexKind;
use adabt_telemetry::{OpKind, Snapshot};

/// Recent operations, sorted by the kind of work they asked for.
///
/// Counts decay with telemetry, so this describes the workload as it is now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WorkloadMix {
    /// Every recent operation, writes included: the denominator of every
    /// share, because a write is a call a read optimization cannot speed up.
    pub operations: u64,
    /// Reads of one record by its identity.
    pub identity: u64,
    /// Filtered reads that no index or cache answered, so scanned.
    pub filtered_scans: u64,
    /// Unfiltered reads of more than one record: whole-collection scans and
    /// groupings.
    pub unfiltered_scans: u64,
    /// Filtered reads an index served and the result cache did not answer:
    /// each found its matches in the index and then fetched every one of
    /// them by identity.
    pub index_fetches: u64,
    /// Reads that constrained a field to a range.
    pub ranges: u64,
    /// Reads that reached stored records, which is every read the result
    /// cache did not answer: what a change to how records are stored, such
    /// as compressing them, makes cheaper or dearer.
    pub record_reads: u64,
}

impl WorkloadMix {
    /// The mix in `t`, judging each field's filters by its usage alone.
    pub fn of(t: &Snapshot) -> Self {
        Self::with_indexes(t, &[])
    }

    /// The mix in `t`, given the indexes that exist now.
    ///
    /// A field the planner has chosen an index for recently has all its
    /// filters counted as served, not only the decayed share recorded since
    /// the index arrived. Usage history lags a new index by the several
    /// cycles telemetry takes to forget, and an index fresh from an
    /// experiment, hidden from the planner until promotion, leaves behind a
    /// history of nothing but scans. On the soak that history admitted a
    /// column store the cycle after each index was promoted, and the index
    /// had made it useless by the time it was built.
    pub fn with_indexes(t: &Snapshot, existing: &[(String, String, IndexKind)]) -> Self {
        let operations = t.total_calls();
        let reads: u64 = t
            .per_op
            .iter()
            .filter(|(k, _)| !k.is_write())
            .map(|(_, s)| s.calls)
            .sum();

        let filtered = t.field_filters.values().sum::<u64>().min(reads);
        let served: u64 = t
            .field_filters
            .iter()
            .map(|((c, f), n)| {
                let used = t.index_use_count(c, f);
                let indexed = existing.iter().any(|(ec, ef, _)| ec == c && ef == f);
                if indexed && used > 0 {
                    *n
                } else {
                    used.min(*n)
                }
            })
            .sum();
        let cache_answered = t.cache_hits.get("result_cache").copied().unwrap_or(0);
        // The filtered reads the cache answers, as its hit rate over the reads
        // that reached it, applied to every filtered read. Not the raw hit
        // count: while an experiment runs, queries on its collection bypass
        // the cache without probing it, so for the cycles telemetry takes to
        // forget, the count says the cache answered far fewer reads than it
        // does. On the soak that history claimed the range phase's indexed
        // reads fetched by identity 80% of the time, not 55%, and admitted a
        // direct array that was retracted again within the phase. The rate
        // is unbiased by the bypass, because a bypassed query is not a probe.
        let probes = t.probes("result_cache");
        let cache_answered_filtered = if probes == 0 {
            0
        } else {
            (filtered as f64 * cache_answered.min(probes) as f64 / probes as f64).round() as u64
        };
        let ranges = t
            .field_filters
            .keys()
            .map(|(c, f)| t.range_count(c, f))
            .sum::<u64>()
            .min(filtered);

        let unfiltered = reads - filtered;
        let single_row: u64 = t.per_op.get(&OpKind::Get).map_or(0, |s| s.calls)
            + t.per_shape
                .values()
                .filter(|s| s.rows <= s.calls)
                .map(|s| s.calls)
                .sum::<u64>();
        let identity = unfiltered.min(single_row);

        Self {
            operations,
            identity,
            filtered_scans: filtered.saturating_sub(served + cache_answered_filtered),
            index_fetches: served.min(filtered).saturating_sub(cache_answered_filtered),
            unfiltered_scans: unfiltered - identity,
            ranges,
            record_reads: reads - cache_answered.min(reads),
        }
    }

    fn share(&self, n: u64) -> f64 {
        if self.operations == 0 {
            0.0
        } else {
            (n as f64 / self.operations as f64).min(1.0)
        }
    }

    /// Share of operations that looked up one record by identity.
    pub fn identity_share(&self) -> f64 {
        self.share(self.identity)
    }

    /// Share of operations that fetched records by identity: lookups, and
    /// reads an index served, which fetch each match by its id.
    pub fn id_fetch_share(&self) -> f64 {
        self.share(self.identity + self.index_fetches)
    }

    /// Share of operations that read through a collection rather than into
    /// it, filtered or not.
    pub fn scan_share(&self) -> f64 {
        self.share(self.filtered_scans + self.unfiltered_scans)
    }

    /// Share of operations that read a whole collection with no filter.
    pub fn unfiltered_scan_share(&self) -> f64 {
        self.share(self.unfiltered_scans)
    }

    /// Share of operations that constrained a field to a range.
    pub fn range_share(&self) -> f64 {
        self.share(self.ranges)
    }

    /// Share of operations that read stored records, by any path but the
    /// result cache.
    pub fn record_read_share(&self) -> f64 {
        self.share(self.record_reads)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use adabt_telemetry::event::{Event, QueryShape};
    use adabt_telemetry::{CollectingProbe, Probe};

    /// One query of `shape` returning `rows`, filtered on `filters` (field,
    /// equality), the first of which an index served if `indexed`.
    fn query(p: &CollectingProbe, shape: u64, rows: u64, filters: &[(&str, bool)], indexed: bool) {
        for (field, equality) in filters {
            p.record(Event::FieldFiltered {
                collection: "users",
                field,
                equality: *equality,
            });
        }
        if indexed {
            p.record(Event::IndexUsed {
                collection: "users",
                field: filters[0].0,
            });
        }
        p.record(Event::Op {
            collection: "users",
            kind: OpKind::Scan,
            shape: QueryShape(shape),
            nanos: 1_000,
            rows,
        });
    }

    #[test]
    fn identity_lookups_are_not_scans() {
        let p = CollectingProbe::new();
        for _ in 0..100 {
            query(&p, 1, 1, &[], false);
        }
        let mix = WorkloadMix::of(&p.snapshot());
        assert_eq!(mix.identity_share(), 1.0);
        assert_eq!(mix.scan_share(), 0.0);
        assert_eq!(mix.range_share(), 0.0);
    }

    #[test]
    fn an_unindexed_filter_scans_and_an_indexed_one_does_not() {
        let p = CollectingProbe::new();
        for _ in 0..60 {
            query(&p, 1, 20, &[("country", true)], false);
        }
        for _ in 0..40 {
            query(&p, 2, 50, &[("age", false)], true);
        }
        let mix = WorkloadMix::of(&p.snapshot());
        assert!((mix.scan_share() - 0.6).abs() < 1e-9, "{mix:?}");
        assert!((mix.range_share() - 0.4).abs() < 1e-9, "{mix:?}");
        assert_eq!(mix.identity_share(), 0.0);
    }

    #[test]
    fn every_read_but_a_cache_hit_reads_records_and_writes_do_not() {
        let p = CollectingProbe::new();
        for i in 0..80 {
            query(&p, 1, 20, &[("country", true)], i % 2 == 0);
            p.record(Event::CacheProbe {
                name: "result_cache",
                hit: i < 20,
            });
        }
        for _ in 0..20 {
            p.record(Event::Op {
                collection: "users",
                kind: OpKind::Insert,
                shape: QueryShape::UNKNOWN,
                nanos: 1_000,
                rows: 1,
            });
        }
        let mix = WorkloadMix::of(&p.snapshot());
        assert!((mix.record_read_share() - 0.6).abs() < 1e-9, "{mix:?}");
    }

    #[test]
    fn a_query_the_cache_answered_did_not_scan() {
        let p = CollectingProbe::new();
        for i in 0..100 {
            query(&p, 1, 20, &[("country", true)], false);
            p.record(Event::CacheProbe {
                name: "result_cache",
                hit: i < 90,
            });
        }
        let mix = WorkloadMix::of(&p.snapshot());
        assert!((mix.scan_share() - 0.1).abs() < 1e-9, "{mix:?}");
    }

    #[test]
    fn an_unfiltered_grouping_is_a_scan_and_writes_dilute_every_share() {
        let p = CollectingProbe::new();
        for _ in 0..50 {
            query(&p, 3, 8, &[], false);
            p.record(Event::Op {
                collection: "users",
                kind: OpKind::Insert,
                shape: QueryShape::UNKNOWN,
                nanos: 1_000,
                rows: 1,
            });
        }
        let mix = WorkloadMix::of(&p.snapshot());
        assert!((mix.unfiltered_scan_share() - 0.5).abs() < 1e-9, "{mix:?}");
        assert!((mix.scan_share() - 0.5).abs() < 1e-9, "{mix:?}");
        assert_eq!(mix.identity_share(), 0.0);
    }

    #[test]
    fn a_field_whose_index_the_planner_chooses_is_served_before_its_history_catches_up() {
        // Ninety scans recorded while the index was hidden in an experiment,
        // ten served since it was promoted.
        let p = CollectingProbe::new();
        for i in 0..100 {
            query(&p, 1, 20, &[("country", true)], i >= 90);
        }
        let snap = p.snapshot();
        assert!(WorkloadMix::of(&snap).scan_share() > 0.8);
        let existing = [("users".into(), "country".into(), IndexKind::Hash)];
        assert_eq!(
            WorkloadMix::with_indexes(&snap, &existing).scan_share(),
            0.0
        );
    }

    #[test]
    fn an_index_the_planner_never_chooses_serves_nothing() {
        // A hash index on a range-filtered field exists, and is never used.
        let p = CollectingProbe::new();
        for _ in 0..100 {
            query(&p, 1, 20, &[("age", false)], false);
        }
        let existing = [("users".into(), "age".into(), IndexKind::Hash)];
        assert_eq!(
            WorkloadMix::with_indexes(&p.snapshot(), &existing).scan_share(),
            1.0
        );
    }

    #[test]
    fn reads_an_index_served_fetch_by_identity_unless_the_cache_answered_them() {
        // The expert comparison's range phase: every read filtered on an
        // indexed field, two in five answered by the result cache.
        let p = CollectingProbe::new();
        for i in 0..100 {
            query(&p, 1, 50, &[("age", false)], i >= 40);
            p.record(Event::CacheProbe {
                name: "result_cache",
                hit: i < 40,
            });
        }
        let existing = [("users".into(), "age".into(), IndexKind::BTree)];
        let mix = WorkloadMix::with_indexes(&p.snapshot(), &existing);
        assert!((mix.id_fetch_share() - 0.6).abs() < 1e-9, "{mix:?}");
        assert_eq!(mix.identity_share(), 0.0);
        assert_eq!(mix.scan_share(), 0.0);
    }

    #[test]
    fn reads_that_bypassed_the_cache_do_not_hide_what_it_answers() {
        // Sixty indexed range reads during an experiment, which bypass the
        // cache without probing it, then forty that reach it, half answered.
        let p = CollectingProbe::new();
        for i in 0..100 {
            query(&p, 1, 50, &[("age", false)], true);
            if i >= 60 {
                p.record(Event::CacheProbe {
                    name: "result_cache",
                    hit: i % 2 == 0,
                });
            }
        }
        let existing = [("users".into(), "age".into(), IndexKind::BTree)];
        let mix = WorkloadMix::with_indexes(&p.snapshot(), &existing);
        assert!((mix.id_fetch_share() - 0.5).abs() < 1e-9, "{mix:?}");
    }

    #[test]
    fn an_unindexed_filter_fetches_nothing_by_identity() {
        let p = CollectingProbe::new();
        for _ in 0..100 {
            query(&p, 1, 20, &[("country", true)], false);
        }
        assert_eq!(WorkloadMix::of(&p.snapshot()).id_fetch_share(), 0.0);
    }

    #[test]
    fn no_operations_means_no_share_of_anything() {
        let mix = WorkloadMix::of(&Snapshot::default());
        assert_eq!(mix.identity_share(), 0.0);
        assert_eq!(mix.scan_share(), 0.0);
    }
}
