//! Execution.
//!
//! Operators consume and produce batches, pulling from a `Source` that knows how
//! to fetch records and consult indexes. The source is a trait so that execution
//! does not depend on the storage crate: swapping a heap for a column store or a
//! directly-addressed array is a change of `Source` implementation, not a change
//! here.
//!
//! This module holds the public entry points, the shared types (`Source`,
//! `ExecStats`, `ExecBudget`), the `run` dispatcher that hands each physical
//! operator to its implementation, and the batching helpers every operator
//! shares. The operators themselves live in the submodules:
//!
//! - `access` — by-id fetches, heap and column scans, columnar top-K.
//! - `index` — index, covering and composite lookups and ranges.
//! - `relational` — filter, project, limit, sort, aggregate.
//! - `join` — the join operator and its two algorithms.

mod access;
mod index;
mod join;
mod relational;

use adabt_core::error::{Error, Result};
use adabt_core::ids::RecordId;
use adabt_core::record::Record;
use adabt_core::value::Value;
use adabt_ir::plan::{Agg, SortKey};
use adabt_ir::Expr;
use std::ops::Bound;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::batch::{RecordBatch, BATCH_SIZE};
use crate::physical::{PhysicalOp, PhysicalPlan};

/// What a running query is allowed to cost, and how it can be told to stop
/// before it finishes on its own.
///
/// `ExecBudget::default()` — no ram ceiling, no cancel flag — is unbounded and
/// uncancellable, which is every query's behaviour before this existed and
/// still the default for a caller that does not opt in to either.
#[derive(Clone, Default)]
pub struct ExecBudget {
    /// Checked against the running total of buffered row bytes in an operator
    /// that must hold its whole input before producing output — `Sort` and
    /// `Aggregate`. Not checked in a streaming operator (`Filter`, `Project`,
    /// a plain scan): those already bound their own memory to one
    /// `RecordBatch` regardless of collection size, so there is nothing
    /// unbounded there to guard against.
    pub max_ram_bytes: Option<u64>,
    /// Polled periodically while an operator iterates rows one at a time.
    /// Setting it from another thread after the query has started is the
    /// entire mechanism — there is no separate timeout clock in here at all,
    /// on purpose: a caller that wants "stop after 5 seconds" spawns a thread
    /// that sleeps 5 seconds and then sets this, which is a timeout with
    /// nothing else in this crate needing to know what a `Duration` is.
    pub cancel: Option<Arc<AtomicBool>>,
}

impl ExecBudget {
    pub fn unbounded() -> Self {
        Self::default()
    }

    fn check_cancelled(&self) -> Result<()> {
        if let Some(flag) = &self.cancel {
            if flag.load(Ordering::Relaxed) {
                return Err(Error::Cancelled("execution was cancelled".into()));
            }
        }
        Ok(())
    }

    /// `used` is the running total this operator has buffered so far, in the
    /// same approximate units as `Record::approx_size` — refuse to buffer any
    /// more once it would pass the ceiling.
    fn check_ram(&self, used: usize) -> Result<()> {
        if let Some(limit) = self.max_ram_bytes {
            if used as u64 > limit {
                return Err(Error::Cancelled(format!(
                    "exceeded the query's memory budget of {limit} bytes while buffering {used}"
                )));
            }
        }
        Ok(())
    }
}

/// How often a row-at-a-time loop polls `ExecBudget::cancel`, in rows.
///
/// An atomic load is cheap, but not free enough to pay it every row of a
/// hot scan; this amortizes it while still keeping a cancelled query's
/// response time small next to how long a large scan takes anyway.
const CANCEL_CHECK_INTERVAL: usize = 4096;

/// Where execution gets its data.
pub trait Source {
    fn fetch(&mut self, collection: &str, id: RecordId) -> Result<Option<Record>>;
    fn all_ids(&mut self, collection: &str) -> Result<Vec<RecordId>>;

    /// Read one field of one row without decoding the rest of the record.
    ///
    /// The two `Option`s carry distinct facts and the distinction is
    /// load-bearing: the outer `None` says *the row does not exist* (a dead
    /// record, an id out of range), while `Some(None)` says *the row exists
    /// but has no value for this field* — a distinction only visible to
    /// `IsNull`-shaped predicates, which must treat absent and null alike.
    /// A source that cannot peek returns the default, which is a fetch plus
    /// a projection: correct everywhere, free of assumptions about storage.
    fn peek_field(
        &mut self,
        collection: &str,
        id: RecordId,
        field: &str,
    ) -> Result<Option<Option<Value>>> {
        Ok(self.fetch(collection, id)?.map(|r| r.get(field).cloned()))
    }

    /// Fetch only the listed fields, decoding only those fields' bytes where
    /// the store supports it. Default is fetch-then-project.
    fn fetch_projected(
        &mut self,
        collection: &str,
        id: RecordId,
        fields: &[&str],
    ) -> Result<Option<Record>> {
        Ok(self.fetch(collection, id)?.map(|r| r.project(fields)))
    }

    /// Compute a grouped aggregate directly from columns, allocating once per
    /// group rather than once per row. `None` when no columnar representation
    /// exists.
    ///
    /// This, not `column_scan`, is where a columnar layout actually pays: a
    /// scan that rebuilds a record per row hands the executor rows again and
    /// keeps only the I/O advantage.
    fn column_aggregate(
        &mut self,
        _collection: &str,
        _group_by: &[String],
        _aggs: &[Agg],
        _predicate: Option<&Expr>,
    ) -> Result<Option<Vec<(RecordId, Record)>>> {
        Ok(None)
    }

    /// Every row, columnar, restricted to `fields`. `None` when no columnar
    /// representation exists for this collection.
    fn column_scan(
        &mut self,
        _collection: &str,
        _fields: &[String],
    ) -> Result<Option<Vec<(RecordId, Record)>>> {
        Ok(None)
    }

    /// The k ids whose single-key values come first under the executor's
    /// total order, selected inside the columnar representation without
    /// materializing records, returned in answer order.
    ///
    /// This, like `column_aggregate`, is where a columnar layout actually
    /// pays: `column_scan` hands back one record per row even for one field,
    /// and at scan scale that per-row construction is the cost of the query.
    /// `None` when no columnar representation exists or it does not hold
    /// `field`; the caller then falls back to `column_scan`, which is slower
    /// and correct in the same way.
    fn column_topk(
        &mut self,
        _collection: &str,
        _field: &str,
        _descending: bool,
        _k: usize,
    ) -> Result<Option<Vec<RecordId>>> {
        Ok(None)
    }

