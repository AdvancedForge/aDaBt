//! Optimizations that change how stored data is represented: compressed
//! records, a columnar copy, and delta-encoded integer columns.

use adabt_core::policy::GuaranteeRequirements;
use adabt_opt::action::{Action, ChangePlan};
use adabt_opt::config::Params;
use adabt_opt::cost::{AxisEffects, BuildCost, CostEstimate};
use adabt_opt::optimization::{
    Applicability, OptContext, OptMeta, Optimization, Reversibility, ScopeKind,
};

// -- record compression -----------------------------------------------------

pub struct RecordCompressionOpt;

const RECORD_COMPRESSION_META: OptMeta = OptMeta {
    name: "record_compression",
    summary: "compress stored records, trading CPU for storage and residency",
    scope_kind: ScopeKind::Global,
    min_level: 2,
    // The first optimization with a *positive* resources effect. Every other
    // one spends memory to buy latency; without at least one trading the other
    // way, a resources-priority policy has nothing to select and a third of the
    // premise goes untested.
    axis_effects: AxisEffects::new(-1, 6, 0),
    requires_guarantees: GuaranteeRequirements::ANY,
    prerequisites: &[],
    conflicts_with: &[],
    // Turning it off is instant, but the records already written stay
    // compressed until something rewrites them.
    reversibility: Reversibility::RebuildRequired,
};

impl Optimization for RecordCompressionOpt {
    fn meta(&self) -> &OptMeta {
        &RECORD_COMPRESSION_META
    }

    fn applicability(&self, ctx: &OptContext<'_>) -> Applicability {
        let rows: usize = ctx.collections.iter().map(|(_, n)| n).sum();
        if rows < MIN_ROWS_FOR_COMPRESSION {
            return Applicability::NotYet(format!(
                "only {rows} rows stored; compression's CPU cost is not worth it below {MIN_ROWS_FOR_COMPRESSION}"
            ));
        }
        Applicability::Applicable
    }

    fn estimate(&self, ctx: &OptContext<'_>) -> CostEstimate {
        let rows: usize = ctx.collections.iter().map(|(_, n)| n).sum();
        let (raw, saved) = compression_saving(ctx);
        // Decoding is paid on every read that reaches a stored record, so the
        // CPU and latency cost scale with the share of traffic doing that,
        // the way other estimates scale their gain by the share they help.
        // With no traffic seen yet there is no evidence of a cheap workload,
        // so the full cost is assumed rather than none.
        let mix = ctx.workload();
        let reads = if mix.operations == 0 {
            1.0
        } else {
            mix.record_read_share()
        };
        // Whether a particular dataset compresses is a property of the data,
        // not of the algorithm, so confidence here is deliberately low: this is
        // a candidate for *measurement*, which is what shadow trials are for.
        // The ratio behind `saved` is measured when the engine reports one,
        // but the CPU it costs is not.
        let mut e = CostEstimate::faster(1.0 + 0.05 * reads, 1.0 + 0.08 * reads);
        // The saving is counted once: the bytes leave the stored records, and
        // a quarter of them is credited as buffer-pool residency, the share
        // the old fixed figures (48 and 16 bytes a row) gave it. Counting the
        // whole saving on both would claim twice what is freed.
        e.storage_bytes = -((saved * 0.75) as i64);
        e.ram_bytes = -((saved * 0.25) as i64);
        e.cpu_frac = 0.08 * reads;
        let shrink = if raw > 0.0 { saved / raw } else { 0.0 };
        e.io_ops = -((rows as f64 / 100.0 * shrink) as i64);
        e.with_maintenance(0.06)
            .with_confidence(0.3)
            .with_build(BuildCost {
                estimated_secs: rows as f64 / 5e5,
                rows_read: rows as u64,
                online: false,
            })
    }

    fn plan_enable(&self, _ctx: &OptContext<'_>, _scope: &str, _params: &Params) -> ChangePlan {
        ChangePlan::new(
            vec![Action::SetRecordCompression(true)],
            vec![Action::SetRecordCompression(false)],
        )
    }

    fn plan_disable(&self, _ctx: &OptContext<'_>, _scope: &str, _params: &Params) -> ChangePlan {
        ChangePlan::new(vec![Action::SetRecordCompression(false)], vec![])
    }
}

