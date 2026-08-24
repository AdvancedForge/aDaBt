//! Execution.
//!
//! Operators consume and produce batches, pulling from a `Source` that knows how
//! to fetch records and consult indexes. The source is a trait so that execution
//! does not depend on the storage crate: swapping a heap for a column store or a
//! directly-addressed array is a change of `Source` implementation, not a change
//! here.

use adabt_core::error::{Error, Result};
use adabt_core::ids::RecordId;
use adabt_core::record::Record;
use adabt_core::value::Value;
use adabt_ir::plan::{Agg, AggKind, JoinKind, SortKey};
use adabt_ir::Expr;
use std::collections::BTreeMap;
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

    /// Ids in an indexed range, or `None` when no index can serve it.
    fn index_range(
        &mut self,
        collection: &str,
        field: &str,
        lo: Bound<&Value>,
        hi: Bound<&Value>,
    ) -> Result<Option<Vec<RecordId>>>;
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
    let mut out = Vec::new();
    for b in batches {
        stats.batches += 1;
        out.extend(b.into_rows());
    }
    stats.rows_returned = out.len() as u64;
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

fn run<S: Source>(
    op: &PhysicalOp,
    src: &mut S,
    stats: &mut ExecStats,
    budget: &ExecBudget,
) -> Result<Vec<RecordBatch>> {
    budget.check_cancelled()?;
    match op {
        PhysicalOp::GetById { collection, id } => {
            let mut b = RecordBatch::new();
            if let Some(rec) = src.fetch(collection, *id)? {
                stats.rows_scanned += 1;
                b.push(*id, rec);
            }
            Ok(if b.is_empty() { vec![] } else { vec![b] })
        }

        PhysicalOp::GetByIds { collection, ids } => {
            fetch_batches(collection, ids.clone(), src, stats, budget)
        }

        PhysicalOp::HeapScan { collection } => {
            let ids = src.all_ids(collection)?;
            fetch_batches(collection, ids, src, stats, budget)
        }

        PhysicalOp::ColumnScan { collection, fields } => {
            match src.column_scan(collection, fields)? {
                Some(rows) => {
                    stats.rows_scanned += rows.len() as u64;
                    Ok(batches_of(rows))
                }
                None => {
                    // The planner believed a column store existed and it does not.
                    // Falling back keeps the answer correct; the counter makes the
                    // disagreement visible rather than silent.
                    stats.index_misses += 1;
                    let ids = src.all_ids(collection)?;
                    fetch_batches(collection, ids, src, stats, budget)
                }
            }
        }

        PhysicalOp::IndexLookup {
            collection,
            field,
            key,
            ..
        } => {
            stats.index_probes += 1;
            match src.index_lookup(collection, field, key)? {
                Some(ids) => fetch_batches(collection, ids, src, stats, budget),
                None => {
                    // The planner believed an index existed and it does not.
                    // Falling back to a scan keeps the answer correct; the
                    // counter makes the disagreement visible rather than silent.
                    stats.index_misses += 1;
                    let ids = src.all_ids(collection)?;
                    fetch_batches(collection, ids, src, stats, budget)
                }
            }
        }

        // The whole point of a covering index: rows without a fetch.
        //
        // Note what is *not* here — no `fetch_batches`, no page directory, no
        // buffer pool, no decode. The rows come out of the index. The
        // fallback is an ordinary indexed lookup, not a scan, because the
        // index on the field still exists even when its projection cannot
        // answer this particular query.
        PhysicalOp::CoveringLookup {
            collection,
            field,
            key,
            needed,
        } => {
            stats.index_probes += 1;
            match src.covering_lookup(collection, field, key, needed)? {
                Some(rows) => {
                    stats.rows_scanned += rows.len() as u64;
                    // Already in ascending id order — the covering index keeps
                    // its projections in an ordered map, and `fetch_batches`
                    // sorts for exactly the same reason: the same query must
                    // not return rows in a different order because a different
                    // structure answered it.
                    Ok(batches_of(rows))
                }
                None => {
                    stats.index_misses += 1;
                    match src.index_lookup(collection, field, key)? {
                        Some(ids) => fetch_batches(collection, ids, src, stats, budget),
                        None => {
                            let ids = src.all_ids(collection)?;
                            fetch_batches(collection, ids, src, stats, budget)
                        }
                    }
                }
            }
        }

        PhysicalOp::CompositeLookup {
            collection,
            fields,
            key,
        } => {
            stats.index_probes += 1;
            match src.composite_lookup(collection, fields, key)? {
                Some(ids) => fetch_batches(collection, ids, src, stats, budget),
                None => {
                    // The planner believed a composite index existed and it
                    // does not. Falling back keeps the answer correct; the
                    // counter makes the disagreement visible rather than
                    // silent, exactly as the single-field paths do.
                    stats.index_misses += 1;
                    let ids = src.all_ids(collection)?;
                    fetch_batches(collection, ids, src, stats, budget)
                }
            }
        }

        PhysicalOp::IndexRange {
            collection,
            field,
            lo,
            hi,
        } => {
            stats.index_probes += 1;
            let lo_ref = match lo {
                Bound::Included(v) => Bound::Included(v),
                Bound::Excluded(v) => Bound::Excluded(v),
                Bound::Unbounded => Bound::Unbounded,
            };
            let hi_ref = match hi {
                Bound::Included(v) => Bound::Included(v),
                Bound::Excluded(v) => Bound::Excluded(v),
                Bound::Unbounded => Bound::Unbounded,
            };
            match src.index_range(collection, field, lo_ref, hi_ref)? {
                Some(ids) => fetch_batches(collection, ids, src, stats, budget),
                None => {
                    stats.index_misses += 1;
                    let ids = src.all_ids(collection)?;
                    fetch_batches(collection, ids, src, stats, budget)
                }
            }
        }

        PhysicalOp::Filter { input, predicate } => {
            // Compiled once per execution of this node, then run per row —
            // the whole point of `adabt_ir::vm`. Tree-walking `Expr::matches`
            // re-descends `Box`ed nodes and (for `Like`) re-parses the
            // pattern on every single row; none of that work depends on the
            // row. `vm`'s own differential test is what makes substituting
            // the compiled evaluator safe: it asserts the two agree on
            // thousands of generated expression/record pairs, including the
            // three-valued cases where a reimplementation would quietly
            // diverge.
            let program = adabt_ir::vm::Program::compile(predicate);
            let mut out = Vec::new();
            for mut b in run(input, src, stats, budget)? {
                // Evaluated across the batch as a mask, which is the shape a
                // vectorised or compiled predicate will need.
                let keep: Vec<bool> = b.records.iter().map(|r| program.matches(r)).collect();
                b.retain_mask(&keep);
                if !b.is_empty() {
                    out.push(b);
                }
            }
            Ok(out)
        }

        PhysicalOp::Project { input, fields } => {
            let refs: Vec<&str> = fields.iter().map(|s| s.as_str()).collect();
            let mut out = Vec::new();
            for b in run(input, src, stats, budget)? {
                let mut p = RecordBatch::with_capacity(b.len());
                for (id, rec) in b.iter() {
                    p.push(id, rec.project(&refs));
                }
                out.push(p);
            }
            Ok(out)
        }

        PhysicalOp::Limit { input, n } => {
            let mut out = Vec::new();
            let mut taken = 0usize;
            for mut b in run(input, src, stats, budget)? {
                if taken >= *n {
                    break;
                }
                let room = *n - taken;
                if b.len() > room {
                    let keep: Vec<bool> = (0..b.len()).map(|i| i < room).collect();
                    b.retain_mask(&keep);
                }
                taken += b.len();
                out.push(b);
            }
            Ok(out)
        }

        // Blocking: the whole input is needed before any output is correct.
        PhysicalOp::Sort { input, keys } => {
            let mut rows = collect_rows(input, src, stats, budget)?;
            rows.sort_by(|a, b| compare_rows(&a.1, &b.1, keys).then(a.0.cmp(&b.0)));
            Ok(batches_of(rows))
        }

        PhysicalOp::Aggregate {
            input,
            group_by,
            aggs,
        } => {
            // Push the whole aggregate into the column store when its input is
            // a columnar scan, so nothing below builds a record per row.
            if let Some((collection, predicate)) = columnar_input(input) {
                if let Some(rows) = src.column_aggregate(collection, group_by, aggs, predicate)? {
                    stats.rows_scanned += rows.len() as u64;
                    return Ok(vec![RecordBatch::from_rows(rows)]);
                }
                stats.index_misses += 1;
            }
            let rows = collect_rows(input, src, stats, budget)?;
            Ok(vec![aggregate(rows, group_by, aggs)])
        }

        PhysicalOp::Join {
            left,
            right,
            kind,
            on,
        } => {
            budget.check_cancelled()?;
            let left_collection = left.collection().to_string();
            let right_collection = right.collection().to_string();
            if left_collection == right_collection {
                // Every field from both sides is prefixed `collection.field`
                // (see `merge_joined_fields`) precisely so two sides can never
                // silently overwrite one another's same-named field. A
                // self-join defeats that by construction — both prefixes are
                // identical — so it is refused rather than left to silently
                // drop half of one side's fields.
                return Err(Error::Unsupported(format!(
                    "self-joins are not supported yet: both sides of the join read {left_collection}"
                )));
            }
            let left_rows = collect_rows(left, src, stats, budget)?;

            // The fast path: `right` is an unfiltered, unconstrained scan, so
            // probing its join-field index once per left row (when one
            // exists) is exactly equivalent to materializing the whole right
            // side and hash-joining — except it never materializes a right
            // side that could be arbitrarily larger than the join actually
            // needs. Any other shape for `right` (its own filter, sort,
            // projection...) means bypassing it would silently ignore
            // whatever that subtree computes, so only a bare `HeapScan`
            // qualifies.
            if matches!(right.as_ref(), PhysicalOp::HeapScan { .. }) {
                if let Some(rows) = indexed_nested_loop_join(
                    &left_rows,
                    &left_collection,
                    &right_collection,
                    *kind,
                    on,
                    src,
                    stats,
                    budget,
                )? {
                    return Ok(batch_from_rows(rows));
                }
            }

            let right_rows = collect_rows(right, src, stats, budget)?;
            let rows = hash_join(
                &left_rows,
                &right_rows,
                &left_collection,
                &right_collection,
                *kind,
                on,
                budget,
            )?;
            Ok(batch_from_rows(rows))
        }
    }
}

