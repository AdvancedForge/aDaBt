//! Index-driven access paths: single-field, covering and composite lookups,
//! and indexed ranges. Each falls back to a slower path that gives the same
//! answer when the index the planner chose is not there, and counts the miss.

use adabt_core::error::Result;
use adabt_core::ids::RecordId;
use adabt_core::record::Record;
use adabt_core::value::Value;
use adabt_ir::vm::Program;
use adabt_ir::Expr;
use std::ops::Bound;

use super::access::scan_heap;
use super::{batches_of, fetch_batches, ExecBudget, ExecStats, Source};
use crate::batch::{RecordBatch, BATCH_SIZE};
use crate::physical::PhysicalOp;

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

/// A `Filter` over a covering access — and the `Project` above it, when
/// there is one — evaluated against the index's stored rows while they are
/// still borrowed, so a row is copied only if it survives and only in the
/// fields the plan returns. `None` when `input` is not a covering access or
/// no covering index can serve it; the caller then runs the operators one by
/// one, which is the same answer at the cost of cloning every stored row.
///
/// The answer is the separate operators' answer, batch for batch. Rows are
/// visited in the order the covering operator would have returned them, and
/// batches are cut where `batches_of` would have cut the unfiltered rows, so
/// the filter's dropping of emptied batches — and with it the `batches`
/// count — is unchanged. `Record::project` on the borrowed row equals
/// `Record::retain_fields` on an owned copy of it. Stats count what the
/// covering operator counts: one probe, and every row visited as scanned.
/// Nothing is counted on a miss, because the caller's fallback counts it.
pub(super) fn covering_filtered<S: Source>(
    input: &PhysicalOp,
    predicate: &Expr,
    projection: Option<&[&str]>,
    src: &mut S,
    stats: &mut ExecStats,
) -> Result<Option<Vec<RecordBatch>>> {
    let program = Program::compile(predicate);
    let mut sink = FilteredBatches::new(&program, projection);
    let served = match input {
        PhysicalOp::CoveringLookup {
            collection,
            field,
            key,
            needed,
        } => src.visit_covering_lookup(collection, field, key, needed, &mut |id, rec| {
            sink.offer(id, rec)
        })?,
        PhysicalOp::CoveringRange {
            collection,
            field,
            lo,
            hi,
            needed,
        } => src.visit_covering_range(
            collection,
            field,
            lo.as_ref(),
            hi.as_ref(),
            needed,
            &mut |id, rec| sink.offer(id, rec),
        )?,
        _ => return Ok(None),
    };
    if !served {
        return Ok(None);
    }
    stats.index_probes += 1;
    stats.rows_scanned += sink.visited as u64;
    Ok(Some(sink.finish()))
}

/// Batches built from lent rows: what `Filter` (and `Project`) would have
/// produced from the same rows handed over in `batches_of`'s batches.
struct FilteredBatches<'a> {
    program: &'a Program,
    projection: Option<&'a [&'a str]>,
    visited: usize,
    current: RecordBatch,
    done: Vec<RecordBatch>,
}

impl<'a> FilteredBatches<'a> {
    fn new(program: &'a Program, projection: Option<&'a [&'a str]>) -> Self {
        Self {
            program,
            projection,
            visited: 0,
            current: RecordBatch::new(),
            done: Vec::new(),
        }
    }

    fn offer(&mut self, id: RecordId, rec: &Record) {
        // An unfiltered batch would have ended here, so this one ends here
        // too — unless the filter left it empty, and `Filter` drops those.
        if self.visited > 0 && self.visited.is_multiple_of(BATCH_SIZE) && !self.current.is_empty() {
            self.done.push(std::mem::take(&mut self.current));
        }
        self.visited += 1;
        if !self.program.matches(rec) {
            return;
        }
        let row = match self.projection {
            Some(fields) => rec.project(fields),
            None => rec.clone(),
        };
        self.current.push(id, row);
    }

    fn finish(mut self) -> Vec<RecordBatch> {
        if !self.current.is_empty() {
            self.done.push(self.current);
        }
        self.done
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
