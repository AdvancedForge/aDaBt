//! Index-driven access paths: single-field, covering and composite lookups,
//! and indexed ranges. Each falls back to a slower path that gives the same
//! answer when the index the planner chose is not there, and counts the miss.

use adabt_core::error::Result;
use adabt_core::value::Value;
use std::ops::Bound;

use super::access::scan_heap;
use super::{batches_of, fetch_batches, ExecBudget, ExecStats, Source};
use crate::batch::RecordBatch;

pub(super) fn index_lookup<S: Source>(
    collection: &str,
    field: &str,
    key: &Value,
    src: &mut S,
    stats: &mut ExecStats,
    budget: &ExecBudget,
) -> Result<Vec<RecordBatch>> {
    stats.index_probes += 1;
    match src.index_lookup(collection, field, key)? {
        Some(ids) => fetch_batches(collection, ids, src, stats, budget),
        None => {
            // The planner believed an index existed and it does not.
            // Falling back to a scan keeps the answer correct; the
            // counter makes the disagreement visible rather than silent.
            stats.index_misses += 1;
            scan_heap(collection, src, stats, budget)
        }
    }
}

/// The whole point of a covering index: rows without a fetch.
///
/// Note what is *not* here — no `fetch_batches`, no page directory, no
/// buffer pool, no decode. The rows come out of the index. The
/// fallback is an ordinary indexed lookup, not a scan, because the
/// index on the field still exists even when its projection cannot
/// answer this particular query.
pub(super) fn covering_lookup<S: Source>(
    collection: &str,
    field: &str,
    key: &Value,
    needed: &[String],
    src: &mut S,
    stats: &mut ExecStats,
    budget: &ExecBudget,
) -> Result<Vec<RecordBatch>> {
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
                None => scan_heap(collection, src, stats, budget),
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn covering_range<S: Source>(
    collection: &str,
    field: &str,
    lo: &Bound<Value>,
    hi: &Bound<Value>,
    needed: &[String],
    src: &mut S,
    stats: &mut ExecStats,
    budget: &ExecBudget,
) -> Result<Vec<RecordBatch>> {
    stats.index_probes += 1;
    let lo_ref = lo.as_ref();
    let hi_ref = hi.as_ref();
    match src.covering_range(collection, field, lo_ref, hi_ref, needed)? {
        Some(rows) => {
            stats.rows_scanned += rows.len() as u64;
            // Same ordering contract as CoveringLookup: the covering
            // index keeps ids ascending inside its range scan.
            Ok(batches_of(rows))
        }
        None => {
            stats.index_misses += 1;
            match src.index_range(collection, field, lo_ref, hi_ref)? {
                Some(ids) => fetch_batches(collection, ids, src, stats, budget),
                None => scan_heap(collection, src, stats, budget),
            }
        }
    }
}

pub(super) fn composite_lookup<S: Source>(
    collection: &str,
    fields: &[String],
    key: &Value,
    src: &mut S,
    stats: &mut ExecStats,
    budget: &ExecBudget,
) -> Result<Vec<RecordBatch>> {
    stats.index_probes += 1;
    match src.composite_lookup(collection, fields, key)? {
        Some(ids) => fetch_batches(collection, ids, src, stats, budget),
        None => {
            // The planner believed a composite index existed and it
            // does not. Falling back keeps the answer correct; the
            // counter makes the disagreement visible rather than
            // silent, exactly as the single-field paths do.
            stats.index_misses += 1;
            scan_heap(collection, src, stats, budget)
        }
    }
}

pub(super) fn index_range<S: Source>(
    collection: &str,
    field: &str,
    lo: &Bound<Value>,
    hi: &Bound<Value>,
    src: &mut S,
    stats: &mut ExecStats,
    budget: &ExecBudget,
) -> Result<Vec<RecordBatch>> {
    stats.index_probes += 1;
    let lo_ref = lo.as_ref();
    let hi_ref = hi.as_ref();
    match src.index_range(collection, field, lo_ref, hi_ref)? {
        Some(ids) => fetch_batches(collection, ids, src, stats, budget),
        None => {
            stats.index_misses += 1;
            scan_heap(collection, src, stats, budget)
        }
    }
}