/// Below this the CPU spent compressing outweighs the bytes saved.
const MIN_ROWS_FOR_COMPRESSION: usize = 500;

/// Record size assumed for a collection whose size the engine did not report.
const ASSUMED_RECORD_BYTES: f64 = 96.0;

/// Compressed over uncompressed size assumed where none was measured: a
/// saving a little above the 12% below which the store keeps a record raw.
const ASSUMED_COMPRESSION_RATIO: f64 = 0.7;

/// What the records take uncompressed, and how many of those bytes
/// compression would free, summed over every collection.
///
/// Built on the uncompressed size and the measured ratio, both reported the
/// same whether compression is on or off, so the claim does not shrink once
/// the saving is made. It used to be a fixed 64 bytes a row: more than the
/// expert comparison's records occupy (about 56 bytes, too short for the
/// store to compress at all), so it claimed to free the whole database.
fn compression_saving(ctx: &OptContext<'_>) -> (f64, f64) {
    let (mut raw, mut saved) = (0.0, 0.0);
    for (name, rows) in ctx.collections {
        let (bytes, ratio) = match ctx.record_bytes_of(name) {
            Some(b) => (
                b.raw as f64,
                b.compression_ratio.unwrap_or(ASSUMED_COMPRESSION_RATIO),
            ),
            None => (
                *rows as f64 * ASSUMED_RECORD_BYTES,
                ASSUMED_COMPRESSION_RATIO,
            ),
        };
        raw += bytes;
        saved += bytes * (1.0 - ratio).clamp(0.0, 1.0);
    }
    (raw, saved)
}

// -- column store -----------------------------------------------------------

pub struct ColumnStoreOpt;

const COLUMN_STORE_META: OptMeta = OptMeta {
    name: "column_store",
    summary: "keep a columnar copy for scans and aggregates over few fields",
    scope_kind: ScopeKind::PerCollection,
    min_level: 4,
    // Costs memory for a second copy, but dictionary encoding makes that copy
    // small for low-cardinality text, and reading two columns instead of twenty
    // fields is a large win on aggregates.
    axis_effects: AxisEffects::new(6, -3, 0),
    requires_guarantees: GuaranteeRequirements::ANY,
    prerequisites: &[],
    conflicts_with: &[],
    reversibility: Reversibility::RebuildRequired,
};

/// Below this a scan is cheap enough that a second copy is not worth its memory.
const MIN_ROWS_FOR_COLUMNS: usize = 2_000;

impl Optimization for ColumnStoreOpt {
    fn meta(&self) -> &OptMeta {
        &COLUMN_STORE_META
    }

    fn applicability(&self, ctx: &OptContext<'_>) -> Applicability {
        let biggest = ctx.collections.iter().map(|(_, n)| *n).max().unwrap_or(0);
        if biggest < MIN_ROWS_FOR_COLUMNS {
            return Applicability::NotYet(format!(
                "largest collection holds {biggest} rows, below the {MIN_ROWS_FOR_COLUMNS} at which a second copy pays"
            ));
        }
        // A columnar copy suits read-heavy workloads far better, because it
        // cannot be updated in place. That is a judgment about *worth*, not
        // possibility, so it lives in the estimate below — see the note on
        // `Optimization::applicability`.
        Applicability::Applicable
    }

    fn estimate(&self, ctx: &OptContext<'_>) -> CostEstimate {
        let rows: usize = ctx.collections.iter().map(|(_, n)| n).sum();
        // The planner reads columns only for a query no index can serve, so
        // the gain is claimed on the share of the workload that scans and on
        // nothing else: an identity lookup or an indexed filter never touches
        // the copy. Writes count against that share as well as through the
        // upkeep below, since they tombstone columnar rows.
        let scans = ctx.workload().scan_share();
        CostEstimate::faster(1.0 - 0.5 * scans, 1.0 - 0.55 * scans)
            .with_ram(rows as i64 * 24)
            .with_maintenance(0.12 + 0.3 * ctx.telemetry.write_fraction())
            .with_confidence(0.4)
            .with_build(BuildCost {
                estimated_secs: rows as f64 / 1e6,
                rows_read: rows as u64,
                online: true,
            })
    }

