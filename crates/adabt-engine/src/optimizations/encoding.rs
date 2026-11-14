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
        // Whether a particular dataset compresses is a property of the data,
        // not of the algorithm, so confidence here is deliberately low: this is
        // a candidate for *measurement*, which is what shadow trials are for.
        let mut e = CostEstimate::faster(1.05, 1.08);
        e.storage_bytes = -(rows as i64 * 48);
        e.ram_bytes = -(rows as i64 * 16);
        e.cpu_frac = 0.08;
        e.io_ops = -(rows as i64 / 100);
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
