//! Materialized views over grouped aggregates.
//!
//! An aggregate reads every row to produce a handful. A view holds the handful
//! and updates it as rows arrive, turning a query that costs O(rows) into one
//! that costs O(groups) — for a country breakdown of a million customers, four
//! numbers instead of a million reads.
//!
//! # The arithmetic problem, and the condition that solves it
//!
//! A maintained aggregate is computed by different arithmetic than a scanned
//! one. The scan adds every value in record order; the view adds each value as
//! it arrives and subtracts it again on delete. Floating-point addition is not
//! associative, so in general `SUM` maintained incrementally and `SUM`
//! recomputed by a scan disagree in the low bits — and disagree *more* the
//! longer the view lives. A discrepancy in the last decimal place of a sum is
//! exactly the divergence this project refuses to tolerate.
//!
//! But "in general" is doing work in that sentence. There is a condition under
//! which floating-point addition *is* exact and order therefore stops mattering:
//! when every value is an integer and every partial sum stays below 2^53, each
//! addition and subtraction is representable without rounding, so any order
//! produces bit-identical results. Counts, quantities, money in minor units —
//! most of what anyone actually sums — satisfy it.
//!
//! So each accumulator carries a budget. It admits a value only if the value is
//! an integer, and it tracks the running total of *absolute* values ever added
//! or subtracted; while that total stays under 2^53 every partial sum is exactly
//! representable and the view is trustworthy. The moment either condition fails,
//! the accumulator marks itself inexact and the view stops answering — the query
//! falls back to the scan, silently and correctly.
//!
//! The budget is a high-water mark and is never reduced on delete, which is
//! deliberately conservative: the scan only ever adds the values that are still
//! there, so a view that gives up early is safe and a view that gives up late is
//! not.
//!
//! `MIN` and `MAX` are excluded for an unrelated reason that no condition
//! rescues: they cannot be maintained under deletion at all. Removing the
//! current minimum tells you nothing about what the new one is without
//! re-reading every remaining value, so a "maintained" min is a scan wearing a
//! disguise.
//!
//! # What a view is
//!
//! A derived representation like any other: rebuildable from the primary,
//! droppable at any instant, and never consulted for a question it was not built
//! to answer. A view is defined by its collection, its grouping and its
//! aggregates, and answers only a query matching all three with no filter — a
//! filter would need the rows the view no longer has.

use adabt_core::ids::RecordId;
use adabt_core::record::Record;
use adabt_core::value::Value;
use adabt_ir::plan::{Agg, AggKind, LogicalOp};
use std::collections::BTreeMap;

/// Whether an aggregate can be kept up to date without re-reading the rows.
///
/// A statement about arithmetic, not about how much work each kind would be.
/// `MIN` and `MAX` are impossible under deletion; the rest are possible, and
/// whether they stay *exact* is decided per value as the data arrives.
pub fn is_maintainable(aggs: &[Agg]) -> bool {
    !aggs.is_empty()
        && aggs
            .iter()
            .all(|a| matches!(a.kind, AggKind::Count | AggKind::Sum | AggKind::Avg))
}

/// The largest integer magnitude an `f64` represents without rounding.
const EXACT_INTEGER_LIMIT: f64 = 9_007_199_254_740_992.0; // 2^53

/// The scan's numeric conversion, reproduced exactly.
///
/// Not "a conversion that gets the same answer" — the same one. Two functions
/// that agree on every case anyone thought to test are two places for the paths
/// to drift apart later.
fn as_f64(v: &Value) -> Option<f64> {
    match v {
        Value::I64(n) => Some(*n as f64),
        Value::U64(n) => Some(*n as f64),
        Value::F64(f) => Some(*f),
        _ => None,
    }
}

/// One accumulator, mirroring the executor's.
#[derive(Debug, Clone, Default, PartialEq)]
struct Acc {
    count: u64,
    sum: f64,
    saw_value: bool,
}