    fn plan_enable(&self, _ctx: &OptContext<'_>, _scope: &str, _params: &Params) -> ChangePlan {
        ChangePlan::new(
            vec![Action::SetColumnStore(true)],
            vec![Action::SetColumnStore(false)],
        )
    }

    fn plan_disable(&self, _ctx: &OptContext<'_>, _scope: &str, _params: &Params) -> ChangePlan {
        ChangePlan::new(vec![Action::SetColumnStore(false)], vec![])
    }
}

// -- delta encoding ---------------------------------------------------------

pub struct DeltaEncodingOpt;

const DELTA_ENCODING_META: OptMeta = OptMeta {
    name: "delta_encoding",
    summary: "use delta-varint for sorted integer columns (block directory bounded access)",
    scope_kind: ScopeKind::Global,
    min_level: 4,
    axis_effects: AxisEffects::new(3, -1, 0),
    requires_guarantees: GuaranteeRequirements::ANY,
    prerequisites: &[],
    conflicts_with: &[],
    reversibility: Reversibility::RebuildRequired,
};

impl Optimization for DeltaEncodingOpt {
    fn meta(&self) -> &OptMeta {
        &DELTA_ENCODING_META
    }

    fn applicability(&self, ctx: &OptContext<'_>) -> Applicability {
        let biggest = ctx.collections.iter().map(|(_, n)| *n).max().unwrap_or(0);
        if biggest < 2_000 {
            return Applicability::NotYet(format!(
                "largest collection holds {biggest} rows, below the 2,000 at which delta pays"
            ));
        }
        if ctx.collections.is_empty() {
            return Applicability::NotYet("no collections yet".to_string());
        }
        Applicability::Applicable
    }

    fn estimate(&self, ctx: &OptContext<'_>) -> CostEstimate {
        let rows: usize = ctx.collections.iter().map(|(_, n)| n).sum();
        // Decoding sorted integer columns faster helps the queries that read
        // through them, which are scans; a lookup decodes one record either
        // way. The memory saving does not depend on the traffic.
        let scans = ctx.workload().scan_share();
        CostEstimate::faster(1.0 - 0.1 * scans, 1.0 - 0.1 * scans)
            .with_ram(-(rows as i64 * 8))
            .with_confidence(0.5)
            .with_build(BuildCost {
                estimated_secs: rows as f64 / 1e6,
                rows_read: rows as u64,
                online: true,
            })
    }

    fn plan_enable(&self, _ctx: &OptContext<'_>, _scope: &str, _params: &Params) -> ChangePlan {
        ChangePlan::new(
            vec![Action::SetDeltaEncoding(true)],
            vec![Action::SetDeltaEncoding(false)],
        )
    }

