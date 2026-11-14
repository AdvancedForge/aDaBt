//! Access paths that read a collection directly: fetches by id, heap and
//! column scans, and the columnar top-K selection.

use adabt_core::error::Result;
use adabt_core::ids::RecordId;
use adabt_core::record::Record;
use adabt_ir::plan::SortKey;
use std::collections::BinaryHeap;

use super::{
    batches_of, collect_rows, compare_rows, fetch_batches, ExecBudget, ExecStats, Source,
    CANCEL_CHECK_INTERVAL,
};
use crate::batch::RecordBatch;
use crate::physical::PhysicalOp;

pub(super) fn get_by_id<S: Source>(
    collection: &str,
    id: RecordId,
    src: &mut S,
    stats: &mut ExecStats,
) -> Result<Vec<RecordBatch>> {
    let mut b = RecordBatch::new();
    if let Some(rec) = src.fetch(collection, id)? {
        stats.rows_scanned += 1;
        b.push(id, rec);
    }
    Ok(if b.is_empty() { vec![] } else { vec![b] })
}

/// Every live row of `collection`, fetched in record-id order.
pub(super) fn scan_heap<S: Source>(
    collection: &str,
    src: &mut S,
    stats: &mut ExecStats,
    budget: &ExecBudget,
) -> Result<Vec<RecordBatch>> {
    let ids = src.all_ids(collection)?;
    fetch_batches(collection, ids, src, stats, budget)
}

pub(super) fn scan_columns<S: Source>(
    collection: &str,
    fields: &[String],
    src: &mut S,
    stats: &mut ExecStats,
    budget: &ExecBudget,
) -> Result<Vec<RecordBatch>> {
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
            scan_heap(collection, src, stats, budget)
        }
    }
}

pub(super) fn columnar_top_k<S: Source>(
    collection: &str,
    key: &String,
    descending: bool,
    k: usize,
    src: &mut S,
    stats: &mut ExecStats,
    budget: &ExecBudget,
) -> Result<Vec<RecordBatch>> {
    let keys = [SortKey {
        field: key.clone(),
        descending,
    }];
    // Fast path: selection happens inside the columnar
    // representation, over raw cells, and only the winners cross the
    // boundary. The ids arrive in answer order.
    if let Some(ids) = src.column_topk(collection, key, descending, k)? {
        stats.rows_scanned += ids.len() as u64;
        let out = fetch_in_order(collection, ids.into_iter(), src, stats, budget)?;
        return Ok(batches_of(out));
    }
    match src.column_scan(collection, std::slice::from_ref(key))? {
        Some(rows) => {
            stats.rows_scanned += rows.len() as u64;
            // The winners are then fetched WHOLE. What the columnar
            // read supplies is enough to choose them, not to answer
            // with: a record carrying only the sort key would hand
            // the projection above a row missing every other field,
            // and missing fields do not error — they read as absent.
            //
            // Fetched in loop rather than through `fetch_batches`,
            // which sorts ids for locality — right for a set of
            // lookups, wrong here, where the id order IS the answer's
            // order and k is small.
            let winners = top_k(rows, &keys, k);
            let out = fetch_in_order(
                collection,
                winners.into_iter().map(|(id, _)| id),
                src,
                stats,
                budget,
            )?;
            Ok(batches_of(out))
        }
        None => {
            // Same contract as ColumnScan's fallback above: the store
            // the planner believed in is gone, so answer the slow way
            // and say so.
            stats.index_misses += 1;
            let mut rows = collect_rows(
                &PhysicalOp::HeapScan {
                    collection: collection.to_string(),
                },
                src,
                stats,
                budget,
            )?;
            rows.sort_by(|a, b| compare_rows(&a.1, &b.1, &keys).then(a.0.cmp(&b.0)));
            rows.truncate(k);
            Ok(batches_of(rows))
        }
    }
}

/// Fetch each id in full, in exactly the order given, skipping ids that no
/// longer exist. Unlike `fetch_batches` this neither sorts nor de-duplicates:
/// the caller's order is the answer's order.
fn fetch_in_order<S: Source>(
    collection: &str,
    ids: impl ExactSizeIterator<Item = RecordId>,
    src: &mut S,
    stats: &mut ExecStats,
    budget: &ExecBudget,
) -> Result<Vec<(RecordId, Record)>> {
    let mut out = Vec::with_capacity(ids.len());
    for (i, id) in ids.enumerate() {
        if i % CANCEL_CHECK_INTERVAL == 0 {
            budget.check_cancelled()?;
        }
        if let Some(rec) = src.fetch(collection, id)? {
            stats.rows_scanned += 1;
            out.push((id, rec));
        }
    }
    Ok(out)
}

/// One row held in the top-K heap, ordered by Sort's total order.
///
/// A `BinaryHeap` is a max-heap by its element ordering, so ordering entries
/// by "later under the query's order is Greater" makes `peek()` the worst row
/// kept — exactly the one to evict when a better row arrives.
struct HeapEntry<'a>((RecordId, Record), &'a [SortKey]);

impl Eq for HeapEntry<'_> {}

impl PartialEq for HeapEntry<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == std::cmp::Ordering::Equal
    }
}

impl Ord for HeapEntry<'_> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        compare_rows(&self.0 .1, &other.0 .1, self.1).then(self.0 .0.cmp(&other.0 .0))
    }
}

impl PartialOrd for HeapEntry<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// The k smallest rows under Sort's exact total order — `compare_rows` over
/// `keys`, then record id, the same comparison a full `Sort` applies before
/// `Limit` truncates it — returned in that order.
///
/// The output is identical to sorting everything and taking k, ties included,
/// because the comparator and tiebreak are the same objects; only the work
/// between them changed. That identity is what lets the planner swap one for
/// the other without asking the differential rig a new question.
fn top_k(rows: Vec<(RecordId, Record)>, keys: &[SortKey], k: usize) -> Vec<(RecordId, Record)> {
    if k == 0 {
        return Vec::new();
    }
    let mut heap: BinaryHeap<HeapEntry<'_>> = BinaryHeap::with_capacity(k + 1);
    for row in rows {
        if heap.len() < k {
            heap.push(HeapEntry(row, keys));
            continue;
        }
        // The same comparison `Ord for HeapEntry` makes, unwrapped so this
        // reads exactly like the sort arm's comparator.
        let worst = heap.peek().expect("heap at capacity is non-empty");
        let ord = compare_rows(&row.1, &worst.0 .1, keys).then(row.0.cmp(&worst.0 .0));
        if ord == std::cmp::Ordering::Less {
            heap.pop();
            heap.push(HeapEntry(row, keys));
        }
    }
    let mut out: Vec<(RecordId, Record)> = heap.into_iter().map(|e| e.0).collect();
    out.sort_by(|a, b| compare_rows(&a.1, &b.1, keys).then(a.0.cmp(&b.0)));
    out
}