/// One group's running totals.
#[derive(Debug, Clone, Default, PartialEq)]
struct Group {
    /// Rows in the group. Tracked apart from the accumulators because a group
    /// exists as long as a row is in it, whatever any particular `COUNT(field)`
    /// says — a group of rows that are all null in the counted field still
    /// exists, and still reports zero.
    rows: u64,
    accs: Vec<Acc>,
}

#[derive(Debug, Clone)]
pub struct View {
    pub collection: String,
    pub group_by: Vec<String>,
    pub aggs: Vec<Agg>,
    groups: BTreeMap<Vec<Value>, Group>,
    /// Per aggregate: whether it can still be trusted, and how much of its
    /// exactness budget has been spent.
    exact: Vec<bool>,
    spent: Vec<f64>,
}

impl View {
    pub fn new(collection: &str, group_by: &[String], aggs: &[Agg]) -> Self {
        Self {
            collection: collection.to_string(),
            group_by: group_by.to_vec(),
            aggs: aggs.to_vec(),
            groups: BTreeMap::new(),
            exact: vec![true; aggs.len()],
            spent: vec![0.0; aggs.len()],
        }
    }

    /// Whether every aggregate in this view is still exact.
    ///
    /// One inexact accumulator disqualifies the whole view. Answering some
    /// aggregates from a view and the rest from a scan would be two answers to
    /// one question, and the row they arrive in has no way to say so.
    pub fn is_exact(&self) -> bool {
        self.exact.iter().all(|e| *e)
    }

    /// Charge one value against an aggregate's exactness budget.
    ///
    /// Both directions charge: a subtraction is as much an operation as an
    /// addition, and it is the total traffic through the accumulator that
    /// determines whether every partial sum stayed representable.
    fn charge(&mut self, i: usize, v: f64) {
        if !self.exact[i] {
            return;
        }
        if !v.is_finite() || v.fract() != 0.0 {
            self.exact[i] = false;
            return;
        }
        self.spent[i] += v.abs();
        if self.spent[i] >= EXACT_INTEGER_LIMIT {
            self.exact[i] = false;
        }
    }

    fn matches(&self, collection: &str, group_by: &[String], aggs: &[Agg]) -> bool {
        self.collection == collection && self.group_by == group_by && self.aggs == aggs
    }

    /// The group a record belongs to.
    ///
    /// A missing field groups under null, exactly as the scanning aggregate does
    /// — the two have to agree about this or every record with a missing field
    /// lands in a different group depending on which path answered.
    fn key_of(&self, rec: &Record) -> Vec<Value> {
        self.group_by
            .iter()
            .map(|g| rec.get(g).cloned().unwrap_or(Value::Null))
            .collect()
    }

    /// What this record contributes to each aggregate.
    ///
    /// `None` where it contributes nothing — a null in a counted field, a
    /// non-numeric value in a summed one — which is what the scan does too.
    fn contributions(&self, rec: &Record) -> Vec<Option<f64>> {
        self.aggs
            .iter()
            .map(|a| match (a.kind, &a.field) {
                // COUNT(*) counts rows; COUNT(field) counts non-null values.
                (AggKind::Count, None) => Some(0.0),
                (AggKind::Count, Some(f)) => rec.get(f).filter(|v| !v.is_null()).map(|_| 0.0),
                (_, Some(f)) => rec.get(f).and_then(as_f64),
                (_, None) => None,
            })
            .collect()
    }

    pub fn insert(&mut self, rec: &Record) {
        let key = self.key_of(rec);
        let add = self.contributions(rec);
        let n = self.aggs.len();
        let group = self.groups.entry(key).or_insert_with(|| Group {
            rows: 0,
            accs: vec![Acc::default(); n],
        });
        group.rows += 1;
        for (i, contribution) in add.iter().enumerate() {
            let Some(v) = contribution else { continue };
            let acc = &mut group.accs[i];
            acc.count += 1;
            acc.sum += v;
            acc.saw_value = true;
        }
        for (i, contribution) in add.into_iter().enumerate() {
            if let (Some(v), true) = (contribution, self.aggs[i].kind != AggKind::Count) {
                self.charge(i, v);
            }
        }
    }