    fn plan_disable(&self, _ctx: &OptContext<'_>, _scope: &str, _params: &Params) -> ChangePlan {
        ChangePlan::new(vec![Action::SetDeltaEncoding(false)], vec![])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::optimizations::tests::Fx;
    use adabt_opt::optimization::RecordBytes;
    use adabt_telemetry::event::{Event, OpKind, QueryShape};
    use adabt_telemetry::{CollectingProbe, Probe as _};

    #[test]
    fn a_resource_saver_reports_negative_storage_and_positive_cpu() {
        let mut fx = Fx::new();
        fx.collections = vec![("users".into(), 100_000)];
        let e = RecordCompressionOpt.estimate(&fx.ctx());
        assert!(e.storage_bytes < 0, "compression should reduce storage");
        assert!(e.ram_bytes < 0, "fewer pages should reduce residency");
        assert!(e.cpu_frac > 0.0, "compression should cost CPU");
        assert!(
            !e.helps_latency(),
            "compression is not a latency optimization"
        );
    }

    #[test]
    fn compression_waits_until_there_is_enough_data_to_be_worth_it() {
        let mut fx = Fx::new();
        fx.collections = vec![("users".into(), 10)];
        let a = RecordCompressionOpt.applicability(&fx.ctx());
        assert!(a.reason().unwrap().contains("not worth it"), "{a:?}");

        fx.collections = vec![("users".into(), 100_000)];
        assert!(RecordCompressionOpt
            .applicability(&fx.ctx())
            .is_applicable());
    }

    fn measured(raw: u64, ratio: f64) -> Vec<(String, RecordBytes)> {
        vec![(
            "users".into(),
            RecordBytes {
                raw,
                compression_ratio: Some(ratio),
            },
        )]
    }

    #[test]
    fn records_that_do_not_compress_are_claimed_to_save_nothing() {
        // The expert comparison's records: about 43 bytes each, below the
        // size at which the store compresses anything. The fixed estimate
        // claimed 64 bytes a row of them, more than the database held.
        let mut fx = Fx::new();
        fx.collections = vec![("users".into(), 20_000)];
        fx.record_bytes = measured(20_000 * 43, 1.0);
        fx.current_bytes = 20_000 * 56;
        let e = RecordCompressionOpt.estimate(&fx.ctx());
        assert_eq!(e.storage_bytes + e.ram_bytes, 0, "{e:?}");
        let s = adabt_opt::score::score_against(
            &RECORD_COMPRESSION_META.axis_effects,
            &e,
            &fx.policy,
            fx.current_bytes,
        );
        assert!(s.total < 0.0, "a saving of nothing is not worth CPU: {s:?}");
    }

    #[test]
    fn the_claim_is_what_the_measured_ratio_frees_and_no_more() {
        let mut fx = Fx::new();
        fx.collections = vec![("users".into(), 10_000)];
        fx.record_bytes = measured(1_300_000, 0.3);
        let e = RecordCompressionOpt.estimate(&fx.ctx());
        let claimed = -(e.storage_bytes + e.ram_bytes);
        assert!((claimed - 910_000).abs() <= 2, "{claimed}");
    }

    #[test]
    fn the_claim_does_not_shrink_once_compression_has_shrunk_the_store() {
        // Off, the store holds the raw records; on, about a third of them.
        // The uncompressed size and the ratio are reported the same either
        // way, so the bytes claimed must be too, or the change argues for
        // itself before it is made and against itself after.
        let mut fx = Fx::new();
        fx.collections = vec![("users".into(), 10_000)];
        fx.record_bytes = measured(1_300_000, 0.3);
        fx.current_bytes = 1_430_000;
        let off = RecordCompressionOpt.estimate(&fx.ctx());
        fx.current_bytes = 520_000;
        let on = RecordCompressionOpt.estimate(&fx.ctx());
        assert_eq!(off.storage_bytes, on.storage_bytes);
        assert_eq!(off.ram_bytes, on.ram_bytes);
    }

    #[test]
    fn decoding_costs_only_on_the_reads_that_reach_records() {
        let mut fx = Fx::new();
        fx.collections = vec![("users".into(), 10_000)];
        fx.record_bytes = measured(1_300_000, 0.3);
        fx.scan_traffic(1_000);
        let reading = RecordCompressionOpt.estimate(&fx.ctx());

        // The same traffic, nine in ten answered by the result cache.
        let probe = CollectingProbe::new();
        for i in 0..1_000 {
            probe.record(Event::Op {
                collection: "users",
                kind: OpKind::Scan,
                shape: QueryShape(2),
                nanos: 1_000,
                rows: 12,
            });
            probe.record(Event::CacheProbe {
                name: "result_cache",
                hit: i % 10 != 0,
            });
        }
        fx.snap = probe.snapshot();
        let cached = RecordCompressionOpt.estimate(&fx.ctx());

        assert!(cached.cpu_frac < reading.cpu_frac / 5.0, "{cached:?}");
        assert!(cached.p99_delta.0 < reading.p99_delta.0);
        assert_eq!(
            cached.storage_bytes, reading.storage_bytes,
            "the saving does not depend on the traffic"
        );
    }

    #[test]
    fn its_confidence_is_low_because_compressibility_depends_on_the_data() {
        let mut fx = Fx::new();
        fx.collections = vec![("users".into(), 100_000)];
        let e = RecordCompressionOpt.estimate(&fx.ctx());
        assert!(
            e.confidence < 0.5,
            "an estimate that cannot know the data should not be confident"
        );
    }
}
