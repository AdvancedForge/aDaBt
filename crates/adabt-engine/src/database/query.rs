//! Planning and executing queries: the plan context, explain, filtered-field
//! and page-touch bookkeeping, the general query path, the slow-query log,
//! compiled identity lookups, and per-operation telemetry observation.

use adabt_core::error::{Error, Result};
use adabt_core::ids::RecordId;
use adabt_core::index_kind::IndexKind;
use adabt_core::record::Record;
use adabt_core::store::LogicalStore;
use adabt_exec::exec::{execute_with_budget, ExecBudget, ExecStats};
use adabt_exec::physical::PhysicalPlan;
use adabt_exec::planner::{plan as make_plan, PlanContext};
use adabt_ir::plan::{LogicalOp, LogicalPlan};
use adabt_ir::{QueryKey, QueryShape};
use adabt_telemetry::event::{Event, OpKind};
use adabt_telemetry::Probe;
#[cfg(feature = "loom")]
use loom::sync::Arc;
use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
#[cfg(not(feature = "loom"))]
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::experiment::Candidates;

use super::{Database, QueryMode, SlowQueryEvent};

/// How many `Join` nodes appear anywhere in `op` — `query_join`'s check for
/// "at most one," since `LogicalOp::contains_join` only ever answers whether
/// there is at least one.
fn count_joins(op: &adabt_ir::plan::LogicalOp) -> usize {
    let this = usize::from(matches!(op, adabt_ir::plan::LogicalOp::Join { .. }));
    this + op.children().iter().map(|c| count_joins(c)).sum::<usize>()
}

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

impl Database {
    // -- queries -----------------------------------------------------------

    /// Whether a structure built for an experiment is off-limits to this query.
    ///
    /// The mask applies to *everything* the planner can see, so a candidate that
    /// is hidden is genuinely invisible rather than merely deprioritised. An
    /// index the planner can still find is an index it will use.
    pub(super) fn masked(&self) -> Option<&Candidates> {
        if self.hidden.is_empty() {
            None
        } else {
            Some(&self.hidden)
        }
    }

    /// The one experiment whose candidate this query may see, if any.
    ///
    /// `candidate_visible` says a candidate side is being served;
    /// `experiment_under_test` says whose. Neither alone is enough: the first
    /// on its own revealed every running experiment's structures at once.
    pub(super) fn revealed(&self) -> Option<u64> {
        if self.candidate_visible {
            self.experiment_under_test
        } else {
            None
        }
    }