/// Combine one matched pair of rows into a joined row.
///
/// Every field from both sides is prefixed `collection.field`, unconditionally
/// — not only on an actual name collision. A join result has no schema of its
/// own to say which fields might collide as either side's schema evolves, so
/// a rule that only prefixes on collision would mean whether `id` means
/// `users.id` or `orders.id` silently depends on facts about the *other*
/// side's field set that this row's own fields say nothing about. Prefixing
/// always is the same tradeoff SQL's own `table.column` qualification makes,
/// just applied consistently rather than only when ambiguous.
///
/// `right` is `None` for an unmatched row in a `Left` join: the right side's
/// fields are then simply absent from the result, not present with a null
/// value — consistent with how a `Dynamic` collection already treats a field
/// no record happens to carry.
fn merge_joined_fields(
    left_collection: &str,
    left_rec: &Record,
    right: Option<(&str, &Record)>,
) -> Record {
    let mut rec = Record::new();
    for (name, v) in left_rec.iter() {
        rec.set(format!("{left_collection}.{name}"), v.clone());
    }
    if let Some((right_collection, right_rec)) = right {
        for (name, v) in right_rec.iter() {
            rec.set(format!("{right_collection}.{name}"), v.clone());
        }
    }
    rec
}

/// Track a running total of joined-row bytes against `budget`, the same way
/// `collect_rows` tracks its input — a join's *output* can fan out well past
/// the size of either input (a many-to-many join), so it needs its own check
/// rather than trusting the inputs having already been within budget.
struct JoinAccumulator<'a> {
    budget: &'a ExecBudget,
    used: usize,
    out: Vec<Record>,
}

