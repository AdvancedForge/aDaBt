//! What the database reports about its own workload: operation timings, which
//! fields queries filter and project on, and sampled record touches.

use adabt_core::ids::RecordId;
use adabt_ir::plan::LogicalPlan;
use adabt_ir::QueryShape;
use adabt_telemetry::event::{Event, OpKind};
use adabt_telemetry::{CollectingProbe, Probe};
#[cfg(feature = "loom")]
use loom::sync::Arc;
use std::cell::Cell;
#[cfg(not(feature = "loom"))]
use std::sync::Arc;
use std::time::Instant;

/// Whether `predicate` constrains `field` with at least one ordered
/// comparison — a range want, as distinct from an equality pin.
///
/// Local mirror of the planner's range walk, kept deliberately shallow: this
/// only decides whether a projection observation is worth recording against
/// a b-tree-backed structure, not what the bounds are.
fn range_constrained(predicate: &adabt_ir::Expr, field: &str) -> bool {
    match predicate {
        adabt_ir::Expr::Compare { op, lhs, .. } => {
            matches!(
                op,
                adabt_ir::CmpOp::Ge
                    | adabt_ir::CmpOp::Gt
                    | adabt_ir::CmpOp::Le
                    | adabt_ir::CmpOp::Lt
            ) && matches!(lhs.as_ref(), adabt_ir::Expr::Field(f) if f == field)
        }
        adabt_ir::Expr::And(parts) => parts.iter().any(|p| range_constrained(p, field)),
        _ => false,
    }
}

/// The probe every observation is recorded into, and the sampling state that
/// keeps the cheapest of them off the hot path.
pub(super) struct Telemetry {
    pub(super) probe: Arc<CollectingProbe>,
    /// Sampling stride for temperature observation. Feeding the sketch on
    /// every read would add a lock to the hot path to measure something only
    /// relative frequency is wanted from, and uniform sampling preserves that.
    touch_sample: Cell<u32>,
}

impl Telemetry {
    pub(super) fn new() -> Self {
        Self {
            probe: Arc::new(CollectingProbe::new()),
            touch_sample: Cell::new(0),
        }
    }

    /// Report which fields a query filtered on, and how.
    ///
    /// Equality and range wants are recorded separately because they want
    /// different index structures, and a driver that cannot tell them apart
    /// builds hash indexes for range predicates.
    pub(super) fn note_filtered_fields(&self, logical: &LogicalPlan) {
        let collection = logical.collection();
        let mut op = Some(&logical.root);
        // Gathered across the whole walk rather than at the filter node:
        // the projection may sit above sorts and limits, and what a covering
        // index needs to know is which output fields travelled with this
        // predicate, wherever they were asked for.
        let mut equalities_all: Vec<Vec<String>> = Vec::new();
        let mut ranges: Vec<String> = Vec::new();
        let mut projected: Vec<String> = Vec::new();
        while let Some(o) = op {
            match o {
                adabt_ir::plan::LogicalOp::Filter { predicate, .. } => {
                    let mut equalities: Vec<String> = predicate
                        .equality_constraints()
                        .into_iter()
                        .map(|(f, _)| f)
                        .collect();
                    // Sorted and de-duplicated so that `country AND age` and
                    // `age AND country` are recognised as the same shape. A
                    // composite index over a set does not care which order the
                    // predicate wrote them in, and counting the two separately
                    // would halve the evidence for building one.
                    equalities.sort();
                    equalities.dedup();
                    let mut all = Vec::new();
                    predicate.referenced_fields(&mut all);
                    for f in all {
                        // Constrained but not to a literal: range-filtered,
                        // which is the covering question asked of a b-tree
                        // rather than a hash.
                        let equality = equalities.contains(&f);
                        if !equality && !ranges.contains(&f) && range_constrained(predicate, &f) {
                            ranges.push(f.clone());
                        }
                        self.probe.record(Event::FieldFiltered {
                            collection,
                            field: &f,
                            equality,
                        });
                    }
                    if equalities.len() > 1 {
                        self.probe.record(Event::FieldsPinnedTogether {
                            collection,
                            fields: &equalities,
                        });
                    }
                    equalities_all.push(equalities);
                }
                adabt_ir::plan::LogicalOp::Project { fields, .. } => {
                    projected.extend(fields.iter().cloned());
                }
                _ => {}
            }
            op = o.child();
        }
        if projected.is_empty() || (equalities_all.is_empty() && ranges.is_empty()) {
            return;
        }
        projected.sort();
        projected.dedup();
        // One observation per filtered field, projection minus that field —
        // the index carries its own key, so asking it to "cover" the filtered
        // field would double-count a column it stores anyway. Equality and
        // range observations stay apart: they want differently-backed
        // indexes.
        let mut seen: Vec<(String, bool)> = Vec::new();
        for eqs in &equalities_all {
            for f in eqs {
                if !seen.iter().any(|(s, _)| s == f) {
                    seen.push((f.clone(), true));
                }
            }
        }
        ranges.sort();
        for f in &ranges {
            if !seen.iter().any(|(s, _)| s == f) {
                seen.push((f.clone(), false));
            }
        }
        seen.sort();
        for (f, equality) in seen {
            let covers: Vec<String> = projected.iter().filter(|p| *p != &f).cloned().collect();
            self.probe.record(Event::FieldsProjectedTogether {
                collection,
                filtered: &f,
                fields: &covers,
                equality,
            });
        }
    }

    /// Feed the temperature sketch, on a sampled basis.
    pub(super) fn note_touch(&self, collection: &str, id: RecordId) {
        const STRIDE: u32 = 16;
        let n = self.touch_sample.get().wrapping_add(1);
        self.touch_sample.set(n);
        if n % STRIDE == 0 {
            self.probe.record(Event::Touch { collection, id });
        }
    }

    pub(super) fn observe_shape(
        &self,
        collection: &str,
        shape: QueryShape,
        started: Instant,
        rows: u64,
    ) {
        self.probe.record(Event::Op {
            collection,
            kind: OpKind::Scan,
            shape: adabt_telemetry::QueryShape(shape.0),
            nanos: started.elapsed().as_nanos() as u64,
            rows,
        });
    }

    pub(super) fn observe(&self, collection: &str, kind: OpKind, started: Instant, rows: u64) {
        self.probe.record(Event::Op {
            collection,
            kind,
            shape: adabt_telemetry::QueryShape::UNKNOWN,
            nanos: started.elapsed().as_nanos() as u64,
            rows,
        });
    }
}