    pub fn remove(&mut self, rec: &Record) {
        let key = self.key_of(rec);
        let sub = self.contributions(rec);
        let Some(group) = self.groups.get_mut(&key) else {
            return;
        };
        group.rows = group.rows.saturating_sub(1);
        for (i, contribution) in sub.iter().enumerate() {
            let Some(v) = contribution else { continue };
            let acc = &mut group.accs[i];
            acc.count = acc.count.saturating_sub(1);
            acc.sum -= v;
            // `saw_value` is not unset. The scan sets it when it meets a numeric
            // value and the view's count is what decides whether any remain, so
            // clearing it here would make an emptied sum report null where the
            // scan reports zero — and a group with no rows is removed anyway.
        }
        // A group with no rows is not a group with zero in it: the scanning
        // aggregate emits no row for a value nothing has, and a view that kept
        // an empty group would answer with a row the scan does not produce.
        if group.rows == 0 {
            self.groups.remove(&key);
        }
        for (i, contribution) in sub.into_iter().enumerate() {
            if let (Some(v), true) = (contribution, self.aggs[i].kind != AggKind::Count) {
                self.charge(i, v);
            }
        }
    }

    pub fn update(&mut self, old: &Record, new: &Record) {
        self.remove(old);
        self.insert(new);
    }

    pub fn group_count(&self) -> usize {
        self.groups.len()
    }

    /// Roughly what this view costs in memory.
    pub fn memory_bytes(&self) -> usize {
        self.groups.len()
            * (std::mem::size_of::<Group>()
                + self.group_by.len() * std::mem::size_of::<Value>()
                + self.aggs.len() * 8)
    }

    /// The answer, in the form the scanning aggregate produces it.
    ///
    /// Group order comes from the map, which orders by the same total `Value`
    /// order the executor's `BTreeMap` does, and ids are positional in both.
    /// Neither of those is a coincidence worth relying on quietly, which is why
    /// there is a test asserting the two paths agree row for row.
    pub fn rows(&self) -> Vec<(RecordId, Record)> {
        // Mirrors the executor's output construction exactly, including the
        // cases where an aggregate is null rather than zero.
        let row = |i: usize, key: &[Value], accs: &[Acc]| {
            let mut rec = Record::new();
            for (g, v) in self.group_by.iter().zip(key) {
                rec.set(g.clone(), v.clone());
            }
            for (a, acc) in self.aggs.iter().zip(accs) {
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
                    // Never materialized; unreachable through `is_maintainable`.
                    AggKind::Min | AggKind::Max => Value::Null,
                };
                rec.set(a.output.clone(), v);
            }
            (RecordId(i as u64), rec)
        };
        let mut out: Vec<(RecordId, Record)> = self
            .groups
            .iter()
            .enumerate()
            .map(|(i, (key, group))| row(i, key, &group.accs))
            .collect();
        // An ungrouped aggregate over no rows is still one row saying zero.
        if out.is_empty() && self.group_by.is_empty() {
            out.push(row(0, &[], &vec![Acc::default(); self.aggs.len()]));
        }
        out
    }
}

/// The set of views the database is keeping.
#[derive(Default)]
pub struct MaterializedViews {
    enabled: bool,
    views: Vec<View>,
}