impl<'a> JoinAccumulator<'a> {
    fn new(budget: &'a ExecBudget) -> Self {
        Self {
            budget,
            used: 0,
            out: Vec::new(),
        }
    }
    fn push(&mut self, rec: Record) -> Result<()> {
        self.used += rec.approx_size();
        self.budget.check_ram(self.used)?;
        self.out.push(rec);
        Ok(())
    }
}

/// Indexed nested-loop join: for each left row, probe the right side's index
/// on the join field directly, never materializing the right side at all.
///
/// Returns `Ok(None)` when the right side has no index on `on.1` — the
/// caller's signal to fall back to `hash_join` instead. Whether an index
/// exists is checked once, against the first left row that actually has the
/// join field, rather than treated as a per-row question: an index either
/// exists on this field for this collection or it does not, and probing
/// every remaining row the same way a first probe already answered would
/// only repeat a full scan's worth of work under a different name.
#[allow(clippy::too_many_arguments)]
fn indexed_nested_loop_join<S: Source>(
    left_rows: &[(RecordId, Record)],
    left_collection: &str,
    right_collection: &str,
    kind: JoinKind,
    on: &(String, String),
    src: &mut S,
    stats: &mut ExecStats,
    budget: &ExecBudget,
) -> Result<Option<Vec<Record>>> {
    let Some(probe_key) = left_rows.iter().find_map(|(_, r)| r.get(&on.0)) else {
        // No left row carries the join field at all — nothing to probe, and
        // nothing an index could tell this join that materializing the right
        // side would not equally well answer, so let the caller take the
        // general path.
        return Ok(None);
    };
    if src
        .index_lookup(right_collection, &on.1, probe_key)?
        .is_none()
    {
        return Ok(None);
    }

    let mut acc = JoinAccumulator::new(budget);
    for (i, (_, left_rec)) in left_rows.iter().enumerate() {
        if i % CANCEL_CHECK_INTERVAL == 0 {
            budget.check_cancelled()?;
        }
        let Some(key) = left_rec.get(&on.0) else {
            // NULL never joins (SQL's three-valued `NULL = NULL` is unknown,
            // not true), but a `Left` join still owes this row one output
            // row with the right side absent.
            if kind == JoinKind::Left {
                acc.push(merge_joined_fields(left_collection, left_rec, None))?;
            }
            continue;
        };
        // Ids are sorted before fetching for the same reason `fetch_batches`
        // sorts them: every access path must yield the same row order, or an
        // index existing changes which rows appear in which order and this
        // project's central promise — optimization never changes the
        // answer — breaks at exactly its first join.
        let mut ids = src
            .index_lookup(right_collection, &on.1, key)?
            .unwrap_or_default();
        stats.index_probes += 1;
        ids.sort_unstable();
        ids.dedup();
        let mut matched = false;
        for id in ids {
            if let Some(right_rec) = src.fetch(right_collection, id)? {
                stats.rows_scanned += 1;
                matched = true;
                acc.push(merge_joined_fields(
                    left_collection,
                    left_rec,
                    Some((right_collection, &right_rec)),
                ))?;
            }
        }
        if !matched && kind == JoinKind::Left {
            acc.push(merge_joined_fields(left_collection, left_rec, None))?;
        }
    }
    Ok(Some(acc.out))
}

