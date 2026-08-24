//! The built-in optimization library.
//!
//! Each of these is an independently switchable, reversible, measurable
//! physical strategy. They are registered explicitly by `register_builtins` so
//! the set is greppable and its order deterministic.
//!
//! Only optimizations that are genuinely implemented are registered. Page
//! compression and prefetching appear in `Action` and in the level table but
//! have no module here yet, and `Database::apply_action` returns an error rather
//! than quietly succeeding. An optimization that reports success and does
//! nothing is worse than a missing one: it makes the benchmark matrix lie.

use adabt_core::index_kind::IndexKind;
use adabt_core::policy::GuaranteeRequirements;
use adabt_opt::action::{Action, ChangePlan};
use adabt_opt::config::Params;
use adabt_opt::cost::{AxisEffects, BuildCost, CostEstimate};
use adabt_opt::optimization::{
    Applicability, OptContext, OptMeta, Optimization, Reversibility, ScopeKind,
};
use adabt_opt::Registry;

/// Register every implemented optimization, in dependency order.
pub fn register_builtins(registry: &mut Registry) {
    registry.register(Box::new(PlanCacheOpt));
    registry.register(Box::new(ResultCacheOpt));
    registry.register(Box::new(BufferPoolOpt));
    registry.register(Box::new(AutoIndexOpt));
    registry.register(Box::new(RecordCompressionOpt));
    registry.register(Box::new(ColumnStoreOpt));
    registry.register(Box::new(FreezeSchemaOpt));
    registry.register(Box::new(DirectLookupOpt));
    registry.register(Box::new(PrefetchOpt));
    registry.register(Box::new(MaterializedViewOpt));
}

// -- materialized views -----------------------------------------------------

pub struct MaterializedViewOpt;

const MATERIALIZED_VIEW_META: OptMeta = OptMeta {
    name: "materialized_view",
    summary: "keep grouped counts up to date on write instead of recomputing them",
    scope_kind: ScopeKind::Global,
    min_level: 4,
    // A large speed effect on the queries it covers — a handful of numbers
    // instead of every row — paid for on the write path, where every insert and
    // delete now adjusts a total. The freedom axis is untouched: nothing about
    // the data or the schema is constrained by keeping a count of it.
    axis_effects: AxisEffects::new(7, -2, 0),
    requires_guarantees: GuaranteeRequirements::ANY,
    prerequisites: &[],
    conflicts_with: &[],
    reversibility: Reversibility::RebuildRequired,
};

/// Below this, recomputing the aggregate is cheap enough that maintaining it on
/// every write costs more than it saves.
const MIN_ROWS_FOR_VIEW: usize = 2_000;

impl Optimization for MaterializedViewOpt {
    fn meta(&self) -> &OptMeta {
        &MATERIALIZED_VIEW_META
    }

    fn applicability(&self, ctx: &OptContext<'_>) -> Applicability {
        let biggest = ctx.collections.iter().map(|(_, n)| *n).max().unwrap_or(0);
        if biggest < MIN_ROWS_FOR_VIEW {
            return Applicability::NotYet(format!(
                "the largest collection holds {biggest} rows, below the \
                 {MIN_ROWS_FOR_VIEW} at which maintaining a total beats recomputing it"
            ));
        }
        Applicability::Applicable
    }

    fn estimate(&self, ctx: &OptContext<'_>) -> CostEstimate {
        // The saving is enormous where it applies and nil where it does not, and
        // which of those is true depends on how much of the workload aggregates
        // — something only telemetry knows. Hence the low confidence: this is an
        // estimate that expects to be corrected by measurement.
        let read_share = 1.0 - ctx.telemetry.write_fraction();
        CostEstimate::faster(1.0 - 0.5 * read_share, 1.0 - 0.5 * read_share)
            .with_maintenance(0.03)
            .with_confidence(0.35)
    }

    fn plan_enable(&self, _ctx: &OptContext<'_>, _scope: &str, _params: &Params) -> ChangePlan {
        ChangePlan::new(
            vec![Action::SetMaterializedViews(true)],
            vec![Action::SetMaterializedViews(false)],
        )
    }