impl MaterializedViews {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_enabled(&mut self, on: bool) {
        self.enabled = on;
        if !on {
            self.views.clear();
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn len(&self) -> usize {
        self.views.len()
    }

    pub fn is_empty(&self) -> bool {
        self.views.is_empty()
    }

    pub fn memory_bytes(&self) -> usize {
        self.views.iter().map(|v| v.memory_bytes()).sum()
    }

    pub fn clear_collection(&mut self, collection: &str) {
        self.views.retain(|v| v.collection != collection);
    }

    /// Take a query apart into a view definition, if it is one a view can serve.
    ///
    /// A filter disqualifies it: the view holds totals, not rows, so it cannot
    /// answer a question about a subset. So does anything between the scan and
    /// the aggregate, for the same reason.
    pub fn definition_of(op: &LogicalOp) -> Option<(&str, &[String], &[Agg])> {
        let LogicalOp::Aggregate {
            input,
            group_by,
            aggs,
        } = op
        else {
            return None;
        };
        let LogicalOp::Scan { collection, .. } = input.as_ref() else {
            return None;
        };
        is_maintainable(aggs).then_some((collection.as_str(), group_by, aggs))
    }

    pub fn answer(&self, op: &LogicalOp) -> Option<Vec<(RecordId, Record)>> {
        if !self.enabled {
            return None;
        }
        let (collection, group_by, aggs) = Self::definition_of(op)?;
        self.views
            .iter()
            .find(|v| v.matches(collection, group_by, aggs))
            // A view whose arithmetic has stopped being exact declines rather
            // than answering approximately. The caller scans, which is what it
            // would have done had the view never existed.
            .filter(|v| v.is_exact())
            .map(|v| v.rows())
    }

    pub fn has_view_for(&self, op: &LogicalOp) -> bool {
        Self::definition_of(op)
            .is_some_and(|(c, g, a)| self.views.iter().any(|v| v.matches(c, g, a)))
    }

    /// Build a view for this query from the collection's rows.
    ///
    /// Returns whether one was built. Populating from a scan is what makes a
    /// view rebuildable: nothing about it is authoritative, and dropping one
    /// costs the scan that built it.
    pub fn materialize<'a>(
        &mut self,
        op: &LogicalOp,
        rows: impl Iterator<Item = &'a Record>,
    ) -> bool {
        if !self.enabled {
            return false;
        }
        let Some((collection, group_by, aggs)) = Self::definition_of(op) else {
            return false;
        };
        if self
            .views
            .iter()
            .any(|v| v.matches(collection, group_by, aggs))
        {
            return false;
        }
        let mut view = View::new(collection, group_by, aggs);
        for rec in rows {
            view.insert(rec);
        }
        self.views.push(view);
        true
    }

    pub fn on_insert(&mut self, collection: &str, rec: &Record) {
        for v in self.views.iter_mut().filter(|v| v.collection == collection) {
            v.insert(rec);
        }
    }

    pub fn on_remove(&mut self, collection: &str, rec: &Record) {
        for v in self.views.iter_mut().filter(|v| v.collection == collection) {
            v.remove(rec);
        }
    }

    pub fn on_update(&mut self, collection: &str, old: &Record, new: &Record) {
        for v in self.views.iter_mut().filter(|v| v.collection == collection) {
            v.update(old, new);
        }
    }

    /// Whether any view watches this collection, so a caller knows whether the
    /// old record has to be read before a write overwrites it.
    pub fn watches(&self, collection: &str) -> bool {
        self.enabled && self.views.iter().any(|v| v.collection == collection)
    }

