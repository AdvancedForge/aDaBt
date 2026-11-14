//! The join operator: refuses self-joins, then runs an indexed nested-loop
//! join when the right side is a bare scan with an index on the join field,
//! and a hash join otherwise.

use adabt_core::error::{Error, Result};
use adabt_core::ids::RecordId;
use adabt_core::record::Record;
use adabt_core::value::Value;
use adabt_ir::plan::JoinKind;

use super::{batches_of, collect_rows, ExecBudget, ExecStats, Source, CANCEL_CHECK_INTERVAL};
use crate::batch::RecordBatch;
use crate::physical::PhysicalOp;

pub(super) fn join<S: Source>(
    left: &PhysicalOp,
    right: &PhysicalOp,
    kind: JoinKind,
    on: &(String, String),
    src: &mut S,
    stats: &mut ExecStats,
    budget: &ExecBudget,
) -> Result<Vec<RecordBatch>> {
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
    if matches!(right, PhysicalOp::HeapScan { .. }) {
        if let Some(rows) = indexed_nested_loop_join(
            &left_rows,
            &left_collection,
            &right_collection,
            kind,
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
        kind,
        on,
        budget,
    )?;
    Ok(batch_from_rows(rows))
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