    /// Ids matching an indexed equality, or `None` when no index can serve it.
    fn index_lookup(
        &mut self,
        collection: &str,
        field: &str,
        key: &Value,
    ) -> Result<Option<Vec<RecordId>>>;
    /// Ids matching a composite index's full key, or `None` when no such
    /// index exists. Defaulted so a `Source` that has no composite indexes
    /// — the reference model, the merged-rows source used for sharded
    /// post-merge work — needs no change and correctly reports that it
    /// cannot serve one.
    fn composite_lookup(
        &mut self,
        _collection: &str,
        _fields: &[String],
        _key: &Value,
    ) -> Result<Option<Vec<RecordId>>> {
        Ok(None)
    }

    /// Rows matching an indexed equality, served entirely from a covering
    /// index — or `None` when no index on `field` carries every field in
    /// `needed`.
    ///
    /// The distinction between `None` and `Some(vec![])` is load-bearing here
    /// exactly as it is for `range`: the second means "the key matched
    /// nothing", and a caller that read it as "no covering index" would fall
    /// back to a scan and get the same answer slowly, while a caller that read
    /// `None` as "nothing matched" would silently drop every row.
    ///
    /// Defaulted, so a `Source` with no covering indexes correctly reports
    /// that it cannot serve one rather than needing to be changed.
    fn covering_lookup(
        &mut self,
        _collection: &str,
        _field: &str,
        _key: &Value,
        _needed: &[String],
    ) -> Result<Option<Vec<(RecordId, Record)>>> {
        Ok(None)
    }

    /// [`Source::covering_lookup`] with the rows lent rather than given:
    /// `visit` sees each stored projection in the same order, borrowed, and
    /// keeps what it needs. `false` means no covering index can serve the
    /// query, exactly as `None` does there; `visit` is then never called.
    ///
    /// It exists because an owned row is the wrong shape for the plan above
    /// a covering access. That plan filters the rows and projects them, and
    /// the filtered field is usually not projected — so handing over owned
    /// rows clones a value per row only for it to be checked and dropped, and
    /// clones whole rows the filter then rejects. Lent rows let the caller
    /// test the predicate first and copy only the fields it will return.
    ///
    /// The default lends the rows `covering_lookup` returns, so a `Source`
    /// that overrides only that (or nothing) behaves as before.
    fn visit_covering_lookup(
        &mut self,
        collection: &str,
        field: &str,
        key: &Value,
        needed: &[String],
        visit: &mut dyn FnMut(RecordId, &Record),
    ) -> Result<bool> {
        Ok(lend_rows(
            self.covering_lookup(collection, field, key, needed)?,
            visit,
        ))
    }

    /// Ids in an indexed range, or `None` when no index can serve it.
    fn index_range(
        &mut self,
        collection: &str,
        field: &str,
        lo: Bound<&Value>,
        hi: Bound<&Value>,
    ) -> Result<Option<Vec<RecordId>>>;
    /// Rows in an indexed range, served entirely from a covering index's
    /// projections — no fetch per id. `None` when no covering index on
    /// `field` with a range-capable backing carries every field in `needed`.
    fn covering_range(
        &mut self,
        _collection: &str,
        _field: &str,
        _lo: Bound<&Value>,
        _hi: Bound<&Value>,
        _needed: &[String],
    ) -> Result<Option<Vec<(RecordId, Record)>>> {
        Ok(None)
    }

    /// [`Source::covering_range`] with the rows lent rather than given, for
    /// the reasons and on the terms of [`Source::visit_covering_lookup`].
    fn visit_covering_range(
        &mut self,
        collection: &str,
        field: &str,
        lo: Bound<&Value>,
        hi: Bound<&Value>,
        needed: &[String],
        visit: &mut dyn FnMut(RecordId, &Record),
    ) -> Result<bool> {
        Ok(lend_rows(
            self.covering_range(collection, field, lo, hi, needed)?,
            visit,
        ))
    }
}