/// Hash join: build an index over the right side's join field, then probe it
/// once per left row.
///
/// The right side is always the build side and the left side is always the
/// one driven in order, for both join kinds — not a choice made per query.
/// `Left` requires visiting every left row exactly once regardless of
/// matches, so the left side has to be the driving side; keeping `Inner`
/// symmetric with it, rather than building on whichever side happens to be
/// smaller, is what keeps a join's output order independent of which
/// algorithm executed it. A build-side choice driven by row counts would
/// mean the same query could return the same rows in a different order
/// depending on data size alone — exactly the thing this project's central
/// invariant, that optimization never changes the answer, rules out.
fn hash_join(
    left_rows: &[(RecordId, Record)],
    right_rows: &[(RecordId, Record)],
    left_collection: &str,
    right_collection: &str,
    kind: JoinKind,
    on: &(String, String),
    budget: &ExecBudget,
) -> Result<Vec<Record>> {
    // The build side's own memory, charged against the same budget the rows
    // themselves were. `collect_rows` already bounded `right_rows`, but this
    // map is *additional* live memory on top of them — proportional to them,
    // so it cannot blow up independently, but a budget that silently ignores
    // a real allocation is not the circuit breaker it claims to be. Charged
    // per entry (`HASH_ENTRY_BYTES`, one position plus its share of slot and
    // `Vec` overhead) rather than by measuring the map, which `HashMap` does
    // not expose.
    //
    // Cancellation is checked here too: building a map over a large right
    // side is real work, and a query cancelled mid-build previously kept
    // building to completion before anything noticed.
    const HASH_ENTRY_BYTES: usize = std::mem::size_of::<usize>() + 32;
    let mut index: std::collections::HashMap<&Value, Vec<usize>> = std::collections::HashMap::new();
    let mut build_bytes = 0usize;
    for (i, (_, rec)) in right_rows.iter().enumerate() {
        if i % CANCEL_CHECK_INTERVAL == 0 {
            budget.check_cancelled()?;
        }
        // `!is_null` matters as much as `Some`: `NULL = NULL` is unknown, not
        // true, so a null key must never become a match candidate. Storage
        // normalization (`normalize_for_storage`) strips explicit nulls on
        // write, which hides this for rows read straight from a collection —
        // but a right side that is itself an aggregate can produce a genuine
        // `Value::Null` (an empty `Sum` does), and those never pass through
        // normalization. `Index::index_record` excludes nulls for exactly
        // this reason; matching it here is what keeps the two join
        // algorithms from disagreeing.
        if let Some(key) = rec.get(&on.1).filter(|v| !v.is_null()) {
            index.entry(key).or_default().push(i);
            build_bytes += HASH_ENTRY_BYTES;
            budget.check_ram(build_bytes)?;
        }
    }

    let mut acc = JoinAccumulator::new(budget);
    for (i, (_, left_rec)) in left_rows.iter().enumerate() {
        if i % CANCEL_CHECK_INTERVAL == 0 {
            budget.check_cancelled()?;
        }
        // Null-filtered on the probe side too, for the same reason as the
        // build side above — and to match `indexed_nested_loop_join`, where a
        // null needle finds nothing because the index never held a null to
        // find. A `Left` join still owes an unmatched row here, which the
        // `_` arm below produces.
        let matches = left_rec
            .get(&on.0)
            .filter(|v| !v.is_null())
            .and_then(|key| index.get(key));
        match matches {
            Some(idxs) if !idxs.is_empty() => {
                for &idx in idxs {
                    let (_, right_rec) = &right_rows[idx];
                    acc.push(merge_joined_fields(
                        left_collection,
                        left_rec,
                        Some((right_collection, right_rec)),
                    ))?;
                }
            }
            _ => {
                if kind == JoinKind::Left {
                    acc.push(merge_joined_fields(left_collection, left_rec, None))?;
                }
            }
        }
    }
    Ok(acc.out)
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
fn batches_of(rows: Vec<(RecordId, Record)>) -> Vec<RecordBatch> {
    let mut out = Vec::with_capacity(rows.len().div_ceil(BATCH_SIZE));
    let mut it = rows.into_iter();
    loop {
        let chunk: Vec<(RecordId, Record)> = it.by_ref().take(BATCH_SIZE).collect();
        if chunk.is_empty() {
            return out;
        }
        out.push(RecordBatch::from_rows(chunk));
    }
}

/// Fabricate a `RecordId` from each row's position in the join's output — the
/// same convention `aggregate` and `MaterializedViews::rows` already use for
/// a row with no single natural id of its own.
fn batch_from_rows(rows: Vec<Record>) -> Vec<RecordBatch> {
    let rows: Vec<(RecordId, Record)> = rows
        .into_iter()
        .enumerate()
        .map(|(i, r)| (RecordId(i as u64), r))
        .collect();
    batches_of(rows)
}

/// A columnar scan, optionally under one filter, that an aggregate can be
/// pushed into. Anything else returns `None` and takes the ordinary path.
fn columnar_input(op: &PhysicalOp) -> Option<(&str, Option<&Expr>)> {
    match op {
        PhysicalOp::ColumnScan { collection, .. } => Some((collection, None)),
        PhysicalOp::Filter { input, predicate } => match input.as_ref() {
            PhysicalOp::ColumnScan { collection, .. } => Some((collection, Some(predicate))),
            _ => None,
        },
        _ => None,
    }
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

struct AggAcc {
    count: u64,
    sum: f64,
    /// Whether any non-null value was seen, so `sum` over nothing is null
    /// rather than zero.
    saw_value: bool,
    min: Option<Value>,
    max: Option<Value>,
}

impl Default for AggAcc {
    fn default() -> Self {
        Self {
            count: 0,
            sum: 0.0,
            saw_value: false,
            min: None,
            max: None,
        }
    }
}

fn as_f64(v: &Value) -> Option<f64> {
    match v {
        Value::I64(n) => Some(*n as f64),
        Value::U64(n) => Some(*n as f64),
        Value::F64(f) => Some(*f),
        _ => None,
    }
}

fn aggregate(rows: Vec<(RecordId, Record)>, group_by: &[String], aggs: &[Agg]) -> RecordBatch {
    let mut groups: BTreeMap<Vec<Value>, Vec<AggAcc>> = BTreeMap::new();
    for (_, rec) in &rows {
        let key: Vec<Value> = group_by
            .iter()
            .map(|g| rec.get(g).cloned().unwrap_or(Value::Null))
            .collect();
        let accs = groups
            .entry(key)
            .or_insert_with(|| (0..aggs.len()).map(|_| AggAcc::default()).collect());
        for (i, a) in aggs.iter().enumerate() {
            let acc = &mut accs[i];
            match a.kind {
                // COUNT(*) counts rows; COUNT(field) counts non-null values.
                AggKind::Count => match &a.field {
                    None => acc.count += 1,
                    Some(f) => {
                        if rec.get(f).is_some_and(|v| !v.is_null()) {
                            acc.count += 1;
                        }
                    }
                },
                AggKind::Sum | AggKind::Avg => {
                    if let Some(v) = a.field.as_ref().and_then(|f| rec.get(f)).and_then(as_f64) {
                        acc.sum += v;
                        acc.count += 1;
                        acc.saw_value = true;
                    }
                }
                AggKind::Min => {
                    if let Some(v) = a.field.as_ref().and_then(|f| rec.get(f)) {
                        if !v.is_null() && acc.min.as_ref().is_none_or(|m| v < m) {
                            acc.min = Some(v.clone());
                        }
                    }
                }
                AggKind::Max => {
                    if let Some(v) = a.field.as_ref().and_then(|f| rec.get(f)) {
                        if !v.is_null() && acc.max.as_ref().is_none_or(|m| v > m) {
                            acc.max = Some(v.clone());
                        }
                    }
                }
            }
        }
    }

    // A grouped aggregate over no rows yields no groups; an ungrouped one still
    // yields a single row, because COUNT(*) of nothing is 0, not nothing.
    if groups.is_empty() && group_by.is_empty() {
        groups.insert(
            Vec::new(),
            (0..aggs.len()).map(|_| AggAcc::default()).collect(),
        );
    }

    let mut out = RecordBatch::with_capacity(groups.len());
    for (i, (key, accs)) in groups.into_iter().enumerate() {
        let mut rec = Record::new();
        for (g, v) in group_by.iter().zip(key) {
            rec.set(g.clone(), v);
        }
        for (a, acc) in aggs.iter().zip(accs) {
            let v = match a.kind {
                AggKind::Count => Value::U64(acc.count),
                AggKind::Sum => {
                    if acc.saw_value {
                        Value::F64(acc.sum)
                    } else {
                        Value::Null
                    }
                }
                AggKind::Avg => {
                    if acc.count > 0 {
                        Value::F64(acc.sum / acc.count as f64)
                    } else {
                        Value::Null
                    }
                }
                AggKind::Min => acc.min.unwrap_or(Value::Null),
                AggKind::Max => acc.max.unwrap_or(Value::Null),
            };
            rec.set(a.output.clone(), v);
        }
        // Aggregate output has no natural record identity; ids are positional.
        out.push(RecordId(i as u64), rec);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::planner::{plan, PlanContext};
    use adabt_index::{BTreeIndex, HashIndex, Index, IndexKind};
    use adabt_ir::plan::{Agg, AggKind, LogicalOp, SortKey};
    use adabt_ir::{CmpOp, Expr};
    use std::collections::BTreeMap;

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

    #[test]
    fn hash_join_never_matches_a_null_key() {
        // Reachable in production despite `normalize_for_storage` stripping
        // nulls on write: a right side that is itself an aggregate produces
        // genuine `Value::Null`s (an empty `Sum` does) which never pass
        // through storage normalization. `Index::index_record` excludes
        // nulls, so if `hash_join` did not, the same query would answer
        // differently depending only on whether an index existed.
        let nulls: Vec<(RecordId, Record)> = vec![
            (RecordId(1), Record::new().with("k", Value::Null)),
            (RecordId(2), Record::new().with("k", Value::Null)),
        ];
        let budget = ExecBudget::default();
        let on = ("k".to_string(), "k".to_string());

        let inner = hash_join(&nulls, &nulls, "l", "r", JoinKind::Inner, &on, &budget).unwrap();
        assert!(inner.is_empty(), "null joined null: {inner:?}");

        // A left join still owes every left row exactly one unmatched row.
        let left = hash_join(&nulls, &nulls, "l", "r", JoinKind::Left, &on, &budget).unwrap();
        assert_eq!(left.len(), 2);
        assert!(left.iter().all(|r| r.get("r.k").is_none()));
    }

    #[test]
    fn hash_join_still_matches_non_null_keys_normally() {
        // The guard above must not have made the join stop matching at all.
        let rows: Vec<(RecordId, Record)> = vec![(RecordId(1), Record::new().with("k", 7i64))];
        let out = hash_join(
            &rows,
            &rows,
            "l",
            "r",
            JoinKind::Inner,
            &("k".to_string(), "k".to_string()),
            &ExecBudget::default(),
        )
        .unwrap();
        assert_eq!(out.len(), 1);
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
}