    pub fn describe(&self) -> String {
        if self.views.is_empty() {
            return "no materialized views".into();
        }
        self.views
            .iter()
            .map(|v| {
                format!(
                    "{} grouped by [{}] -> {} groups",
                    v.collection,
                    v.group_by.join(", "),
                    v.group_count()
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn count_by_country() -> LogicalOp {
        LogicalOp::scan("users").aggregate(vec!["country".into()], vec![Agg::count("n")])
    }

    fn rec(country: &str) -> Record {
        Record::new().with("country", country)
    }

    fn views() -> MaterializedViews {
        let mut v = MaterializedViews::new();
        v.set_enabled(true);
        v
    }

    fn counts(rows: &[(RecordId, Record)]) -> Vec<(String, u64)> {
        rows.iter()
            .map(|(_, r)| {
                (
                    match r.get("country") {
                        Some(Value::Str(s)) => s.to_string(),
                        other => format!("{other:?}"),
                    },
                    match r.get("n") {
                        Some(Value::U64(n)) => *n,
                        other => panic!("expected a count, got {other:?}"),
                    },
                )
            })
            .collect()
    }

    #[test]
    fn a_view_counts_what_it_was_built_from() {
        let mut v = views();
        let rows = [rec("NO"), rec("SE"), rec("NO"), rec("DK")];
        assert!(v.materialize(&count_by_country(), rows.iter()));
        let answer = v.answer(&count_by_country()).unwrap();
        assert_eq!(
            counts(&answer),
            vec![("DK".into(), 1), ("NO".into(), 2), ("SE".into(), 1)]
        );
    }

    #[test]
    fn maintenance_tracks_inserts_updates_and_deletes() {
        let mut v = views();
        assert!(v.materialize(&count_by_country(), std::iter::empty()));
        v.on_insert("users", &rec("NO"));
        v.on_insert("users", &rec("NO"));
        v.on_insert("users", &rec("SE"));
        assert_eq!(
            counts(&v.answer(&count_by_country()).unwrap()),
            vec![("NO".into(), 2), ("SE".into(), 1)]
        );

        v.on_update("users", &rec("NO"), &rec("SE"));
        assert_eq!(
            counts(&v.answer(&count_by_country()).unwrap()),
            vec![("NO".into(), 1), ("SE".into(), 2)]
        );

        v.on_remove("users", &rec("NO"));
        assert_eq!(
            counts(&v.answer(&count_by_country()).unwrap()),
            vec![("SE".into(), 2)],
            "an emptied group was still reported"
        );
    }

    #[test]
    fn a_group_disappears_rather_than_reporting_zero() {
        // The scanning aggregate emits nothing for a value no row holds. A view
        // that kept the group would answer with a row the scan never produces,
        // which is a divergence even though the number in it is right.
        let mut v = views();
        v.materialize(&count_by_country(), [rec("NO")].iter());
        v.on_remove("users", &rec("NO"));
        assert!(v.answer(&count_by_country()).unwrap().is_empty());
    }

    #[test]
    fn a_missing_field_groups_under_null() {
        let mut v = views();
        let rows = [rec("NO"), Record::new(), Record::new()];
        v.materialize(&count_by_country(), rows.iter());
        let answer = v.answer(&count_by_country()).unwrap();
        assert_eq!(answer.len(), 2);
        // Null sorts first under the total value order the executor also uses.
        assert_eq!(answer[0].1.get("n"), Some(&Value::U64(2)));
    }

    #[test]
    fn counting_a_field_ignores_nulls_but_keeps_the_group() {
        let op = LogicalOp::scan("users").aggregate(
            vec!["country".into()],
            vec![Agg::over(AggKind::Count, "email", "with_email")],
        );
        let mut v = views();
        let rows = [rec("NO").with("email", "a@b"), rec("NO"), rec("SE")];
        v.materialize(&op, rows.iter());
        let answer = v.answer(&op).unwrap();
        assert_eq!(answer.len(), 2, "a group of all-null values vanished");
        // Groups come out in value order, so NO before SE.
        assert_eq!(answer[0].1.get("country"), Some(&Value::from("NO")));
        assert_eq!(answer[0].1.get("with_email"), Some(&Value::U64(1)));
        assert_eq!(answer[1].1.get("country"), Some(&Value::from("SE")));
        assert_eq!(answer[1].1.get("with_email"), Some(&Value::U64(0)));
    }

    #[test]
    fn an_ungrouped_count_of_nothing_is_one_row_saying_zero() {
        let op = LogicalOp::scan("users").aggregate(vec![], vec![Agg::count("n")]);
        let mut v = views();
        v.materialize(&op, std::iter::empty());
        let answer = v.answer(&op).unwrap();
        assert_eq!(answer.len(), 1, "COUNT(*) of nothing produced nothing");
        assert_eq!(answer[0].1.get("n"), Some(&Value::U64(0)));
    }

    #[test]
    fn extremes_are_refused_because_deletion_cannot_maintain_them() {
        // No condition rescues these. Removing the current minimum tells you
        // nothing about the new one without re-reading every remaining value.
        for kind in [AggKind::Min, AggKind::Max] {
            let aggs = vec![Agg::over(kind, "amount", "extreme")];
            assert!(!is_maintainable(&aggs), "{kind:?} was accepted");
            let op = LogicalOp::scan("users").aggregate(vec!["country".into()], aggs);
            assert!(MaterializedViews::definition_of(&op).is_none());
        }
    }

    #[test]
    fn one_unmaintainable_aggregate_disqualifies_the_whole_query() {
        // Answering the count from a view and the minimum from a scan would be
        // two answers to one question, and the row they arrive in cannot say so.
        let op = LogicalOp::scan("users").aggregate(
            vec!["country".into()],
            vec![Agg::count("n"), Agg::over(AggKind::Min, "amount", "low")],
        );
        assert!(MaterializedViews::definition_of(&op).is_none());
    }

    fn sum_by_country() -> LogicalOp {
        LogicalOp::scan("users").aggregate(
            vec!["country".into()],
            vec![Agg::over(AggKind::Sum, "amount", "total")],
        )
    }

    fn amount(country: &str, v: Value) -> Record {
        Record::new().with("country", country).with("amount", v)
    }

    #[test]
    fn an_integer_sum_is_maintained_exactly() {
        let mut v = views();
        let rows = [
            amount("NO", Value::I64(3)),
            amount("NO", Value::I64(-8)),
            amount("SE", Value::U64(1_000_000)),
        ];
        assert!(v.materialize(&sum_by_country(), rows.iter()));
        let got = v.answer(&sum_by_country()).unwrap();
        assert_eq!(got[0].1.get("total"), Some(&Value::F64(-5.0)));
        assert_eq!(got[1].1.get("total"), Some(&Value::F64(1_000_000.0)));

        v.on_insert("users", &amount("NO", Value::I64(5)));
        v.on_remove("users", &amount("NO", Value::I64(-8)));
        let got = v.answer(&sum_by_country()).unwrap();
        assert_eq!(got[0].1.get("total"), Some(&Value::F64(8.0)));
    }

    #[test]
    fn a_fractional_value_makes_the_view_stop_answering() {
        // The moment exactness cannot be guaranteed, the view declines and the
        // query is answered by the scan. Silently approximating is the one
        // outcome that is not allowed.
        let mut v = views();
        v.materialize(&sum_by_country(), [amount("NO", Value::I64(3))].iter());
        assert!(v.answer(&sum_by_country()).is_some());

        v.on_insert("users", &amount("NO", Value::F64(0.1)));
        assert!(
            v.answer(&sum_by_country()).is_none(),
            "a view containing 0.1 still claimed to know the sum"
        );
    }

    #[test]
    fn a_sum_that_outgrows_exact_arithmetic_stops_answering() {
        // Below 2^53 every partial sum of integers is exactly representable and
        // order stops mattering. Above it, it does not, and the view says so
        // rather than drifting.
        let mut v = views();
        v.materialize(&sum_by_country(), std::iter::empty());
        v.on_insert("users", &amount("NO", Value::I64(4_000_000_000_000_000)));
        assert!(v.answer(&sum_by_country()).is_some(), "gave up too early");
        v.on_insert("users", &amount("NO", Value::I64(6_000_000_000_000_000)));
        assert!(
            v.answer(&sum_by_country()).is_none(),
            "kept answering past the point where addition is exact"
        );
    }

    #[test]
    fn the_budget_counts_deletions_too() {
        // A subtraction is as much an operation as an addition. A view that only
        // charged for inserts could be walked past the limit by a long
        // insert-delete cycle while believing it had spent nothing.
        let mut v = views();
        v.materialize(&sum_by_country(), std::iter::empty());
        let big = amount("NO", Value::I64(5_000_000_000_000_000));
        v.on_insert("users", &big);
        assert!(v.answer(&sum_by_country()).is_some());
        v.on_remove("users", &big);
        assert!(
            v.answer(&sum_by_country()).is_none(),
            "deleting a value cost nothing against the exactness budget"
        );
    }

    #[test]
    fn one_inexact_aggregate_disqualifies_the_view() {
        // Answering the count from the view and the sum from a scan is not an
        // option, so the whole view declines.
        let op = LogicalOp::scan("users").aggregate(
            vec!["country".into()],
            vec![Agg::count("n"), Agg::over(AggKind::Sum, "amount", "total")],
        );
        let mut v = views();
        v.materialize(&op, [amount("NO", Value::I64(1))].iter());
        assert!(v.answer(&op).is_some());
        v.on_insert("users", &amount("NO", Value::F64(1.5)));
        assert!(v.answer(&op).is_none());
    }

    #[test]
    fn a_non_numeric_value_contributes_to_neither_sum_nor_exactness() {
        // The scan skips it. The view must skip it identically — and must not
        // treat skipping as a reason to give up, or a text column beside a
        // numeric one would disable every view that mentions it.
        let mut v = views();
        v.materialize(&sum_by_country(), std::iter::empty());
        v.on_insert("users", &amount("NO", Value::from("not a number")));
        v.on_insert("users", &amount("NO", Value::I64(7)));
        let got = v.answer(&sum_by_country()).expect("gave up on a string");
        assert_eq!(got[0].1.get("total"), Some(&Value::F64(7.0)));
    }

    #[test]
    fn a_sum_over_nothing_numeric_is_null_not_zero() {
        let mut v = views();
        v.materialize(&sum_by_country(), std::iter::empty());
        v.on_insert("users", &amount("NO", Value::from("x")));
        let got = v.answer(&sum_by_country()).unwrap();
        assert_eq!(got[0].1.get("total"), Some(&Value::Null));
    }

    #[test]
    fn a_filtered_aggregate_is_not_served_by_a_view() {
        // The view holds totals, not rows, so it cannot answer about a subset.
        use adabt_ir::Expr;
        let op = LogicalOp::scan("users")
            .filter(Expr::eq("country", "NO"))
            .aggregate(vec!["country".into()], vec![Agg::count("n")]);
        assert!(MaterializedViews::definition_of(&op).is_none());
        let mut v = views();
        assert!(!v.materialize(&op, std::iter::empty()));
        assert!(v.answer(&op).is_none());
    }

    #[test]
    fn a_view_answers_only_the_query_it_was_built_for() {
        let mut v = views();
        v.materialize(&count_by_country(), [rec("NO")].iter());
        let other = LogicalOp::scan("users").aggregate(vec!["age".into()], vec![Agg::count("n")]);
        assert!(
            v.answer(&other).is_none(),
            "a view answered another grouping"
        );
        let elsewhere =
            LogicalOp::scan("orders").aggregate(vec!["country".into()], vec![Agg::count("n")]);
        assert!(
            v.answer(&elsewhere).is_none(),
            "a view answered another collection"
        );
    }

    #[test]
    fn disabling_drops_every_view() {
        let mut v = views();
        v.materialize(&count_by_country(), [rec("NO")].iter());
        assert_eq!(v.len(), 1);
        v.set_enabled(false);
        assert!(v.is_empty());
        assert!(v.answer(&count_by_country()).is_none());
    }

    #[test]
    fn nothing_is_materialized_while_disabled() {
        let mut v = MaterializedViews::new();
        assert!(!v.materialize(&count_by_country(), [rec("NO")].iter()));
        assert!(v.answer(&count_by_country()).is_none());
    }

    #[test]
    fn maintenance_of_an_unwatched_collection_changes_nothing() {
        let mut v = views();
        v.materialize(&count_by_country(), [rec("NO")].iter());
        v.on_insert("orders", &rec("NO"));
        assert_eq!(
            counts(&v.answer(&count_by_country()).unwrap()),
            vec![("NO".into(), 1)]
        );
        assert!(v.watches("users"));
        assert!(!v.watches("orders"));
    }
}