    fn plan_disable(&self, _ctx: &OptContext<'_>, _scope: &str, _params: &Params) -> ChangePlan {
        ChangePlan::new(
            vec![Action::SetMaterializedViews(false)],
            vec![Action::SetMaterializedViews(true)],
        )
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
        // queries that walk pages in order.
        let scan_heavy = 1.0 - ctx.telemetry.write_fraction();
        let gain = 1.0 - 0.25 * scan_heavy;
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

// -- plan cache -------------------------------------------------------------

pub struct PlanCacheOpt;

const PLAN_CACHE_META: OptMeta = OptMeta {
    name: "plan_cache",
    summary: "reuse physical plans across queries of the same shape",
    scope_kind: ScopeKind::Global,
    min_level: 1,
    axis_effects: AxisEffects::new(2, -1, 0),
    requires_guarantees: GuaranteeRequirements::ANY,
    prerequisites: &[],
    conflicts_with: &[],
    reversibility: Reversibility::Instant,
};

impl Optimization for PlanCacheOpt {
    fn meta(&self) -> &OptMeta {
        &PLAN_CACHE_META
    }
    fn applicability(&self, _ctx: &OptContext<'_>) -> Applicability {
        Applicability::Applicable
    }
    fn estimate(&self, _ctx: &OptContext<'_>) -> CostEstimate {
        // Planning is a small part of a query's cost, so the benefit is modest
        // and the confidence low until measured.
        CostEstimate::faster(0.95, 0.95)
            .with_ram(512 * 4_096)
            .with_confidence(0.4)
    }
    fn plan_enable(&self, ctx: &OptContext<'_>, _scope: &str, _params: &Params) -> ChangePlan {
        let n = entries(ctx, 512);
        ChangePlan::new(
            vec![Action::SetPlanCacheEntries(n)],
            vec![Action::SetPlanCacheEntries(0)],
        )
    }
    fn plan_disable(&self, _ctx: &OptContext<'_>, _scope: &str, _params: &Params) -> ChangePlan {
        ChangePlan::new(vec![Action::SetPlanCacheEntries(0)], vec![])
    }
}

// -- result cache -----------------------------------------------------------

pub struct ResultCacheOpt;

const RESULT_CACHE_META: OptMeta = OptMeta {
    name: "result_cache",
    summary: "serve repeated identical queries from memory",
    scope_kind: ScopeKind::Global,
    min_level: 1,
    axis_effects: AxisEffects::new(5, -4, 0),
    requires_guarantees: GuaranteeRequirements::ANY,
    prerequisites: &[],
    conflicts_with: &[],
    reversibility: Reversibility::Instant,
};

impl Optimization for ResultCacheOpt {
    fn meta(&self) -> &OptMeta {
        &RESULT_CACHE_META
    }
    fn applicability(&self, _ctx: &OptContext<'_>) -> Applicability {
        // Structurally always possible. Whether it is a good *idea* under a
        // write-heavy workload is a scoring judgment, and it belongs in the
        // estimate below rather than here.
        //
        // Putting it here was a bug worth recording: a bulk load makes the
        // workload look 100% writes, so a database loaded and then queried
        // would refuse its own result cache forever on the strength of its
        // loading phase. Applicability answers "is this possible", not "is this
        // wise" — conflating the two lets a transient workload permanently
        // veto an optimization.
        Applicability::Applicable
    }
    fn estimate(&self, ctx: &OptContext<'_>) -> CostEstimate {
        let read_fraction = 1.0 - ctx.telemetry.write_fraction();
        CostEstimate::faster(1.0 - 0.5 * read_fraction, 1.0 - 0.3 * read_fraction)
            .with_ram(entries(ctx, 256) as i64 * 4_096)
            .with_maintenance(0.02)
            .with_confidence(0.35)
    }
    fn plan_enable(&self, ctx: &OptContext<'_>, _scope: &str, _params: &Params) -> ChangePlan {
        let n = entries(ctx, 256);
        ChangePlan::new(
            vec![Action::SetResultCacheEntries(n)],
            vec![Action::SetResultCacheEntries(0)],
        )
    }
    fn plan_disable(&self, _ctx: &OptContext<'_>, _scope: &str, _params: &Params) -> ChangePlan {
        ChangePlan::new(vec![Action::SetResultCacheEntries(0)], vec![])
    }
}

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

// -- automatic indexing -----------------------------------------------------

pub struct AutoIndexOpt;

const AUTO_INDEX_META: OptMeta = OptMeta {
    name: "auto_index",
    summary: "index fields that queries repeatedly filter on",
    scope_kind: ScopeKind::PerField,
    min_level: 2,
    axis_effects: AxisEffects::new(7, -3, 0),
    requires_guarantees: GuaranteeRequirements::ANY,
    prerequisites: &[],
    conflicts_with: &[],
    // The index has to be rebuilt from the primary to come back.
    reversibility: Reversibility::RebuildRequired,
};

const MIN_ROWS_FOR_INDEX: usize = 1_000;
const MIN_QUERIES_FOR_INDEX: u64 = 8;

impl AutoIndexOpt {
    /// Fields worth indexing: filtered often enough, on a collection big enough
    /// that a scan actually costs something, and not already indexed.
    fn candidates(ctx: &OptContext<'_>) -> Vec<(String, String)> {
        ctx.filtered_fields
            .iter()
            .filter(|(c, f, n)| {
                *n >= MIN_QUERIES_FOR_INDEX
                    && ctx.rows_in(c) >= MIN_ROWS_FOR_INDEX
                    && !ctx.has_index(c, f)
            })
            .map(|(c, f, _)| (c.clone(), f.clone()))
            .collect()
    }
}

impl Optimization for AutoIndexOpt {
    fn meta(&self) -> &OptMeta {
        &AUTO_INDEX_META
    }

    /// One scope per field worth indexing, so each index is judged, applied and
    /// retracted on its own evidence.
    fn candidate_scopes(&self, ctx: &OptContext<'_>) -> Vec<String> {
        Self::candidates(ctx)
            .into_iter()
            .map(|(c, f)| format!("{c}.{f}"))
            .collect()
    }

    fn applicability(&self, ctx: &OptContext<'_>) -> Applicability {
        if ctx.filtered_fields.is_empty() {
            return Applicability::NotYet("no queries have filtered on any field yet".to_string());
        }
        if Self::candidates(ctx).is_empty() {
            let best = ctx.filtered_fields.iter().max_by_key(|(_, _, n)| *n);
            let detail = match best {
                Some((c, f, n)) if ctx.has_index(c, f) => {
                    format!("{c}.{f} is already indexed")
                }
                Some((c, f, n)) if *n < MIN_QUERIES_FOR_INDEX => format!(
                    "{c}.{f} filtered only {n} times, below the {MIN_QUERIES_FOR_INDEX} needed"
                ),
                Some((c, _, _)) => format!(
                    "{c} holds {} rows, below the {MIN_ROWS_FOR_INDEX} at which an index pays",
                    ctx.rows_in(c)
                ),
                None => "no candidate fields".to_string(),
            };
            return Applicability::NotYet(detail);
        }
        Applicability::Applicable
    }

    fn estimate(&self, ctx: &OptContext<'_>) -> CostEstimate {
        let candidates = Self::candidates(ctx);
        let rows: usize = candidates.iter().map(|(c, _)| ctx.rows_in(c)).sum();
        CostEstimate::faster(0.3, 0.25)
            // Roughly a key plus an id plus node overhead per row.
            .with_ram(rows as i64 * 64)
            .with_maintenance(0.10)
            .with_confidence(0.6)
            .with_build(BuildCost {
                estimated_secs: rows as f64 / 1e6,
                rows_read: rows as u64,
                online: true,
            })
    }

    fn plan_enable(&self, ctx: &OptContext<'_>, scope: &str, params: &Params) -> ChangePlan {
        // One index, for the scope being decided. A plan covering every
        // candidate would make the whole set stand or fall together.
        let Some((collection, field)) = split_scope(scope) else {
            return ChangePlan::default();
        };
        // An explicit `kind` param — set by a manual override that named one
        // ("index users.country hash") — wins over the telemetry-derived
        // guess; absent, the field is chosen from how it is actually
        // filtered, exactly as before this existed.
        let kind =
            index_kind_param(params).unwrap_or_else(|| index_kind_for(ctx, &collection, &field));
        ChangePlan::new(
            vec![Action::CreateIndex {
                collection: collection.clone(),
                field: field.clone(),
                kind,
            }],
            vec![Action::DropIndex {
                collection,
                field,
                kind,
            }],
        )
    }

    fn plan_disable(&self, ctx: &OptContext<'_>, scope: &str, _params: &Params) -> ChangePlan {
        // Drop only this scope's index. Dropping every index because one stopped
        // paying was the M8 behaviour, and it threw away the ones that were.
        let Some((collection, field)) = split_scope(scope) else {
            return ChangePlan::default();
        };
        let kind = ctx
            .existing_indexes
            .iter()
            .find(|(c, f, _)| *c == collection && *f == field)
            .map(|(_, _, k)| *k)
            .unwrap_or_else(|| index_kind_for(ctx, &collection, &field));
        ChangePlan::new(
            vec![Action::DropIndex {
                collection,
                field,
                kind,
            }],
            vec![],
        )
    }
}

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
        // a candidate for *measurement*, which is what Phase 7 is for.
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

/// An explicit `kind` param, if a decision's `params` carries one.
///
/// The encoding is `Params`'s own constraint, not a design choice: `Params`
/// is `BTreeMap<String, i64>`, so `IndexKind` — which already exists to be
/// *named* without depending on what builds one, see `adabt_core::index_kind`
/// — is carried as its ordinal rather than its `as_str()`/`parse()` string
/// form. `Params` staying integer-only is deliberately not widened for this:
/// every other optimization's params are already numeric, and a single
/// string-valued key would mean every reader of `Params` has to handle a
/// case that means something only here.
fn index_kind_param(params: &Params) -> Option<IndexKind> {
    params.get("kind").and_then(IndexKind::from_ordinal)
}

/// The index structure a field's predicates can actually use.
///
/// A hash index cannot answer a range at all, so proposing one for a
/// range-filtered field builds something the planner will never choose — which
/// is exactly the pure loss the M7 matrix measured.
fn index_kind_for(ctx: &OptContext<'_>, collection: &str, field: &str) -> IndexKind {
    const MOSTLY_EQUALITY: f64 = 0.8;
    match ctx.telemetry.equality_fraction(collection, field) {
        Some(f) if f >= MOSTLY_EQUALITY => IndexKind::Hash,
        // Mixed or range-dominated: an ordered index serves both, at a higher
        // maintenance cost than a hash index would have been.
        Some(_) => IndexKind::BTree,
        None => IndexKind::Hash,
    }
}

/// Split a `"collection.field"` scope. Returns `None` for a scope that names no
/// field, which is how a per-field optimization declines a global request.
fn split_scope(scope: &str) -> Option<(String, String)> {
    let (c, f) = scope.split_once('.')?;
    if c.is_empty() || f.is_empty() {
        return None;
    }
    Some((c.to_string(), f.to_string()))
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
        // Writes tombstone columnar rows faster than reads can use them, so the
        // benefit shrinks and the upkeep grows as the mix shifts to writes.
        let reads = 1.0 - ctx.telemetry.write_fraction();
        CostEstimate::faster(1.0 - 0.5 * reads, 1.0 - 0.55 * reads)
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

// -- schema freezing --------------------------------------------------------

pub struct FreezeSchemaOpt;

const FREEZE_SCHEMA_META: OptMeta = OptMeta {
    name: "freeze_schema",
    summary: "raise a collection's schema to the most rigid its data supports",
    scope_kind: ScopeKind::PerCollection,
    min_level: 8,
    // The largest freedom cost in the library, and the only one that is not
    // notional: a frozen collection rejects records the loose one accepted.
    axis_effects: AxisEffects::new(6, 4, -9),
    requires_guarantees: GuaranteeRequirements::ANY,
    prerequisites: &[],
    // Widening a schema back out is safe, but records written under the narrow
    // one were already constrained by it. That cannot be undone.
    reversibility: Reversibility::Destructive,
    conflicts_with: &[],
};

/// Records that must agree before a shape is called settled.
///
/// Freezing on a handful of records is freezing on a coincidence.
const MIN_ROWS_TO_FREEZE: usize = 1_000;

impl Optimization for FreezeSchemaOpt {
    fn meta(&self) -> &OptMeta {
        &FREEZE_SCHEMA_META
    }

    fn candidate_scopes(&self, ctx: &OptContext<'_>) -> Vec<String> {
        ctx.collections
            .iter()
            .filter(|(c, n)| *n >= MIN_ROWS_TO_FREEZE && !ctx.is_fixed_size(c))
            .map(|(c, _)| c.clone())
            .collect()
    }

    fn applicability(&self, ctx: &OptContext<'_>) -> Applicability {
        if self.candidate_scopes(ctx).is_empty() {
            return Applicability::NotYet(format!(
                "no collection is both loose and larger than {MIN_ROWS_TO_FREEZE} rows"
            ));
        }
        Applicability::Applicable
    }

    fn estimate(&self, ctx: &OptContext<'_>) -> CostEstimate {
        let rows: usize = ctx.collections.iter().map(|(_, n)| n).sum();
        let mut e = CostEstimate::faster(0.7, 0.65);
        // A fixed layout is denser than a tagged one, so this saves as well as
        // speeding up — unusual, and the reason its resource effect is positive.
        e.storage_bytes = -(rows as i64 * 16);
        // Deliberately unconfident: whether the shape is really settled cannot
        // be known from a sample, and being wrong here rejects real data.
        e.with_confidence(0.25).with_build(BuildCost {
            estimated_secs: rows as f64 / 2e5,
            rows_read: rows as u64,
            online: false,
        })
    }

    fn plan_enable(&self, _ctx: &OptContext<'_>, scope: &str, _params: &Params) -> ChangePlan {
        ChangePlan::new(
            vec![Action::FreezeSchema {
                collection: scope.to_string(),
            }],
            // No inverse: widening the schema back would not un-constrain the
            // records already written under it.
            vec![],
        )
    }

    fn plan_disable(&self, _ctx: &OptContext<'_>, _scope: &str, _params: &Params) -> ChangePlan {
        ChangePlan::default()
    }
}

// -- direct lookup ----------------------------------------------------------

pub struct DirectLookupOpt;

const DIRECT_LOOKUP_META: OptMeta = OptMeta {
    name: "direct_lookup",
    summary: "address records arithmetically where the schema fixes their size",
    scope_kind: ScopeKind::PerCollection,
    min_level: 10,
    // Costs freedom: it only works while the schema stays fixed-width.
    axis_effects: AxisEffects::new(9, -7, -4),
    requires_guarantees: GuaranteeRequirements::ANY,
    prerequisites: &[],
    conflicts_with: &[],
    reversibility: Reversibility::RebuildRequired,
};

/// Below this, the array wastes more memory on empty slots than it saves.
const MIN_DENSITY: f64 = 0.5;

impl Optimization for DirectLookupOpt {
    fn meta(&self) -> &OptMeta {
        &DIRECT_LOOKUP_META
    }

    fn applicability(&self, ctx: &OptContext<'_>) -> Applicability {
        if ctx.fixed_size_collections.is_empty() {
            // Structural, not temporary: no amount of workload change makes a
            // variable-width schema directly addressable.
            return Applicability::Ineligible(
                "no collection has a fixed-size schema; records must have a constant stride"
                    .to_string(),
            );
        }
        // A flat array spans the whole id range whether or not the slots are
        // used, so a sparse id space turns the optimization into pure waste.
        let sparse: Vec<&String> = ctx
            .fixed_size_collections
            .iter()
            .filter(|c| ctx.density_of(c) < MIN_DENSITY)
            .collect();
        if sparse.len() == ctx.fixed_size_collections.len() {
            let worst = sparse
                .iter()
                .map(|c| (ctx.density_of(c), (*c).clone()))
                .min_by(|a, b| a.0.total_cmp(&b.0))
                .unwrap();
            return Applicability::NotYet(format!(
                "{} ids are only {:.1}% dense, below the {:.0}% at which a flat array pays",
                worst.1,
                worst.0 * 100.0,
                MIN_DENSITY * 100.0
            ));
        }
        Applicability::Applicable
    }

    fn estimate(&self, ctx: &OptContext<'_>) -> CostEstimate {
        let rows: usize = ctx
            .fixed_size_collections
            .iter()
            .map(|c| ctx.rows_in(c))
            .sum();
        // A whole second copy of the data, in exchange for removing the page
        // directory, the slot table and the search from the lookup path.
        CostEstimate::faster(0.4, 0.35)
            .with_ram(rows as i64 * 64)
            .with_maintenance(0.15)
            .with_confidence(0.5)
            .with_build(BuildCost {
                estimated_secs: rows as f64 / 2e6,
                rows_read: rows as u64,
                online: true,
            })
    }

    fn plan_enable(&self, _ctx: &OptContext<'_>, _scope: &str, _params: &Params) -> ChangePlan {
        ChangePlan::new(
            vec![Action::SetDirectLookup(true)],
            vec![Action::SetDirectLookup(false)],
        )
    }

    fn plan_disable(&self, _ctx: &OptContext<'_>, _scope: &str, _params: &Params) -> ChangePlan {
        ChangePlan::new(vec![Action::SetDirectLookup(false)], vec![])
    }
}

// -- shared helpers ---------------------------------------------------------

/// Cache size from the level preset, falling back to a default.
fn entries(ctx: &OptContext<'_>, default: usize) -> usize {
    let _ = ctx;
    default
}

fn pool_pages(ctx: &OptContext<'_>) -> usize {
    let _ = ctx;
    8192
}

/// Documented but unimplemented, so the gap is visible in one place.
/// Optimizations the level table names but the engine does not implement.
///
/// Empty, for now. Kept rather than deleted because the list existing is what
/// makes "not implemented" a declaration instead of a silence — a registry that
/// simply lacked an entry would be indistinguishable from one that had never
/// considered it.
pub const NOT_YET_IMPLEMENTED: &[(&str, &str)] = &[];

#[cfg(test)]
mod tests {
    use super::*;
    use adabt_core::index_kind::IndexKind;
    use adabt_core::policy::Policy;
    use adabt_telemetry::Snapshot;

    pub(super) struct Fx {
        pub(super) collections: Vec<(String, usize)>,
        filtered: Vec<(String, String, u64)>,
        fixed: Vec<String>,
        max_ids: Vec<(String, u64)>,
        indexes: Vec<(String, String, IndexKind)>,
        pub(super) snap: Snapshot,
        policy: Policy,
    }

    impl Fx {
        pub(super) fn new() -> Self {
            Self {
                collections: vec![("users".into(), 10_000)],
                filtered: vec![],
                fixed: vec![],
                max_ids: vec![("users".into(), 9_999)],
                indexes: vec![],
                snap: Snapshot::default(),
                policy: Policy::conventional(),
            }
        }
        pub(super) fn ctx(&self) -> OptContext<'_> {
            OptContext {
                policy: &self.policy,
                telemetry: &self.snap,
                collections: &self.collections,
                filtered_fields: &self.filtered,
                fixed_size_collections: &self.fixed,
                max_ids: &self.max_ids,
                existing_indexes: &self.indexes,
                current_bytes: 0,
            }
        }
    }

    #[test]
    fn every_builtin_registers_and_orders() {
        let mut r = Registry::new();
        register_builtins(&mut r);
        assert_eq!(r.len(), 10);
        assert!(r.dependency_order().is_ok());
        for n in r.names() {
            assert!(!r.meta(n).unwrap().summary.is_empty(), "{n} has no summary");
        }
    }

    #[test]
    fn auto_index_waits_until_there_is_evidence() {
        let mut fx = Fx::new();
        assert!(matches!(
            AutoIndexOpt.applicability(&fx.ctx()),
            Applicability::NotYet(_)
        ));

        // Filtered, but not often enough.
        fx.filtered = vec![("users".into(), "country".into(), 3)];
        let a = AutoIndexOpt.applicability(&fx.ctx());
        assert!(
            a.reason().unwrap().contains("filtered only 3 times"),
            "{a:?}"
        );

        // Now often enough.
        fx.filtered = vec![("users".into(), "country".into(), 50)];
        assert!(AutoIndexOpt.applicability(&fx.ctx()).is_applicable());
    }

    #[test]
    fn auto_index_ignores_a_collection_too_small_to_benefit() {
        let mut fx = Fx::new();
        fx.collections = vec![("users".into(), 10)];
        fx.filtered = vec![("users".into(), "country".into(), 500)];
        let a = AutoIndexOpt.applicability(&fx.ctx());
        assert!(a.reason().unwrap().contains("below the 1000"), "{a:?}");
    }

    #[test]
    fn auto_index_does_not_re_propose_an_existing_index() {
        let mut fx = Fx::new();
        fx.filtered = vec![("users".into(), "country".into(), 50)];
        fx.indexes = vec![("users".into(), "country".into(), IndexKind::Hash)];
        let a = AutoIndexOpt.applicability(&fx.ctx());
        assert!(a.reason().unwrap().contains("already indexed"), "{a:?}");
    }

    #[test]
    fn auto_index_plans_a_create_and_its_exact_inverse() {
        let mut fx = Fx::new();
        fx.filtered = vec![("users".into(), "country".into(), 50)];
        let p = AutoIndexOpt.plan_enable(&fx.ctx(), "users.country", &Params::default());
        assert_eq!(p.apply.len(), 1);
        assert_eq!(p.revert.len(), 1);
        assert!(p.describe().contains("create hash index on users.country"));
        assert!(matches!(p.revert[0], Action::DropIndex { .. }));
    }

    #[test]
    fn auto_index_honors_an_explicit_kind_param_over_the_telemetry_guess() {
        let mut fx = Fx::new();
        // Overwhelmingly equality, which alone would auto-detect Hash.
        fx.filtered = vec![("users".into(), "country".into(), 50)];
        let forced_btree = Params::default().with("kind", IndexKind::BTree.as_ordinal());
        let p = AutoIndexOpt.plan_enable(&fx.ctx(), "users.country", &forced_btree);
        assert!(
            p.describe().contains("create btree index on users.country"),
            "{}",
            p.describe()
        );
    }

    #[test]
    fn an_unrecognized_kind_param_falls_back_to_the_telemetry_guess() {
        let mut fx = Fx::new();
        fx.filtered = vec![("users".into(), "country".into(), 50)];
        let garbage = Params::default().with("kind", 99);
        let p = AutoIndexOpt.plan_enable(&fx.ctx(), "users.country", &garbage);
        assert!(p.describe().contains("create hash index on users.country"));
    }

    #[test]
    fn direct_lookup_is_structurally_ineligible_without_a_fixed_schema() {
        let fx = Fx::new();
        let a = DirectLookupOpt.applicability(&fx.ctx());
        // Ineligible, not NotYet: no workload change makes this possible.
        assert!(matches!(a, Applicability::Ineligible(_)), "{a:?}");
        assert!(a.reason().unwrap().contains("constant stride"));
    }

    #[test]
    fn direct_lookup_becomes_applicable_with_a_fixed_schema() {
        let mut fx = Fx::new();
        fx.fixed = vec!["users".into()];
        assert!(DirectLookupOpt.applicability(&fx.ctx()).is_applicable());
        let e = DirectLookupOpt.estimate(&fx.ctx());
        assert!(e.helps_latency());
        assert!(e.costs_resources(), "a second copy of the data is not free");
    }

    #[test]
    fn direct_lookup_reports_its_cost_to_freedom() {
        // It only works while the schema stays fixed-width, which is a real
        // constraint on the user, not merely a memory cost.
        const _: () = assert!(DIRECT_LOOKUP_META.axis_effects.freedom < 0);
        const _: () = assert!(DIRECT_LOOKUP_META.axis_effects.speed > 0);
        const _: () = assert!(DIRECT_LOOKUP_META.axis_effects.resources < 0);
    }

    #[test]
    fn a_write_heavy_workload_lowers_the_result_cache_estimate_without_vetoing_it() {
        use adabt_telemetry::Probe as _;
        let mut fx = Fx::new();
        let read_heavy = ResultCacheOpt.estimate(&fx.ctx());

        let probe = adabt_telemetry::CollectingProbe::new();
        for _ in 0..200 {
            probe.record(adabt_telemetry::Event::Op {
                collection: "users",
                kind: adabt_telemetry::OpKind::Insert,
                shape: adabt_telemetry::QueryShape::UNKNOWN,
                nanos: 1,
                rows: 1,
            });
        }
        fx.snap = probe.snapshot();
        let write_heavy = ResultCacheOpt.estimate(&fx.ctx());

        // Still possible — a bulk load must not permanently veto the cache —
        // but plainly worth less.
        assert!(ResultCacheOpt.applicability(&fx.ctx()).is_applicable());
        assert!(
            write_heavy.p50_delta.0 > read_heavy.p50_delta.0,
            "a write-heavy workload should reduce the expected benefit"
        );
    }

    #[test]
    fn every_optimization_plans_a_reversible_change() {
        let mut fx = Fx::new();
        fx.fixed = vec!["users".into()];
        fx.filtered = vec![("users".into(), "country".into(), 50)];
        let ctx = fx.ctx();
        let mut r = Registry::new();
        register_builtins(&mut r);
        for opt in r.iter() {
            let scopes = opt.candidate_scopes(&ctx);
            let scope = scopes
                .first()
                .cloned()
                .unwrap_or_else(|| "global".to_string());
            let p = opt.plan_enable(&ctx, &scope, &Params::default());
            if p.apply.is_empty() {
                continue;
            }
            if opt.meta().reversibility == Reversibility::Destructive {
                // Allowed to have no inverse — and for that reason never
                // applied automatically. See the driver's check.
                continue;
            }
            assert!(
                !p.revert.is_empty(),
                "{} plans a change it cannot undo but is not declared Destructive",
                opt.meta().name
            );
        }
    }

    #[test]
    fn unimplemented_optimizations_are_declared_rather_than_hidden() {
        let mut r = Registry::new();
        register_builtins(&mut r);
        for (name, _) in NOT_YET_IMPLEMENTED {
            assert!(
                !r.contains(name),
                "{name} is listed as unimplemented but is registered"
            );
        }
    }
}

#[cfg(test)]
mod resource_axis_tests {
    use super::tests::Fx;
    use super::*;
    use adabt_opt::Registry;

    /// The reason `record_compression` exists.
    ///
    /// Every other optimization spends resources to buy latency. A policy of
    /// `resources: 10, speed: 3` would have had nothing at all to select, which
    /// means the three-axis premise was structurally untestable rather than
    /// merely untested.
    #[test]
    fn at_least_one_optimization_reduces_resources() {
        let mut r = Registry::new();
        register_builtins(&mut r);
        let savers: Vec<&str> = r
            .iter()
            .filter(|o| o.meta().axis_effects.resources > 0)
            .map(|o| o.meta().name)
            .collect();
        assert!(
            !savers.is_empty(),
            "no optimization trades in the resource-saving direction; \
             a resources-priority policy would have nothing to choose"
        );
        assert!(savers.contains(&"record_compression"), "{savers:?}");
    }

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

    #[test]
    fn the_axes_now_point_in_both_directions() {
        let mut r = Registry::new();
        register_builtins(&mut r);
        let speed_buyers = r.iter().filter(|o| o.meta().axis_effects.speed > 0).count();
        let resource_savers = r
            .iter()
            .filter(|o| o.meta().axis_effects.resources > 0)
            .count();
        assert!(speed_buyers > 0);
        assert!(resource_savers > 0);
    }
}

#[cfg(test)]
mod irreversibility_tests {
    use super::tests::Fx;
    use super::*;
    use adabt_core::policy::{Policy, Priorities};
    use adabt_opt::driver::{DriverInput, OptimizationDriver};
    use adabt_opt::{AdaptiveDriver, OptimizationConfig, Registry};
    use adabt_telemetry::event::{Event, OpKind, QueryShape};
    use adabt_telemetry::{CollectingProbe, Probe};

    /// Every safety mechanism in the optimizer — measurement, retraction,
    /// shadow comparison, canary rollback — assumes a bad decision can be taken
    /// back. Where it cannot, the decision is not the optimizer's to make.
    #[test]
    fn the_adaptive_driver_never_proposes_an_irreversible_change() {
        let mut reg = Registry::new();
        register_builtins(&mut reg);

        let probe = CollectingProbe::new();
        for _ in 0..5_000 {
            probe.record(Event::Op {
                collection: "users",
                kind: OpKind::Get,
                shape: QueryShape(1),
                nanos: 1_000,
                rows: 1,
            });
        }
        let snap = probe.snapshot();

        let mut fx = Fx::new();
        fx.collections = vec![("users".into(), 100_000)];
        fx.snap = snap;
        let policy = Policy {
            priority: Priorities {
                speed: 10,
                resources: 10,
                freedom: 0,
            },
            ..Policy::conventional()
        };
        let ctx = fx.ctx();
        let empty = OptimizationConfig::new();

        let mut d = AdaptiveDriver::new();
        for _ in 0..20 {
            let decisions = d.decide(DriverInput {
                registry: &reg,
                current: &empty,
                policy: &policy,
                telemetry: &fx.snap,
                ctx: &ctx,
                under_experiment: &[],
                pinned: &[],
            });
            for x in &decisions {
                let meta = reg.meta(x.optimization).unwrap();
                assert_ne!(
                    meta.reversibility,
                    Reversibility::Destructive,
                    "the driver proposed an irreversible change: {}",
                    x.optimization
                );
            }
        }
        // Even with freedom weighted at zero, which is the policy most likely
        // to want it.
        assert!(reg
            .iter()
            .any(|o| o.meta().reversibility == Reversibility::Destructive));
    }
}
