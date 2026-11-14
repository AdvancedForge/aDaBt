//! Relational operators over an input subtree: filter (including the
//! peek-based filters that decide rows from a few fields before fetching),
//! project, limit, sort and aggregate.

use adabt_core::error::Result;
use adabt_core::ids::RecordId;
use adabt_core::record::Record;
use adabt_core::value::Value;
use adabt_ir::plan::{Agg, AggKind, SortKey};
use adabt_ir::Expr;
use std::collections::BTreeMap;

use super::index::covering_filtered;
use super::{
    batches_of, collect_rows, compare_rows, fetch_batches, run, ExecBudget, ExecStats, Source,
    CANCEL_CHECK_INTERVAL,
};
use crate::batch::RecordBatch;
use crate::physical::PhysicalOp;

pub(super) fn filter<S: Source>(
    input: &PhysicalOp,
    predicate: &Expr,
    src: &mut S,
    stats: &mut ExecStats,
    budget: &ExecBudget,
) -> Result<Vec<RecordBatch>> {
    // Over a covering index the rows can be judged where they are stored,
    // so the ones the predicate rejects are never copied out.
    if let Some(batches) = covering_filtered(input, predicate, None, src, stats)? {
        return Ok(batches);
    }

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

    // A predicate that reads exactly one field, sitting directly over
    // a heap scan, can decide most rows from that one field: peek it
    // per id (an address calculation on a fixed-schema collection),
    // evaluate against a one-field record the program cannot see
    // past, and fetch in full only the rows that survive. Decode cost
    // tracks selectivity instead of table size; the answers are the
    // same rows because peeking is defined to agree with decoding.
    let mut referenced = Vec::new();
    predicate.referenced_fields(&mut referenced);
    if !referenced.is_empty() && referenced.len() <= 4 {
        if let PhysicalOp::HeapScan { collection } = input {
            if referenced.len() == 1 {
                return filter_by_peek(collection, &referenced[0], &program, src, stats, budget);
            }
            return filter_by_peek_fields(collection, &referenced, &program, src, stats, budget);
        }
    }

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

/// The borrowed-view filter: decide every row from one peeked field, then
/// fetch only the survivors in full.
///
/// Equivalence with the generic path rests on three facts, each pinned by a
/// test elsewhere: `referenced_fields` enumerates everything the program can
/// read, so the one-field record it evaluates against cannot be missing
/// information; a peeked field equals the same field of the fully decoded
/// record, including the absent-versus-null distinction (`Some(None)` builds
/// an empty record exactly as a decode of a row without that field would);
/// and a dead row yields outer `None` here just as `fetch` yields nothing for
/// it there. Survivors are re-fetched through `fetch_batches`, so ordering,
/// batching, deduplication, and cancellation are whatever the generic path's
/// own are.
fn filter_by_peek<S: Source>(
    collection: &str,
    field: &str,
    program: &adabt_ir::vm::Program,
    src: &mut S,
    stats: &mut ExecStats,
    budget: &ExecBudget,
) -> Result<Vec<RecordBatch>> {
    let ids = src.all_ids(collection)?;
    let mut survivors = Vec::new();
    for (i, id) in ids.iter().enumerate() {
        if i % CANCEL_CHECK_INTERVAL == 0 {
            budget.check_cancelled()?;
        }
        stats.rows_scanned += 1;
        let keep = match src.peek_field(collection, *id, field)? {
            // No row: not a candidate, same as the fetch path skipping it.
            None => false,
            Some(value) => {
                let probe = match value {
                    Some(v) => Record::new().with(field, v),
                    None => Record::new(),
                };
                program.matches(&probe)
            }
        };
        if keep {
            survivors.push(*id);
        }
    }
    // Survivors were already counted when their field was peeked; re-fetching
    // them in full completes those rows rather than scanning new ones, so
    // their contribution is folded back out.
    let counted = stats.rows_scanned;
    let batches = fetch_batches(collection, survivors, src, stats, budget)?;
    stats.rows_scanned = counted;
    Ok(batches)
}

/// Multi-field counterpart: decide every row from a projected peek of only the
/// fields the predicate reads, then fetch only the survivors in full.
/// Uses `fetch_projected` so a wide record's other text never decodes —
///
/// the literal remainder that single-field `filter_by_peek` left.
fn filter_by_peek_fields<S: Source>(
    collection: &str,
    fields: &[String],
    program: &adabt_ir::vm::Program,
    src: &mut S,
    stats: &mut ExecStats,
    budget: &ExecBudget,
) -> Result<Vec<RecordBatch>> {
    let refs: Vec<&str> = fields.iter().map(|s| s.as_str()).collect();
    let ids = src.all_ids(collection)?;
    let mut survivors = Vec::new();
    for (i, id) in ids.iter().enumerate() {
        if i % CANCEL_CHECK_INTERVAL == 0 {
            budget.check_cancelled()?;
        }
        stats.rows_scanned += 1;
        let probe = match src.fetch_projected(collection, *id, &refs)? {
            None => continue,
            Some(r) => r,
        };
        if program.matches(&probe) {
            survivors.push(*id);
        }
    }
    let counted = stats.rows_scanned;
    let batches = fetch_batches(collection, survivors, src, stats, budget)?;
    stats.rows_scanned = counted;
    Ok(batches)
}

pub(super) fn project<S: Source>(
    input: &PhysicalOp,
    fields: &[String],
    src: &mut S,
    stats: &mut ExecStats,
    budget: &ExecBudget,
) -> Result<Vec<RecordBatch>> {
    let refs: Vec<&str> = fields.iter().map(|s| s.as_str()).collect();
    // The covering plan's usual shape. Filtering and projecting the stored
    // rows in place copies out only the returned fields of the surviving
    // rows — not the filtered field, which is checked and then projected
    // away, and which otherwise cost a clone per row.
    if let PhysicalOp::Filter {
        input: below,
        predicate,
    } = input
    {
        if let Some(batches) = covering_filtered(below, predicate, Some(&refs), src, stats)? {
            return Ok(batches);
        }
    }
    let mut batches = run(input, src, stats, budget)?;
    // The input rows are owned and read by nothing after this, so each is
    // narrowed where it stands rather than copied into a fresh projection
    // and then dropped: building a second record per row cost a field
    // vector, a copy of every kept string and a free of every original one.
    for b in &mut batches {
        for rec in &mut b.records {
            rec.retain_fields(&refs);
        }
    }
    Ok(batches)
}

pub(super) fn limit<S: Source>(
    input: &PhysicalOp,
    n: usize,
    src: &mut S,
    stats: &mut ExecStats,
    budget: &ExecBudget,
) -> Result<Vec<RecordBatch>> {
    let mut out = Vec::new();
    let mut taken = 0usize;
    for mut b in run(input, src, stats, budget)? {
        if taken >= n {
            break;
        }
        let room = n - taken;
        if b.len() > room {
            let keep: Vec<bool> = (0..b.len()).map(|i| i < room).collect();
            b.retain_mask(&keep);
        }
        taken += b.len();
        out.push(b);
    }
    Ok(out)
}

/// Blocking: the whole input is needed before any output is correct.
pub(super) fn sort<S: Source>(
    input: &PhysicalOp,
    keys: &[SortKey],
    src: &mut S,
    stats: &mut ExecStats,
    budget: &ExecBudget,
) -> Result<Vec<RecordBatch>> {
    let mut rows = collect_rows(input, src, stats, budget)?;
    rows.sort_by(|a, b| compare_rows(&a.1, &b.1, keys).then(a.0.cmp(&b.0)));
    Ok(batches_of(rows))
}

pub(super) fn aggregate<S: Source>(
    input: &PhysicalOp,
    group_by: &[String],
    aggs: &[Agg],
    src: &mut S,
    stats: &mut ExecStats,
    budget: &ExecBudget,
) -> Result<Vec<RecordBatch>> {
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
    Ok(vec![aggregate_rows(rows, group_by, aggs)])
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

fn aggregate_rows(rows: Vec<(RecordId, Record)>, group_by: &[String], aggs: &[Agg]) -> RecordBatch {
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
