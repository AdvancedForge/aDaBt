//! Page I/O optimizations: how many pages stay resident (`buffer_pool`) and
//! whether sequential access reads ahead (`prefetch`).

use adabt_core::policy::GuaranteeRequirements;
use adabt_opt::action::{Action, ChangePlan};
use adabt_opt::config::Params;
use adabt_opt::cost::{AxisEffects, CostEstimate};
use adabt_opt::optimization::{
    Applicability, OptContext, OptMeta, Optimization, Reversibility, ScopeKind,
};

// -- buffer pool ------------------------------------------------------------

pub struct BufferPoolOpt;

const BUFFER_POOL_META: OptMeta = OptMeta {
    name: "buffer_pool",
    summary: "hold more pages in memory",
    scope_kind: ScopeKind::Global,
    min_level: 3,
    axis_effects: AxisEffects::new(4, -6, 0),
    requires_guarantees: GuaranteeRequirements::ANY,
    prerequisites: &[],
    conflicts_with: &[],
    reversibility: Reversibility::Instant,
};

const PAGE_BYTES: i64 = 8192;
const DEFAULT_POOL_PAGES: usize = 1024;

impl Optimization for BufferPoolOpt {
    fn meta(&self) -> &OptMeta {
        &BUFFER_POOL_META
    }
    fn applicability(&self, _ctx: &OptContext<'_>) -> Applicability {
        Applicability::Applicable
    }
    fn estimate(&self, ctx: &OptContext<'_>) -> CostEstimate {
        let pages = pool_pages(ctx);
        CostEstimate::faster(0.8, 0.7)
            .with_ram((pages as i64 - DEFAULT_POOL_PAGES as i64) * PAGE_BYTES)
            .with_confidence(0.5)
    }
    fn plan_enable(&self, ctx: &OptContext<'_>, _scope: &str, _params: &Params) -> ChangePlan {
        ChangePlan::new(
            vec![Action::SetBufferPoolPages(pool_pages(ctx))],
            vec![Action::SetBufferPoolPages(DEFAULT_POOL_PAGES)],
        )
    }
    fn plan_disable(&self, _ctx: &OptContext<'_>, _scope: &str, _params: &Params) -> ChangePlan {
        ChangePlan::new(vec![Action::SetBufferPoolPages(DEFAULT_POOL_PAGES)], vec![])
    }
}

// -- read-ahead -------------------------------------------------------------

pub struct PrefetchOpt;

const PREFETCH_META: OptMeta = OptMeta {
    name: "prefetch",
    summary: "read ahead when page access turns out to be sequential",
    scope_kind: ScopeKind::Global,
    min_level: 3,
    // Costs almost nothing in memory: read-ahead only fills frames the pool was
    // not using, so its resource effect is a rounding error rather than a trade.
    axis_effects: AxisEffects::new(3, -1, 0),
    requires_guarantees: GuaranteeRequirements::ANY,
    prerequisites: &[],
    conflicts_with: &[],
    reversibility: Reversibility::Instant,
};

/// Below this a collection fits in a handful of pages and a scan is one read
/// whether or not anybody reads ahead.
const MIN_ROWS_FOR_PREFETCH: usize = 5_000;

impl Optimization for PrefetchOpt {
    fn meta(&self) -> &OptMeta {
        &PREFETCH_META
    }

    /// Applicable when there is enough data for a scan to span pages.
    ///
    /// Note what this does *not* check: whether the workload scans. That is a
    /// question about whether read-ahead is worth having, which is what the
    /// estimate and the score are for — applicability answers only whether it is
    /// possible, and a database whose current traffic happens to be point
    /// lookups must not thereby veto an optimization it may want an hour later.
    fn applicability(&self, ctx: &OptContext<'_>) -> Applicability {
        let biggest = ctx.collections.iter().map(|(_, n)| *n).max().unwrap_or(0);
        if biggest < MIN_ROWS_FOR_PREFETCH {
            return Applicability::NotYet(format!(
                "the largest collection holds {biggest} rows, below the \
                 {MIN_ROWS_FOR_PREFETCH} at which reading ahead pays"
            ));
        }
        Applicability::Applicable
    }

    fn estimate(&self, ctx: &OptContext<'_>) -> CostEstimate {
        // Worth more the more of the workload is scans. Sixteen pages per read
        // instead of one is a large reduction in syscalls, but only for the
        // queries that walk pages in order, so the gain is claimed on the share
        // that scans. It used to be claimed on every read, which had a
        // database answering only identity lookups turn read-ahead on.
        let scans = ctx.workload().scan_share();
        let gain = 1.0 - 0.25 * scans;
        CostEstimate::faster(gain, gain)
            .with_ram(0)
            .with_confidence(0.4)
    }

    fn plan_enable(&self, _ctx: &OptContext<'_>, _scope: &str, _params: &Params) -> ChangePlan {
        ChangePlan::new(
            vec![Action::SetPrefetch(true)],
            vec![Action::SetPrefetch(false)],
        )
    }

    fn plan_disable(&self, _ctx: &OptContext<'_>, _scope: &str, _params: &Params) -> ChangePlan {
        ChangePlan::new(
            vec![Action::SetPrefetch(false)],
            vec![Action::SetPrefetch(true)],
        )
    }
}

fn pool_pages(ctx: &OptContext<'_>) -> usize {
    let _ = ctx;
    8192
}