/// The default `visit_covering_*`: lend owned rows one at a time, and say
/// whether there were any rows to lend at all.
fn lend_rows(
    rows: Option<Vec<(RecordId, Record)>>,
    visit: &mut dyn FnMut(RecordId, &Record),
) -> bool {
    let Some(rows) = rows else {
        return false;
    };
    for (id, rec) in &rows {
        visit(*id, rec);
    }
    true
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ExecStats {
    pub rows_scanned: u64,
    pub rows_returned: u64,
    pub batches: u64,
    pub index_probes: u64,
    /// Times the plan wanted an index the source could not provide, and fell
    /// back to a scan. A non-zero count means the planner and the source
    /// disagree about what exists, which is a bug worth surfacing.
    pub index_misses: u64,
}

/// Run a plan to completion, unbounded and uncancellable.
pub fn execute<S: Source>(
    plan: &PhysicalPlan,
    src: &mut S,
    stats: &mut ExecStats,
) -> Result<Vec<(RecordId, Record)>> {
    execute_with_budget(plan, src, stats, &ExecBudget::default())
}

/// [`execute`], subject to `budget`.
pub fn execute_with_budget<S: Source>(
    plan: &PhysicalPlan,
    src: &mut S,
    stats: &mut ExecStats,
    budget: &ExecBudget,
) -> Result<Vec<(RecordId, Record)>> {
    let batches = run(&plan.root, src, stats, budget)?;
    // Sized once and filled straight from each batch's columns, so a large
    // answer is neither regrown as it accumulates nor staged through a
    // per-batch row vector on its way here.
    let mut out = Vec::with_capacity(batches.iter().map(RecordBatch::len).sum());
    for b in batches {
        stats.batches += 1;
        out.extend(b.ids.into_iter().zip(b.records));
    }
    stats.rows_returned = out.len() as u64;
    Ok(out)
}

fn run<S: Source>(
    op: &PhysicalOp,
    src: &mut S,
    stats: &mut ExecStats,
    budget: &ExecBudget,
) -> Result<Vec<RecordBatch>> {
    budget.check_cancelled()?;
    match op {
        PhysicalOp::GetById { collection, id } => access::get_by_id(collection, *id, src, stats),
        PhysicalOp::GetByIds { collection, ids } => {
            fetch_batches(collection, ids.clone(), src, stats, budget)
        }
        PhysicalOp::HeapScan { collection } => access::scan_heap(collection, src, stats, budget),
        PhysicalOp::ColumnScan { collection, fields } => {
            access::scan_columns(collection, fields, src, stats, budget)
        }
        PhysicalOp::ColumnarTopK {
            collection,
            key,
            descending,
            k,
        } => access::columnar_top_k(collection, key, *descending, *k, src, stats, budget),
        PhysicalOp::IndexLookup {
            collection,
            field,
            key,
            ..
        } => index::index_lookup(collection, field, key, src, stats, budget),
        PhysicalOp::CoveringLookup {
            collection,
            field,
            key,
            needed,
        } => index::covering_lookup(collection, field, key, needed, src, stats, budget),
        PhysicalOp::CoveringRange {
            collection,
            field,
            needed,
            lo,
            hi,
        } => index::covering_range(collection, field, lo, hi, needed, src, stats, budget),
        PhysicalOp::CompositeLookup {
            collection,
            fields,
            key,
        } => index::composite_lookup(collection, fields, key, src, stats, budget),
        PhysicalOp::IndexRange {
            collection,
            field,
            lo,
            hi,
        } => index::index_range(collection, field, lo, hi, src, stats, budget),
        PhysicalOp::Filter { input, predicate } => {
            relational::filter(input, predicate, src, stats, budget)
        }
        PhysicalOp::Project { input, fields } => {
            relational::project(input, fields, src, stats, budget)
        }
        PhysicalOp::Limit { input, n } => relational::limit(input, *n, src, stats, budget),
        PhysicalOp::LimitedFetch {
            access,
            predicates,
            n,
        } => index::limited_fetch(access, predicates, *n, src, stats, budget),
        PhysicalOp::Sort { input, keys } => relational::sort(input, keys, src, stats, budget),
        PhysicalOp::Aggregate {
            input,
            group_by,
            aggs,
        } => relational::aggregate(input, group_by, aggs, src, stats, budget),
        PhysicalOp::Join {
            left,
            right,
            kind,
            on,
        } => join::join(left, right, *kind, on, src, stats, budget),
    }
}

/// Fetch only the listed fields for each id, decoding only those fields'
/// bytes. Used when the planner knows the projection — e.g. survivors of a
/// selective filter feeding a `Project` — so fetching in full would decode
/// text that `Project` immediately discards.
#[allow(dead_code)]
fn fetch_projected_batches<S: Source>(
    collection: &str,
    mut ids: Vec<RecordId>,
    fields: &[&str],
    src: &mut S,
    stats: &mut ExecStats,
    budget: &ExecBudget,
) -> Result<Vec<RecordBatch>> {
    ids.sort_unstable();
    ids.dedup();
    let mut out = Vec::new();
    let mut current = RecordBatch::with_capacity(BATCH_SIZE.min(ids.len()));
    for (i, id) in ids.into_iter().enumerate() {
        if i % CANCEL_CHECK_INTERVAL == 0 {
            budget.check_cancelled()?;
        }
        if let Some(rec) = src.fetch_projected(collection, id, fields)? {
            stats.rows_scanned += 1;
            current.push(id, rec);
            if current.len() >= BATCH_SIZE {
                out.push(std::mem::take(&mut current));
            }
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    Ok(out)
}

/// Fetch a set of ids into batches, skipping ids that no longer exist.
///
/// Ids are sorted and de-duplicated first, so every access path yields record-id
/// order. That is not a stylistic choice: an index returns ids in *key* order,
/// a scan returns them in *id* order, and without this the same logical query
/// would return differently ordered results depending on whether an index
/// happened to exist. Optimization changing the answer — even only its order —
/// is precisely what this project promises cannot happen, and a promise that
/// holds only up to ordering is not one the differential rig can enforce.
///
/// The cost is a sort over the matched ids, which is small next to the fetches
/// that follow it and is bounded by the match count rather than the collection.
fn fetch_batches<S: Source>(
    collection: &str,
    mut ids: Vec<RecordId>,
    src: &mut S,
    stats: &mut ExecStats,
    budget: &ExecBudget,
) -> Result<Vec<RecordBatch>> {
    ids.sort_unstable();
    ids.dedup();
    let mut out = Vec::new();
    let mut current = RecordBatch::with_capacity(BATCH_SIZE.min(ids.len()));
    for (i, id) in ids.into_iter().enumerate() {
        if i % CANCEL_CHECK_INTERVAL == 0 {
            budget.check_cancelled()?;
        }
        if let Some(rec) = src.fetch(collection, id)? {
            stats.rows_scanned += 1;
            current.push(id, rec);
            if current.len() >= BATCH_SIZE {
                out.push(std::mem::take(&mut current));
            }
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    Ok(out)
}

/// Collect every row of `op`'s output into one `Vec`, checking `budget`'s ram
/// ceiling as rows accumulate. What `Sort` and `Aggregate` share: both need
/// their whole input before they can produce anything, which is exactly the
/// shape a memory budget has to watch.
fn collect_rows<S: Source>(
    op: &PhysicalOp,
    src: &mut S,
    stats: &mut ExecStats,
    budget: &ExecBudget,
) -> Result<Vec<(RecordId, Record)>> {
    let mut rows = Vec::new();
    let mut used = 0usize;
    for b in run(op, src, stats, budget)? {
        for (id, rec) in b.into_rows() {
            used += rec.approx_size();
            budget.check_ram(used)?;
            rows.push((id, rec));
        }
    }
    Ok(rows)
}

/// Split owned rows into batches, moving each row exactly once.
///
/// The obvious spelling — `rows.chunks(BATCH_SIZE).map(|c| c.to_vec())` —
/// *clones* every record, because `chunks` borrows and `to_vec` copies. The
/// rows are already owned at every call site here, so that clone buys nothing
/// at all: a sorted query duplicated its entire result set on the way out and
/// dropped the original immediately.
///
/// Same shape as the disabled result cache: correct answers, doubled work,
/// invisible to anything that compares answers.
///
/// Rows go straight into their batch's columns rather than through an
/// intermediate chunk vector, which would move every row twice.
fn batches_of(rows: Vec<(RecordId, Record)>) -> Vec<RecordBatch> {
    let mut out = Vec::with_capacity(rows.len().div_ceil(BATCH_SIZE));
    let mut remaining = rows.len();
    let mut it = rows.into_iter();
    while remaining > 0 {
        let size = remaining.min(BATCH_SIZE);
        let mut batch = RecordBatch::with_capacity(size);
        for (id, rec) in it.by_ref().take(size) {
            batch.push(id, rec);
        }
        remaining -= size;
        out.push(batch);
    }
    out
}

/// Order two records by the sort keys. A record missing a key sorts last in
/// ascending order, matching SQL's `NULLS LAST` default.
fn compare_rows(a: &Record, b: &Record, keys: &[SortKey]) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    for k in keys {
        let ord = match (a.get(&k.field), b.get(&k.field)) {
            (Some(x), Some(y)) => x.cmp(y),
            (None, None) => Ordering::Equal,
            (None, Some(_)) => Ordering::Greater,
            (Some(_), None) => Ordering::Less,
        };
        let ord = if k.descending { ord.reverse() } else { ord };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    Ordering::Equal
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::planner::{plan, PlanContext};
    use adabt_index::{BTreeIndex, HashIndex, Index, IndexKind};
    use adabt_ir::plan::{Agg, AggKind, LogicalOp, SortKey};
    use adabt_ir::{CmpOp, Expr};
    use std::collections::BTreeMap;
    use std::collections::HashMap;

    /// An in-memory source, so execution is tested apart from storage.
    struct MemSource {
        records: BTreeMap<RecordId, Record>,
        indexes: Vec<Box<dyn Index>>,
        pub fetches: u64,
        /// Set `cancel` the instant `fetches` reaches this count — a
        /// deterministic stand-in for another thread flipping the flag while
        /// a scan is in flight, without any real concurrency or timing in the
        /// test itself.
        cancel_after: Option<(u64, Arc<AtomicBool>)>,
    }

    impl MemSource {
        fn new(n: u64) -> Self {
            let mut records = BTreeMap::new();
            for i in 0..n {
                records.insert(
                    RecordId(i),
                    Record::new()
                        .with("id", i)
                        .with("bucket", (i % 5) as i64)
                        .with("score", (i as f64) * 1.5)
                        .with("name", format!("n{i}")),
                );
            }
            Self {
                records,
                indexes: Vec::new(),
                fetches: 0,
                cancel_after: None,
            }
        }

        fn cancel_after(mut self, fetches: u64, flag: Arc<AtomicBool>) -> Self {
            self.cancel_after = Some((fetches, flag));
            self
        }

        fn with_index(mut self, idx: Box<dyn Index>) -> Self {
            let mut idx = idx;
            for (id, rec) in &self.records {
                idx.index_record(*id, rec);
            }
            self.indexes.push(idx);
            self
        }

        fn ctx(&self) -> PlanContext<'_> {
            let mut m = std::collections::HashMap::new();
            m.insert(
                "c",
                self.indexes.iter().map(|i| (i.field(), i.kind())).collect(),
            );
            PlanContext {
                indexes: m,
                composite: std::collections::HashMap::new(),
                covering: std::collections::HashMap::new(),
                partial: std::collections::HashMap::new(),
                columnar: Vec::new(),
                columnar_fields: HashMap::new(),
                cardinality: HashMap::new(),
                row_counts: HashMap::new(),
            }
        }
    }

    impl Source for MemSource {
        fn fetch(&mut self, _c: &str, id: RecordId) -> Result<Option<Record>> {
            self.fetches += 1;
            if let Some((n, flag)) = &self.cancel_after {
                if self.fetches >= *n {
                    flag.store(true, Ordering::Relaxed);
                }
            }
            Ok(self.records.get(&id).cloned())
        }
        fn all_ids(&mut self, _c: &str) -> Result<Vec<RecordId>> {
            Ok(self.records.keys().copied().collect())
        }
        fn index_lookup(
            &mut self,
            _c: &str,
            field: &str,
            key: &Value,
        ) -> Result<Option<Vec<RecordId>>> {
            Ok(self
                .indexes
                .iter()
                .find(|i| i.field() == field)
                .map(|i| i.lookup(key)))
        }
        fn index_range(
            &mut self,
            _c: &str,
            field: &str,
            lo: Bound<&Value>,
            hi: Bound<&Value>,
        ) -> Result<Option<Vec<RecordId>>> {
            Ok(self
                .indexes
                .iter()
                .find(|i| i.field() == field && i.kind().supports_range())
                .and_then(|i| i.range(lo, hi)))
        }
    }

    fn eval(src: &mut MemSource, logical: &LogicalOp) -> (Vec<(RecordId, Record)>, ExecStats) {
        let p = plan(logical, &src.ctx());
        let mut stats = ExecStats::default();
        let rows = execute(&p, src, &mut stats).unwrap();
        (rows, stats)
    }

    fn eval_peeking<S: PeekPlanCtx + Source>(
        src: &mut S,
        logical: &LogicalOp,
    ) -> (Vec<(RecordId, Record)>, ExecStats) {
        let p = plan(logical, &src.plan_ctx());
        let mut stats = ExecStats::default();
        let rows = execute(&p, src, &mut stats).unwrap();
        (rows, stats)
    }

    /// The one thing `eval_peeking` needs beyond `Source`: a plan context.
    trait PeekPlanCtx {
        fn plan_ctx(&self) -> PlanContext<'_>;
    }
    impl PeekPlanCtx for PeekCountingSource {
        fn plan_ctx(&self) -> PlanContext<'_> {
            self.ctx()
        }
    }

    #[test]
    fn a_scan_returns_everything_in_id_order() {
        let mut s = MemSource::new(50);
        let (rows, _) = eval(&mut s, &LogicalOp::scan("c"));
        assert_eq!(rows.len(), 50);
        assert!(rows.windows(2).all(|w| w[0].0 < w[1].0));
    }

    #[test]
    fn get_by_id_returns_one_row_and_touches_one_record() {
        let mut s = MemSource::new(1000);
        let (rows, stats) = eval(&mut s, &LogicalOp::get("c", RecordId(42)));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, RecordId(42));
        assert_eq!(stats.rows_scanned, 1, "a point get must not scan");
    }

    #[test]
    fn get_by_a_missing_id_returns_nothing() {
        let mut s = MemSource::new(10);
        let (rows, _) = eval(&mut s, &LogicalOp::get("c", RecordId(999)));
        assert!(rows.is_empty());
    }

    #[test]
    fn a_filter_selects_the_right_rows() {
        let mut s = MemSource::new(100);
        let (rows, _) = eval(
            &mut s,
            &LogicalOp::scan("c").filter(Expr::eq("bucket", 3i64)),
        );
        assert_eq!(rows.len(), 20);
        assert!(rows
            .iter()
            .all(|(_, r)| r.get("bucket") == Some(&Value::I64(3))));
    }

    /// A source whose peek is genuinely separate from its fetch, so the test
    /// can count full decodes. The storage answer is the same map either way;
    /// what differs is which method the executor chooses to call.
    struct PeekCountingSource {
        records: BTreeMap<RecordId, Record>,
        fetches: u64,
    }

    impl PeekCountingSource {
        fn ctx(&self) -> PlanContext<'_> {
            let mut m = std::collections::HashMap::new();
            m.insert("c", Vec::new());
            PlanContext {
                indexes: m,
                composite: std::collections::HashMap::new(),
                covering: std::collections::HashMap::new(),
                partial: std::collections::HashMap::new(),
                columnar: Vec::new(),
                columnar_fields: HashMap::new(),
                cardinality: HashMap::new(),
                row_counts: HashMap::new(),
            }
        }
    }

    impl Source for PeekCountingSource {
        fn fetch(&mut self, _c: &str, id: RecordId) -> Result<Option<Record>> {
            self.fetches += 1;
            Ok(self.records.get(&id).cloned())
        }
        fn all_ids(&mut self, _c: &str) -> Result<Vec<RecordId>> {
            Ok(self.records.keys().copied().collect())
        }
        fn peek_field(
            &mut self,
            _c: &str,
            id: RecordId,
            field: &str,
        ) -> Result<Option<Option<Value>>> {
            // Row liveness from a bit, field from a slice — the shape of the
            // real fixed-schema path, minus the decode.
            Ok(self
                .records
                .contains_key(&id)
                .then(|| self.records.get(&id).and_then(|r| r.get(field).cloned())))
        }
        fn index_lookup(&mut self, _: &str, _: &str, _: &Value) -> Result<Option<Vec<RecordId>>> {
            Ok(None)
        }
        fn index_range(
            &mut self,
            _: &str,
            _: &str,
            _: Bound<&Value>,
            _: Bound<&Value>,
        ) -> Result<Option<Vec<RecordId>>> {
            Ok(None)
        }
    }

    #[test]
    fn a_single_field_filter_over_a_scan_fetches_only_the_rows_it_keeps() {
        let logical = LogicalOp::scan("c").filter(Expr::eq("bucket", 3i64));

        let mut cheap = PeekCountingSource {
            records: (0..100)
                .map(|i| {
                    (
                        RecordId(i),
                        Record::new()
                            .with("id", i)
                            .with("bucket", (i % 5) as i64)
                            .with("score", (i as f64) * 1.5)
                            .with("name", format!("n{i}")),
                    )
                })
                .collect(),
            fetches: 0,
        };
        let (got, stats) = eval_peeking(&mut cheap, &logical);

        // Same rows as the generic path...
        let mut plain = MemSource::new(100);
        let (want, _) = eval(&mut plain, &logical);
        assert_eq!(got, want, "the fused filter changed the answer");

        // ...but twenty full decodes instead of a hundred: one per surviving
        // row, none for the eighty the peek rejected.
        assert_eq!(cheap.fetches, 20);
        assert_eq!(stats.rows_scanned, 100, "every row was considered");
    }

    #[test]
    fn a_single_field_filter_treats_absent_and_null_alike_and_skips_dead_rows() {
        // id 0: null bucket; id 1: bucket absent entirely; id 2: bucket = 3;
        // id 3 and id 5: dead rows; id 4: bucket = 3.
        let mut records = BTreeMap::new();
        records.insert(RecordId(0), Record::new().with("bucket", Value::Null));
        records.insert(RecordId(1), Record::new().with("name", "no bucket"));
        records.insert(RecordId(2), Record::new().with("bucket", 3i64));
        records.insert(RecordId(4), Record::new().with("bucket", 3i64));
        let logical = LogicalOp::scan("c").filter(Expr::IsNull(Box::new(Expr::field("bucket"))));
        let mut src = PeekCountingSource {
            records,
            fetches: 0,
        };
        let (rows, _) = eval_peeking(&mut src, &logical);
        // Absent matches IS NULL exactly as null does; dead rows match nothing.
        let ids: Vec<u64> = rows.iter().map(|(id, _)| id.0).collect();
        assert_eq!(ids, vec![0, 1]);
    }

    #[test]
    fn an_index_gives_the_same_rows_as_a_scan_while_reading_far_fewer() {
        let logical = LogicalOp::scan("c").filter(Expr::eq("bucket", 3i64));

        let mut plain = MemSource::new(1000);
        let (want, plain_stats) = eval(&mut plain, &logical);

        let mut indexed = MemSource::new(1000).with_index(Box::new(HashIndex::new("bucket")));
        let (got, idx_stats) = eval(&mut indexed, &logical);

        assert_eq!(got, want, "the index changed the answer");
        assert_eq!(idx_stats.index_probes, 1);
        assert!(
            idx_stats.rows_scanned * 4 < plain_stats.rows_scanned,
            "index read {} rows, scan read {}",
            idx_stats.rows_scanned,
            plain_stats.rows_scanned
        );
    }

    #[test]
    fn a_range_index_gives_the_same_rows_as_a_scan() {
        let logical = LogicalOp::scan("c").filter(Expr::And(vec![
            Expr::cmp("bucket", CmpOp::Ge, 1i64),
            Expr::cmp("bucket", CmpOp::Lt, 4i64),
        ]));
        let mut plain = MemSource::new(500);
        let (want, _) = eval(&mut plain, &logical);
        let mut indexed = MemSource::new(500).with_index(Box::new(BTreeIndex::new("bucket")));
        let (got, stats) = eval(&mut indexed, &logical);
        assert_eq!(got, want);
        assert_eq!(stats.index_probes, 1);
        assert_eq!(stats.index_misses, 0);
    }

    #[test]
    fn a_promised_index_that_is_missing_falls_back_and_is_counted() {
        // The planner is told an index exists; the source does not have it.
        // The answer must stay correct and the disagreement must be visible.
        let mut s = MemSource::new(100);
        let mut m = std::collections::HashMap::new();
        m.insert("c", vec![("bucket", IndexKind::Hash)]);
        let p = plan(
            &LogicalOp::scan("c").filter(Expr::eq("bucket", 2i64)),
            &PlanContext {
                indexes: m,
                composite: std::collections::HashMap::new(),
                covering: std::collections::HashMap::new(),
                partial: std::collections::HashMap::new(),
                columnar: Vec::new(),
                columnar_fields: HashMap::new(),
                cardinality: HashMap::new(),
                row_counts: HashMap::new(),
            },
        );
        let mut stats = ExecStats::default();
        let rows = execute(&p, &mut s, &mut stats).unwrap();
        assert_eq!(rows.len(), 20, "fallback changed the answer");
        assert_eq!(stats.index_misses, 1, "a silent fallback is a bug");
    }

    #[test]
    fn projection_keeps_only_the_named_fields() {
        let mut s = MemSource::new(10);
        let (rows, _) = eval(
            &mut s,
            &LogicalOp::scan("c").project(vec!["id".into(), "name".into()]),
        );
        for (_, r) in &rows {
            assert_eq!(r.len(), 2);
            assert!(r.get("score").is_none());
        }
    }

    #[test]
    fn sorting_orders_ascending_and_descending() {
        let mut s = MemSource::new(30);
        let by = |descending| {
            LogicalOp::scan("c").sort(vec![SortKey {
                field: "score".into(),
                descending,
            }])
        };
        let (asc, _) = eval(&mut s, &by(false));
        let scores: Vec<f64> = asc
            .iter()
            .filter_map(|(_, r)| match r.get("score") {
                Some(Value::F64(f)) => Some(*f),
                _ => None,
            })
            .collect();
        assert!(scores.windows(2).all(|w| w[0] <= w[1]), "not ascending");

        let (desc, _) = eval(&mut s, &by(true));
        let ids: Vec<RecordId> = desc.iter().map(|(i, _)| *i).collect();
        let mut rev: Vec<RecordId> = asc.iter().map(|(i, _)| *i).collect();
        rev.reverse();
        assert_eq!(ids, rev);
    }

    #[test]
    fn sorting_is_stable_by_record_id_within_equal_keys() {
        let mut s = MemSource::new(50);
        let (rows, _) = eval(
            &mut s,
            &LogicalOp::scan("c").sort(vec![SortKey {
                field: "bucket".into(),
                descending: false,
            }]),
        );
        // Within one bucket the ids must still ascend, so results are
        // reproducible rather than depending on sort implementation details.
        let mut by_bucket: BTreeMap<i64, Vec<u64>> = BTreeMap::new();
        for (id, r) in &rows {
            if let Some(Value::I64(b)) = r.get("bucket") {
                by_bucket.entry(*b).or_default().push(id.0);
            }
        }
        for (b, ids) in by_bucket {
            assert!(ids.windows(2).all(|w| w[0] < w[1]), "bucket {b} unstable");
        }
    }

    #[test]
    fn a_record_missing_the_sort_key_sorts_last() {
        let mut s = MemSource::new(3);
        s.records
            .insert(RecordId(99), Record::new().with("id", 99u64));
        let (rows, _) = eval(
            &mut s,
            &LogicalOp::scan("c").sort(vec![SortKey {
                field: "score".into(),
                descending: false,
            }]),
        );
        assert_eq!(rows.last().unwrap().0, RecordId(99));
    }

    #[test]
    fn limit_truncates_and_stops_early() {
        let mut s = MemSource::new(5000);
        let (rows, _) = eval(&mut s, &LogicalOp::scan("c").limit(10));
        assert_eq!(rows.len(), 10);
        let (none, _) = eval(&mut s, &LogicalOp::scan("c").limit(0));
        assert!(none.is_empty());
    }

    #[test]
    fn limit_larger_than_the_input_returns_everything() {
        let mut s = MemSource::new(7);
        let (rows, _) = eval(&mut s, &LogicalOp::scan("c").limit(1000));
        assert_eq!(rows.len(), 7);
    }

    #[test]
    fn aggregates_compute_the_expected_values() {
        let mut s = MemSource::new(10);
        let (rows, _) = eval(
            &mut s,
            &LogicalOp::scan("c").aggregate(
                vec![],
                vec![
                    Agg::count("n"),
                    Agg::over(AggKind::Sum, "bucket", "total"),
                    Agg::over(AggKind::Min, "bucket", "lo"),
                    Agg::over(AggKind::Max, "bucket", "hi"),
                    Agg::over(AggKind::Avg, "bucket", "mean"),
                ],
            ),
        );
        assert_eq!(rows.len(), 1);
        let r = &rows[0].1;
        assert_eq!(r.get("n"), Some(&Value::U64(10)));
        // buckets are 0,1,2,3,4,0,1,2,3,4 -> sum 20, min 0, max 4, avg 2
        assert_eq!(r.get("total"), Some(&Value::F64(20.0)));
        assert_eq!(r.get("lo"), Some(&Value::I64(0)));
        assert_eq!(r.get("hi"), Some(&Value::I64(4)));
        assert_eq!(r.get("mean"), Some(&Value::F64(2.0)));
    }

    #[test]
    fn grouping_splits_the_aggregate() {
        let mut s = MemSource::new(100);
        let (rows, _) = eval(
            &mut s,
            &LogicalOp::scan("c").aggregate(vec!["bucket".into()], vec![Agg::count("n")]),
        );
        assert_eq!(rows.len(), 5);
        for (_, r) in &rows {
            assert_eq!(r.get("n"), Some(&Value::U64(20)));
            assert!(r.get("bucket").is_some());
        }
    }

    #[test]
    fn an_ungrouped_count_of_nothing_is_zero_not_nothing() {
        let mut s = MemSource::new(0);
        let (rows, _) = eval(
            &mut s,
            &LogicalOp::scan("c").aggregate(vec![], vec![Agg::count("n")]),
        );
        assert_eq!(
            rows.len(),
            1,
            "COUNT(*) over an empty input must return a row"
        );
        assert_eq!(rows[0].1.get("n"), Some(&Value::U64(0)));
    }

    #[test]
    fn a_grouped_aggregate_over_nothing_returns_no_groups() {
        let mut s = MemSource::new(0);
        let (rows, _) = eval(
            &mut s,
            &LogicalOp::scan("c").aggregate(vec!["bucket".into()], vec![Agg::count("n")]),
        );
        assert!(rows.is_empty());
    }

    #[test]
    fn sum_over_no_values_is_null_rather_than_zero() {
        let mut s = MemSource::new(0);
        let (rows, _) = eval(
            &mut s,
            &LogicalOp::scan("c").aggregate(vec![], vec![Agg::over(AggKind::Sum, "x", "t")]),
        );
        assert_eq!(rows[0].1.get("t"), Some(&Value::Null));
    }

    #[test]
    fn count_of_a_field_ignores_rows_missing_it() {
        let mut s = MemSource::new(5);
        s.records
            .insert(RecordId(99), Record::new().with("id", 99u64));
        let (rows, _) = eval(
            &mut s,
            &LogicalOp::scan("c").aggregate(
                vec![],
                vec![
                    Agg::count("all"),
                    Agg::over(AggKind::Count, "score", "with_score"),
                ],
            ),
        );
        let r = &rows[0].1;
        assert_eq!(r.get("all"), Some(&Value::U64(6)));
        assert_eq!(r.get("with_score"), Some(&Value::U64(5)));
    }

    #[test]
    fn a_full_pipeline_composes_correctly() {
        let mut s = MemSource::new(200).with_index(Box::new(HashIndex::new("bucket")));
        let (rows, stats) = eval(
            &mut s,
            &LogicalOp::scan("c")
                .filter(Expr::eq("bucket", 2i64))
                .project(vec!["id".into(), "score".into()])
                .sort(vec![SortKey {
                    field: "score".into(),
                    descending: true,
                }])
                .limit(3),
        );
        assert_eq!(rows.len(), 3);
        assert_eq!(stats.index_probes, 1);
        let scores: Vec<f64> = rows
            .iter()
            .filter_map(|(_, r)| match r.get("score") {
                Some(Value::F64(f)) => Some(*f),
                _ => None,
            })
            .collect();
        assert!(scores.windows(2).all(|w| w[0] >= w[1]), "not descending");
        for (_, r) in &rows {
            assert_eq!(r.len(), 2, "projection was not applied");
        }
    }

    #[test]
    fn batching_splits_large_results_without_changing_them() {
        let n = (BATCH_SIZE * 3 + 7) as u64;
        let mut s = MemSource::new(n);
        let (rows, stats) = eval(&mut s, &LogicalOp::scan("c"));
        assert_eq!(rows.len() as u64, n);
        assert_eq!(stats.batches, 4, "expected 3 full batches and a remainder");
    }

    #[test]
    fn a_cancel_flag_set_before_the_call_is_honored_immediately() {
        let mut s = MemSource::new(1000);
        let p = plan(&LogicalOp::scan("c"), &s.ctx());
        let mut stats = ExecStats::default();
        let flag = Arc::new(AtomicBool::new(true));
        let budget = ExecBudget {
            max_ram_bytes: None,
            cancel: Some(flag),
        };
        let err = execute_with_budget(&p, &mut s, &mut stats, &budget).unwrap_err();
        assert!(matches!(err, Error::Cancelled(_)));
        // Cancelled before doing any work at all — `run`'s own check catches
        // it before a single fetch.
        assert_eq!(s.fetches, 0);
    }

    #[test]
    fn a_cancel_flag_never_set_changes_nothing() {
        let mut s = MemSource::new(200);
        let p = plan(&LogicalOp::scan("c"), &s.ctx());
        let mut stats = ExecStats::default();
        let budget = ExecBudget {
            max_ram_bytes: None,
            cancel: Some(Arc::new(AtomicBool::new(false))),
        };
        let rows = execute_with_budget(&p, &mut s, &mut stats, &budget).unwrap();
        assert_eq!(rows.len(), 200);
    }

    #[test]
    fn a_flag_set_mid_scan_by_another_party_stops_the_scan_early() {
        // `cancel_after` sets the flag from inside `fetch` once a threshold is
        // crossed — a deterministic substitute for a second thread racing the
        // scan, so this test's outcome does not depend on timing.
        let n = 20_000u64;
        let flag = Arc::new(AtomicBool::new(false));
        let mut s = MemSource::new(n).cancel_after(100, Arc::clone(&flag));
        let p = plan(&LogicalOp::scan("c"), &s.ctx());
        let mut stats = ExecStats::default();
        let budget = ExecBudget {
            max_ram_bytes: None,
            cancel: Some(flag),
        };
        let err = execute_with_budget(&p, &mut s, &mut stats, &budget).unwrap_err();
        assert!(matches!(err, Error::Cancelled(_)));
        // Stopped within a couple of check intervals of the trigger, nowhere
        // near having scanned the whole 20,000-row collection.
        assert!(
            s.fetches < CANCEL_CHECK_INTERVAL as u64 * 3,
            "scan ran far past cancellation: {} fetches",
            s.fetches
        );
        assert!(s.fetches > 0);
    }

    #[test]
    fn a_sort_that_would_exceed_its_ram_budget_is_refused() {
        let mut s = MemSource::new(2000);
        let p = plan(
            &LogicalOp::scan("c").sort(vec![SortKey {
                field: "score".into(),
                descending: false,
            }]),
            &s.ctx(),
        );
        let mut stats = ExecStats::default();
        // Comfortably smaller than 2000 rows' worth of buffered `Record`s.
        let budget = ExecBudget {
            max_ram_bytes: Some(256),
            cancel: None,
        };
        let err = execute_with_budget(&p, &mut s, &mut stats, &budget).unwrap_err();
        assert!(matches!(err, Error::Cancelled(_)));
    }

    #[test]
    fn a_sort_within_its_ram_budget_succeeds() {
        let mut s = MemSource::new(2000);
        let p = plan(
            &LogicalOp::scan("c").sort(vec![SortKey {
                field: "score".into(),
                descending: false,
            }]),
            &s.ctx(),
        );
        let mut stats = ExecStats::default();
        let budget = ExecBudget {
            max_ram_bytes: Some(10 * 1024 * 1024),
            cancel: None,
        };
        let rows = execute_with_budget(&p, &mut s, &mut stats, &budget).unwrap();
        assert_eq!(rows.len(), 2000);
    }

    #[test]
    fn an_unbudgeted_sort_behaves_exactly_as_before() {
        let mut s = MemSource::new(500);
        let (rows, _) = eval(
            &mut s,
            &LogicalOp::scan("c").sort(vec![SortKey {
                field: "score".into(),
                descending: false,
            }]),
        );
        assert_eq!(rows.len(), 500);
    }

    /// A source with covering access and nothing else of note: rows come
    /// back owned, so the borrowed-row path runs through the trait's default
    /// `visit_covering_*` — the shape every non-engine source has.
    struct CoveringSource(MemSource);

    impl CoveringSource {
        fn rows_where(
            &self,
            keep: impl Fn(&Value) -> bool,
            field: &str,
        ) -> Vec<(RecordId, Record)> {
            self.0
                .records
                .iter()
                .filter(|(_, r)| r.get(field).is_some_and(&keep))
                .map(|(id, r)| (*id, r.clone()))
                .collect()
        }
    }

    impl Source for CoveringSource {
        fn fetch(&mut self, c: &str, id: RecordId) -> Result<Option<Record>> {
            self.0.fetch(c, id)
        }
        fn all_ids(&mut self, c: &str) -> Result<Vec<RecordId>> {
            self.0.all_ids(c)
        }
        fn index_lookup(&mut self, c: &str, f: &str, k: &Value) -> Result<Option<Vec<RecordId>>> {
            self.0.index_lookup(c, f, k)
        }
        fn index_range(
            &mut self,
            c: &str,
            f: &str,
            lo: Bound<&Value>,
            hi: Bound<&Value>,
        ) -> Result<Option<Vec<RecordId>>> {
            self.0.index_range(c, f, lo, hi)
        }
        fn covering_lookup(
            &mut self,
            _c: &str,
            field: &str,
            key: &Value,
            _needed: &[String],
        ) -> Result<Option<Vec<(RecordId, Record)>>> {
            Ok(Some(self.rows_where(|v| v == key, field)))
        }
        fn covering_range(
            &mut self,
            _c: &str,
            field: &str,
            lo: Bound<&Value>,
            hi: Bound<&Value>,
            _needed: &[String],
        ) -> Result<Option<Vec<(RecordId, Record)>>> {
            use std::ops::RangeBounds;
            Ok(Some(self.rows_where(|v| (lo, hi).contains(v), field)))
        }
    }

    /// What `Project{Filter{leaf}}` produced before covering rows were
    /// filtered in place: the leaf's batches, masked, emptied batches
    /// dropped, then narrowed — counted the way `execute` counts them.
    fn operator_by_operator(
        leaf: &PhysicalOp,
        predicate: &Expr,
        fields: Option<&[&str]>,
        src: &mut CoveringSource,
    ) -> (Vec<(RecordId, Record)>, ExecStats) {
        let mut stats = ExecStats::default();
        let program = adabt_ir::vm::Program::compile(predicate);
        let mut rows = Vec::new();
        for mut b in run(leaf, src, &mut stats, &ExecBudget::default()).unwrap() {
            let keep: Vec<bool> = b.records.iter().map(|r| program.matches(r)).collect();
            b.retain_mask(&keep);
            if b.is_empty() {
                continue;
            }
            stats.batches += 1;
            for (id, mut rec) in b.into_rows() {
                if let Some(f) = fields {
                    rec.retain_fields(f);
                }
                rows.push((id, rec));
            }
        }
        stats.rows_returned = rows.len() as u64;
        (rows, stats)
    }

    #[test]
    fn covering_rows_filtered_in_place_match_the_operators_run_one_by_one() {
        // Bucket 0 of 12,000 rows is 2,400 rows: three unfiltered batches.
        // The predicate keeps the first 100 and the last 100 and empties the
        // middle batch, so batching by survivors alone would yield one batch
        // where the operators yield two — the stats would differ.
        let predicate = Expr::And(vec![
            Expr::eq("bucket", 0i64),
            Expr::Or(vec![
                Expr::cmp("id", CmpOp::Lt, 500u64),
                Expr::cmp("id", CmpOp::Ge, 11_500u64),
            ]),
        ]);
        let needed: Vec<String> = ["bucket", "id", "name"].map(String::from).to_vec();
        let leaves = [
            PhysicalOp::CoveringLookup {
                collection: "c".into(),
                field: "bucket".into(),
                key: Value::I64(0),
                needed: needed.clone(),
            },
            PhysicalOp::CoveringRange {
                collection: "c".into(),
                field: "bucket".into(),
                lo: Bound::Included(Value::I64(0)),
                hi: Bound::Excluded(Value::I64(1)),
                needed,
            },
        ];
        for leaf in leaves {
            for projection in [None, Some(vec!["name".to_string()])] {
                let mut src = CoveringSource(MemSource::new(12_000));
                let filtered = PhysicalOp::Filter {
                    input: Box::new(leaf.clone()),
                    predicate: predicate.clone(),
                };
                let root = match &projection {
                    Some(fields) => PhysicalOp::Project {
                        input: Box::new(filtered),
                        fields: fields.clone(),
                    },
                    None => filtered,
                };
                let plan = PhysicalPlan {
                    root,
                    rationale: String::new(),
                };
                let mut stats = ExecStats::default();
                let rows = execute(&plan, &mut src, &mut stats).unwrap();

                let refs: Option<Vec<&str>> = projection
                    .as_ref()
                    .map(|f| f.iter().map(String::as_str).collect());
                let (want_rows, want_stats) =
                    operator_by_operator(&leaf, &predicate, refs.as_deref(), &mut src);
                assert_eq!(rows.len(), 200);
                assert_eq!(rows, want_rows, "{leaf:?} {projection:?}");
                assert_eq!(stats, want_stats, "{leaf:?} {projection:?}");
                assert_eq!(stats.batches, 2);
            }
        }
    }
}