    fn plan_context(&self) -> PlanContext<'_> {
        let masked = self.masked();
        let revealed = self.revealed();
        let mut m = HashMap::new();
        // Selectivity estimates, read where they are cheapest: an index's
        // own key count. Only indexed fields get one, which is the right
        // boundary — the planner consults cardinality to choose between
        // serving structures, never to invent one.
        let mut card: HashMap<&str, HashMap<&str, u64>> = HashMap::new();
        for (c, list) in &self.indexes {
            m.insert(
                c.as_str(),
                list.iter()
                    .map(|i| (i.field(), i.kind()))
                    .filter(|(f, k)| !masked.is_some_and(|h| h.hides_index(revealed, c, f, *k)))
                    .collect(),
            );
            card.insert(
                c.as_str(),
                list.iter()
                    .map(|i| (i.field(), i.key_count() as u64))
                    .collect(),
            );
        }
        // Filtered per collection, not wiped wholesale: an experiment
        // trialling a column store on one collection must not blind the
        // planner to another collection's already-promoted one.
        let columnar: Vec<&str> = self
            .columns
            .keys()
            .map(|k| k.as_str())
            .filter(|c| !masked.is_some_and(|h| h.hides_column_store(revealed, c)))
            .collect();
        // And which fields each of those stores can reconstruct — the
        // membership test a top-K decision needs before it may order by a
        // field, because a columnar projection silently omits the rest.
        let columnar_fields: HashMap<&str, Vec<String>> = self
            .columns
            .iter()
            .filter(|(c, _)| !masked.is_some_and(|h| h.hides_column_store(revealed, c)))
            .map(|(c, store)| {
                (
                    c.as_str(),
                    store.fields().into_iter().map(String::from).collect(),
                )
            })
            .collect();
        // Composite indexes, recognised by their NUL-joined name. Reported
        // separately because the planner asks a different question of them.
        //
        // The covering check comes first and is not optional. A covering
        // index over two or more fields is named `f\u{1}a\u{0}b`, which also
        // contains a NUL — so without this guard one would be read as a
        // *composite* index over the fields `f\u{1}a` and `b`, neither of
        // which any record has. The planner would then choose it for a
        // predicate it cannot serve and get nothing back. Exactly the failure
        // the composite restore path already had once, in a new place.
        let mut composite: HashMap<&str, Vec<Vec<String>>> = HashMap::new();
        let mut covering: HashMap<&str, Vec<adabt_exec::planner::CoveringEntry>> = HashMap::new();
        let mut partial: HashMap<&str, Vec<(&str, adabt_ir::Expr, IndexKind)>> = HashMap::new();
        for (c, list) in &self.indexes {
            for i in list {
                let name = i.field();
                if let Some(encoded) = i.condition() {
                    // A condition that will not decode means an index whose
                    // restriction is unknown. Skipping it costs a slower plan;
                    // using it would mean reading a subset as if it were the
                    // whole collection.
                    if let Ok(cond) = crate::exprcodec::decode_expr_hex(encoded) {
                        if !masked.is_some_and(|h| h.hides_index(revealed, c, name, i.kind())) {
                            partial
                                .entry(c.as_str())
                                .or_default()
                                .push((name, cond, i.kind()));
                        }
                    }
                } else if !i.covers().is_empty() {
                    if !masked.is_some_and(|h| h.hides_index(revealed, c, name, i.kind())) {
                        covering.entry(c.as_str()).or_default().push((
                            name,
                            i.covers().to_vec(),
                            i.kind(),
                        ));
                    }
                } else if name.contains(adabt_index::COMPOSITE_SEP) {
                    composite
                        .entry(c.as_str())
                        .or_default()
                        .push(adabt_index::composite_fields(name));
                }
            }
        }
        let mut row_counts: HashMap<&str, u64> = HashMap::new();
        for c in m.keys().copied().chain(columnar.iter().copied()) {
            if let Some(n) = self.store.live_count(c) {
                row_counts.insert(c, n);
            }
        }
        PlanContext {
            indexes: m,
            composite,
            covering,
            partial,
            columnar,
            columnar_fields,
            cardinality: card,
            row_counts,
        }
    }

    pub fn plan(&self, logical: &LogicalPlan) -> PhysicalPlan {
        make_plan(&logical.root, &self.plan_context())
    }

    pub fn explain(&self, logical: &LogicalPlan) -> String {
        format!(
            "logical:\n{}\nphysical:\n{}",
            logical.explain(),
            self.plan(logical).explain()
        )
    }

    /// Report which fields a query filtered on, and how.
    ///
    /// Equality and range wants are recorded separately because they want
    /// different index structures, and a driver that cannot tell them apart
    /// builds hash indexes for range predicates.
    fn note_filtered_fields(&self, logical: &LogicalPlan) {
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

    pub fn query(&mut self, logical: &LogicalPlan) -> Result<Vec<(RecordId, Record)>> {
        // A join takes its own, much smaller path — see `query_join` — rather
        // than entering the machinery below at all: the plan cache, result
        // cache, materialized-view lookup and compiled shortcuts are all
        // keyed or reasoned about in terms of one collection with one epoch,
        // and `logical.collection()` (the first thing `query_in` calls)
        // panics outright on a plan with more than one source. Checked before
        // the experiment branch too, so a join can never accidentally enter
        // shadow or canary execution, which has nothing built for it either.
        if logical.root.contains_join() {
            return self.query_join(logical);
        }
        if !self.experiments.is_empty() {
            return self.experiment_query(logical);
        }
        self.query_in(logical, QueryMode::Normal)
    }

    /// A plan containing a `Join`, executed directly: planned fresh every
    /// call (no plan cache — a per-shape cached decision was never going to
    /// mean anything for two independent scan sides), not checked against
    /// the result cache or materialized views (nothing maintains either for
    /// a join yet), and its index usage does not reach the adaptive driver's
    /// telemetry (so a join's own access pattern does not yet teach the
    /// optimizer anything about indexing either side — a real, narrower gap
    /// than not joining at all, and a separable one).
    ///
    /// Refuses a plan with more than one `Join` node anywhere in it: each
    /// side of a join is planned and executed independently, by recursing
    /// into the ordinary single-collection machinery, which requires that
    /// neither side contain a join of its own. Real multi-way join planning —
    /// choosing an execution order among the n! orderings of an n-way join —
    /// is a cost-based search problem this project has no cost model for yet
    /// outside `adabt-opt`'s representation-choice scoring, which answers a
    /// different question. One join at a time is what "nested-loop and hash
    /// join" in the plan text asks for; ordering among several is real,
    /// separable, future work.
    fn query_join(&mut self, logical: &LogicalPlan) -> Result<Vec<(RecordId, Record)>> {
        if count_joins(&logical.root) > 1 {
            return Err(Error::Unsupported(
                "nested or multi-way joins are not implemented yet; a query may contain one Join"
                    .into(),
            ));
        }
        let started = Instant::now();
        let physical = adabt_exec::planner::plan(&logical.root, &self.plan_context());
        let mut stats = ExecStats::default();
        let budget = ExecBudget {
            max_ram_bytes: self.policy.constraints.max_query_ram_bytes,
            cancel: self.pending_cancel.clone(),
        };
        let rows = execute_with_budget(&physical, self, &mut stats, &budget)?;
        self.last_stats = stats;
        if let Some((threshold, _)) = &self.slow_query {
            let elapsed = started.elapsed();
            if elapsed >= *threshold {
                let event = SlowQueryEvent {
                    elapsed,
                    rows_scanned: stats.rows_scanned,
                    rows_returned: rows.len() as u64,
                    explain: physical.explain(),
                };
                if let Some((_, sink)) = &mut self.slow_query {
                    sink(&event);
                }
            }
        }
        Ok(rows)
    }

    /// [`Self::query`], but another thread can stop it early by setting
    /// `cancel`.
    ///
    /// There is no timer in here — setting `cancel` is the whole mechanism, so
    /// a caller who wants "stop after 5 seconds" spawns a thread that sleeps 5
    /// seconds and then sets the flag this call was given. What stops is
    /// whatever the query is doing when it next polls the flag: a scan or sort
    /// checks it every few thousand rows (see `adabt_exec::exec`), so an
    /// already-fast query still just returns its answer, and only a query that
    /// was going to take a while is actually shortened.
    ///
    /// A cancelled query returns `Err(Error::Cancelled(_))` having written
    /// nothing — every read path here reads before it decides whether to keep
    /// going, never the reverse.
    pub fn query_cancellable(
        &mut self,
        logical: &LogicalPlan,
        cancel: Arc<AtomicBool>,
    ) -> Result<Vec<(RecordId, Record)>> {
        self.pending_cancel = Some(cancel);
        let result = self.query(logical);
        self.pending_cancel = None;
        result
    }

    /// Call `sink` for every query whose end-to-end wall-clock time — cache
    /// probes and planning included, not just the executor's own work — is
    /// at least `threshold`.
    ///
    /// Applies to the general query path only; the compiled and direct-
    /// lookup shortcuts `query_in` takes for a hot identity read never reach
    /// this check, on the reasoning that an O(1) lookup is not the kind of
    /// query a slow-query log exists to surface, and always timing it just to
    /// confirm that would spend the exact per-query cost this feature is
    /// opt-in specifically to avoid.
    pub fn set_slow_query_sink(
        &mut self,
        threshold: Duration,
        sink: impl FnMut(&SlowQueryEvent) + Send + 'static,
    ) {
        self.slow_query = Some((threshold, Box::new(sink)));
    }

    /// [`Self::set_slow_query_sink`] with a sink that writes one line to
    /// stderr — the same "no dependency, no configuration" default this
    /// project's other operational logging (the server's own start/shutdown
    /// messages) already uses.
    pub fn enable_slow_query_log(&mut self, threshold: Duration) {
        self.set_slow_query_sink(threshold, |event| {
            eprintln!(
                "slow query: {:?}, {} rows scanned, {} returned\n{}",
                event.elapsed, event.rows_scanned, event.rows_returned, event.explain
            );
        });
    }

    pub fn disable_slow_query_log(&mut self) {
        self.slow_query = None;
    }

    /// Force the identity-lookup compiled path for `collection` on immediately,
    /// rather than waiting for `compiled::HOT_THRESHOLD` calls to observe it.
    ///
    /// This is not "compile an arbitrary query shape": `CompiledPaths::candidate`
    /// only ever recognizes one shape, a bare `GetById`, by design — everything
    /// else genuinely has work left to do that skipping the general path would
    /// mean reimplementing (see `compiled.rs`'s own module docs). An expert
    /// naming "compile shape X" can only ever mean this one shape, for any
    /// collection; there is no other shape this mechanism is able to compile,
    /// forced or not. What forcing buys is real but narrow: a workload doing a
    /// handful of expensive identity lookups — fewer than the threshold —
    /// still gets the specialised path from the first call instead of waiting
    /// to earn it.
    pub fn compile_identity_lookups(&mut self, collection: &str) -> Result<()> {
        self.store.schema_of(collection)?;
        // Any id produces the same shape: `QueryShape` is structural and
        // erases literals by construction, which is the entire reason a
        // shape can be cached and rebound to a different id later.
        let probe = LogicalOp::get(collection, RecordId(0));
        let shape = LogicalPlan::new(probe.clone()).shape();
        let path =
            crate::compiled::CompiledPaths::candidate(&probe, |c| self.direct.contains_key(c))
                .expect("GetById is always a compile candidate");
        self.compiled.install(shape, path);
        Ok(())
    }

    pub(super) fn query_in(
        &mut self,
        logical: &LogicalPlan,
        mode: QueryMode,
    ) -> Result<Vec<(RecordId, Record)>> {
        let shape = logical.shape();

        // The specialised path, taken before any of the general machinery. For
        // a hot identity lookup this is the whole query: no filter accounting,
        // no cache probes, no plan, no operators, no batching.
        if mode.uses_caches() && self.compiled.get(shape).is_some() {
            if let adabt_ir::plan::LogicalOp::GetById { collection, id } = &logical.root {
                let (collection, id) = (collection.clone(), *id);
                let rec = match self.direct.get(&collection) {
                    Some(d) => d.get(id)?,
                    None => self.store.get(&collection, id)?,
                };
                return Ok(rec.map(|r| (id, r)).into_iter().collect());
            }
        }
        // Worth specialising? Decided once, not per call.
        if mode.uses_caches() && self.compiled.observe(shape) {
            let direct = &self.direct;
            if let Some(path) =
                crate::compiled::CompiledPaths::candidate(&logical.root, |c| direct.contains_key(c))
            {
                self.compiled.install(shape, path);
            }
        }

        let collection = logical.collection().to_string();
        let key = QueryKey::of(&logical.root);
        let epoch = self.epoch(&collection);
        let started = Instant::now();

        if mode.is_counted() {
            self.note_filtered_fields(logical);
        }

        // A materialized view is a derived representation, not a cache, so it is
        // consulted even under a trial — masking it is the experiment's job, not
        // this function's.
        if let Some(rows) = self.views.answer(&logical.root) {
            if mode.is_counted() {
                self.observe_shape(&collection, shape, started, rows.len() as u64);
            }
            return Ok(rows);
        }

        if mode.uses_caches() {
            if let Some(rows) = self.result_cache.get(key, epoch) {
                let rows = rows.clone();
                self.probe.record(Event::CacheProbe {
                    name: "result_cache",
                    hit: true,
                });
                self.observe_shape(&collection, shape, started, rows.len() as u64);
                return Ok(rows);
            }
            self.probe.record(Event::CacheProbe {
                name: "result_cache",
                hit: false,
            });
        }

        // The cache holds a shape-invariant *decision*; the plan is rebuilt
        // around this query's literals every time. Caching the plan itself would
        // let one query be answered with another's literals.
        //
        // It is also bypassed under a trial, and for a different reason: the
        // decision depends on which structures are visible, and the two sides of
        // an experiment differ in exactly that while sharing a shape. A cached
        // decision would hand the baseline's plan to the candidate and the
        // comparison would be of a thing against itself.
        let decision = match self.plan_cache.get(shape).filter(|_| mode.uses_caches()) {
            Some(d) => {
                let d = d.clone();
                self.probe.record(Event::CacheProbe {
                    name: "plan_cache",
                    hit: true,
                });
                d
            }
            None => {
                if mode.uses_caches() {
                    self.probe.record(Event::CacheProbe {
                        name: "plan_cache",
                        hit: false,
                    });
                }
                let d = adabt_exec::planner::decide(&logical.root, &self.plan_context());
                if mode.uses_caches() {
                    self.plan_cache.insert(shape, || d.clone());
                }
                d
            }
        };
        let physical = adabt_exec::planner::build_from(&logical.root, &decision);
        // Record the access path actually chosen. An index nobody picks is
        // invisible to any amount of watching queries arrive.
        if mode.is_counted() {
            if let adabt_exec::planner::AccessDecision::IndexLookup { field, .. }
            | adabt_exec::planner::AccessDecision::IndexRange { field } = &decision.access
            {
                self.probe.record(Event::IndexUsed {
                    collection: &collection,
                    field,
                });
            }
        }

        let mut stats = ExecStats::default();
        let budget = ExecBudget {
            max_ram_bytes: self.policy.constraints.max_query_ram_bytes,
            cancel: self.pending_cancel.clone(),
        };
        let rows = execute_with_budget(&physical, self, &mut stats, &budget)?;
        self.last_stats = stats;
        // `explain` is only ever built when a query actually crossed the
        // threshold, so a database with no slow-query log configured — the
        // default — pays for exactly what it already paid for `started` and
        // nothing more. Built in two passes because `self.explain` needs
        // `&self` and the sink lives behind `&mut self.slow_query` — not
        // overlapping fields to the borrow checker once either goes through
        // a method call.
        if let Some((threshold, _)) = &self.slow_query {
            let elapsed = started.elapsed();
            if elapsed >= *threshold {
                let event = SlowQueryEvent {
                    elapsed,
                    rows_scanned: stats.rows_scanned,
                    rows_returned: rows.len() as u64,
                    explain: self.explain(logical),
                };
                if let Some((_, sink)) = &mut self.slow_query {
                    sink(&event);
                }
            }
        }
        // The query has just been answered by a scan, which is exactly what a
        // view is built from. Building it here means the first such query pays
        // what it was going to pay anyway and every later one pays nothing.
        self.maybe_materialize(&logical.root)?;
        if mode.uses_caches() {
            self.result_cache
                .insert(key, &collection, epoch, || rows.clone());
        }
        if mode.is_counted() {
            self.observe_shape(&collection, shape, started, rows.len() as u64);
        }
        Ok(rows)
    }

    fn observe_shape(&self, collection: &str, shape: QueryShape, started: Instant, rows: u64) {
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
