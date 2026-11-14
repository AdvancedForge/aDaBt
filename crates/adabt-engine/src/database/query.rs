//! Planning and executing queries: the plan context, explain, the general
//! query path, joins, cancellation, the slow-query log, and compiled identity
//! lookups.

use adabt_core::error::{Error, Result};
use adabt_core::ids::RecordId;
use adabt_core::index_kind::IndexKind;
use adabt_core::record::Record;
use adabt_core::store::LogicalStore;
use adabt_exec::exec::{execute_with_budget, ExecBudget, ExecStats};
use adabt_exec::physical::PhysicalPlan;
use adabt_exec::planner::{plan as make_plan, PlanContext, PlanDecision};
use adabt_ir::plan::{LogicalOp, LogicalPlan};
use adabt_ir::{QueryKey, QueryShape};
use adabt_telemetry::event::Event;
use adabt_telemetry::Probe;
#[cfg(feature = "loom")]
use loom::sync::Arc;
use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
#[cfg(not(feature = "loom"))]
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::{Database, QueryMode, SlowQueryEvent};
use crate::experiment::Candidates;

/// How many `Join` nodes appear anywhere in `op` — `query_join`'s check for
/// "at most one," since `LogicalOp::contains_join` only ever answers whether
/// there is at least one.
fn count_joins(op: &adabt_ir::plan::LogicalOp) -> usize {
    let this = usize::from(matches!(op, adabt_ir::plan::LogicalOp::Join { .. }));
    this + op.children().iter().map(|c| count_joins(c)).sum::<usize>()
}

/// What a running experiment hides from the planner, and which experiment's
/// candidates it may nonetheless see.
#[derive(Clone, Copy)]
struct PlannerMask<'a> {
    hidden: Option<&'a Candidates>,
    revealed: Option<u64>,
}

impl PlannerMask<'_> {
    /// Whether the planner must not see this index.
    fn hides_index(&self, collection: &str, field: &str, kind: IndexKind) -> bool {
        self.hidden
            .is_some_and(|h| h.hides_index(self.revealed, collection, field, kind))
    }

    /// Whether the planner must not see this collection's column store.
    fn hides_column_store(&self, collection: &str) -> bool {
        self.hidden
            .is_some_and(|h| h.hides_column_store(self.revealed, collection))
    }
}

/// The indexes the planner treats specially rather than as plain field lookups.
#[derive(Default)]
struct SpecialIndexes<'a> {
    composite: HashMap<&'a str, Vec<Vec<String>>>,
    covering: HashMap<&'a str, Vec<adabt_exec::planner::CoveringEntry<'a>>>,
    partial: HashMap<&'a str, Vec<(&'a str, adabt_ir::Expr, IndexKind)>>,
}

impl Database {
    // -- queries -----------------------------------------------------------

    fn plan_context(&self) -> PlanContext<'_> {
        let mask = PlannerMask {
            hidden: self.experiments.masked(),
            revealed: self.experiments.revealed(),
        };
        let indexes = self.visible_indexes(mask);
        let cardinality = self.index_cardinality();
        let columnar = self.visible_column_stores(mask);
        let columnar_fields = self.visible_columnar_fields(mask);
        let SpecialIndexes {
            composite,
            covering,
            partial,
        } = self.special_indexes(mask);
        let row_counts = self.row_counts(indexes.keys().copied().chain(columnar.iter().copied()));
        PlanContext {
            indexes,
            composite,
            covering,
            partial,
            columnar,
            columnar_fields,
            cardinality,
            row_counts,
        }
    }

    /// Every index per collection, as `(field, kind)`, minus those an experiment hides.
    fn visible_indexes(&self, mask: PlannerMask<'_>) -> HashMap<&str, Vec<(&str, IndexKind)>> {
        self.derived
            .indexes
            .iter()
            .map(|(c, list)| {
                let visible = list
                    .iter()
                    .map(|i| (i.field(), i.kind()))
                    .filter(|(f, k)| !mask.hides_index(c, f, *k))
                    .collect();
                (c.as_str(), visible)
            })
            .collect()
    }

    /// The distinct key count of every index, per collection and field.
    fn index_cardinality(&self) -> HashMap<&str, HashMap<&str, u64>> {
        // Selectivity estimates, read where they are cheapest: an index's
        // own key count. Only indexed fields get one, which is the right
        // boundary — the planner consults cardinality to choose between
        // serving structures, never to invent one.
        self.derived
            .indexes
            .iter()
            .map(|(c, list)| {
                let counts = list
                    .iter()
                    .map(|i| (i.field(), i.key_count() as u64))
                    .collect();
                (c.as_str(), counts)
            })
            .collect()
    }

    /// The collections with a column store no experiment hides.
    fn visible_column_stores(&self, mask: PlannerMask<'_>) -> Vec<&str> {
        // Filtered per collection, not wiped wholesale: an experiment
        // trialling a column store on one collection must not blind the
        // planner to another collection's already-promoted one.
        self.derived
            .columns
            .keys()
            .map(|k| k.as_str())
            .filter(|c| !mask.hides_column_store(c))
            .collect()
    }

    /// The fields each visible column store can reconstruct.
    fn visible_columnar_fields(&self, mask: PlannerMask<'_>) -> HashMap<&str, Vec<String>> {
        // And which fields each of those stores can reconstruct — the
        // membership test a top-K decision needs before it may order by a
        // field, because a columnar projection silently omits the rest.
        self.derived
            .columns
            .iter()
            .filter(|(c, _)| !mask.hides_column_store(c))
            .map(|(c, store)| {
                (
                    c.as_str(),
                    store.fields().into_iter().map(String::from).collect(),
                )
            })
            .collect()
    }

    /// The partial, covering and composite indexes, sorted by what they are.
    fn special_indexes(&self, mask: PlannerMask<'_>) -> SpecialIndexes<'_> {
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
        let mut out = SpecialIndexes::default();
        for (c, list) in &self.derived.indexes {
            for i in list {
                let name = i.field();
                if let Some(encoded) = i.condition() {
                    // A condition that will not decode means an index whose
                    // restriction is unknown. Skipping it costs a slower plan;
                    // using it would mean reading a subset as if it were the
                    // whole collection.
                    if let Ok(cond) = crate::exprcodec::decode_expr_hex(encoded) {
                        if !mask.hides_index(c, name, i.kind()) {
                            out.partial
                                .entry(c.as_str())
                                .or_default()
                                .push((name, cond, i.kind()));
                        }
                    }
                } else if !i.covers().is_empty() {
                    if !mask.hides_index(c, name, i.kind()) {
                        out.covering.entry(c.as_str()).or_default().push((
                            name,
                            i.covers().to_vec(),
                            i.kind(),
                        ));
                    }
                } else if name.contains(adabt_index::COMPOSITE_SEP) {
                    out.composite
                        .entry(c.as_str())
                        .or_default()
                        .push(adabt_index::composite_fields(name));
                }
            }
        }
        out
    }

    /// The live record count of each named collection that reports one.
    fn row_counts<'a>(&self, collections: impl Iterator<Item = &'a str>) -> HashMap<&'a str, u64> {
        let mut row_counts = HashMap::new();
        for c in collections {
            if let Some(n) = self.store.live_count(c) {
                row_counts.insert(c, n);
            }
        }
        row_counts
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
        if !self.experiments.running.is_empty() {
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
    /// is a cost-based search problem neither of this project's cost models
    /// covers: `adabt_exec::cost` prices single access paths and `adabt-opt`'s
    /// scoring ranks representations, which are different questions. One join at a time is what "nested-loop and hash
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
        let (rows, stats) = self.execute_physical(&physical)?;
        self.report_if_slow(started, &stats, &rows, |_| physical.explain());
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
        self.execution.pending_cancel = Some(cancel);
        let result = self.query(logical);
        self.execution.pending_cancel = None;
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
        self.execution.slow_query = Some((threshold, Box::new(sink)));
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
        self.execution.slow_query = None;
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
        let path = crate::compiled::CompiledPaths::candidate(&probe, |c| {
            self.derived.direct.contains_key(c)
        })
        .expect("GetById is always a compile candidate");
        self.caches.compiled.install(shape, path);
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
        if let Some(rows) = self.try_compiled_lookup(logical, shape, mode)? {
            return Ok(rows);
        }
        self.consider_compiling(logical, shape, mode);

        let collection = logical.collection().to_string();
        let key = QueryKey::of(&logical.root);
        let epoch = self.caches.epoch(&collection);
        let started = Instant::now();

        if mode.is_counted() {
            self.telemetry.note_filtered_fields(logical);
        }
        if let Some(rows) = self.answer_from_view(logical, &collection, shape, started, mode) {
            return Ok(rows);
        }
        if let Some(rows) = self.probe_result_cache(key, epoch, &collection, shape, started, mode) {
            return Ok(rows);
        }

        let decision = self.plan_decision(logical, shape, mode);
        let physical = adabt_exec::planner::build_from(&logical.root, &decision);
        if mode.is_counted() {
            self.record_index_used(&collection, &decision);
        }
        let (rows, stats) = self.execute_physical(&physical)?;
        // `explain` is only ever built when a query actually crossed the
        // threshold, so a database with no slow-query log configured — the
        // default — pays for exactly what it already paid for `started` and
        // nothing more.
        self.report_if_slow(started, &stats, &rows, |db| db.explain(logical));

        // The query has just been answered by a scan, which is exactly what a
        // view is built from. Building it here means the first such query pays
        // what it was going to pay anyway and every later one pays nothing.
        self.maybe_materialize(&logical.root)?;
        if mode.uses_caches() {
            self.caches
                .results
                .insert(key, &collection, epoch, || rows.clone());
            self.caches
                .results
                .note_would_insert(key, &collection, epoch, &rows);
        }
        if mode.is_counted() {
            self.telemetry
                .observe_shape(&collection, shape, started, rows.len() as u64);
        }
        Ok(rows)
    }

    /// Answer a query whose shape has a compiled identity-lookup path installed.
    fn try_compiled_lookup(
        &mut self,
        logical: &LogicalPlan,
        shape: QueryShape,
        mode: QueryMode,
    ) -> Result<Option<Vec<(RecordId, Record)>>> {
        if !mode.uses_caches() {
            return Ok(None);
        }
        let adabt_ir::plan::LogicalOp::GetById { collection, id } = &logical.root else {
            return Ok(None);
        };
        let Some(call) = self.caches.compiled.enter(shape) else {
            return Ok(None);
        };
        let started = call.timed.then(Instant::now);
        let rec = match self.derived.direct.get(collection) {
            Some(d) => d.get(*id)?,
            None => self.store.get(collection, *id)?,
        };
        let rows: Vec<_> = rec.map(|r| (*id, r)).into_iter().collect();
        let nanos = started.map(|t| t.elapsed().as_nanos() as u64);
        self.caches
            .compiled
            .leave(shape, call, rows.len() as u64, nanos);
        Ok(Some(rows))
    }

    /// Count this shape towards compilation, installing a compiled path once it is hot.
    fn consider_compiling(&mut self, logical: &LogicalPlan, shape: QueryShape, mode: QueryMode) {
        // Worth specialising? Decided once, not per call.
        if mode.uses_caches() && self.caches.compiled.observe(shape) {
            let direct = &self.derived.direct;
            if let Some(path) =
                crate::compiled::CompiledPaths::candidate(&logical.root, |c| direct.contains_key(c))
            {
                self.caches.compiled.install(shape, path);
            }
        }
    }

    /// Answer the query from a materialized view, if one covers it.
    fn answer_from_view(
        &mut self,
        logical: &LogicalPlan,
        collection: &str,
        shape: QueryShape,
        started: Instant,
        mode: QueryMode,
    ) -> Option<Vec<(RecordId, Record)>> {
        // A materialized view is a derived representation, not a cache, so it is
        // consulted even under a trial — masking it is the experiment's job, not
        // this function's.
        let rows = self.derived.views.answer(&logical.root)?;
        if mode.is_counted() {
            self.telemetry
                .observe_shape(collection, shape, started, rows.len() as u64);
        }
        Some(rows)
    }

    /// Answer the query from the result cache, recording the probe either way.
    fn probe_result_cache(
        &mut self,
        key: QueryKey,
        epoch: u64,
        collection: &str,
        shape: QueryShape,
        started: Instant,
        mode: QueryMode,
    ) -> Option<Vec<(RecordId, Record)>> {
        if !mode.uses_caches() {
            return None;
        }
        if let Some(rows) = self.caches.results.get(key, epoch) {
            let rows = rows.clone();
            self.telemetry.probe.record(Event::CacheProbe {
                name: "result_cache",
                hit: true,
            });
            self.telemetry
                .observe_shape(collection, shape, started, rows.len() as u64);
            return Some(rows);
        }
        self.telemetry.probe.record(Event::CacheProbe {
            name: "result_cache",
            hit: false,
        });
        // While the cache is off, ask what it would have done: the evidence
        // the optimizer needs to decide whether turning it on would pay.
        if mode.is_counted() && !self.caches.results.enabled() {
            let hit = self.caches.results.would_hit(key, epoch);
            self.telemetry.probe.record(Event::CacheProbe {
                name: crate::caches::RESULT_CACHE_GHOST,
                hit,
            });
        }
        None
    }

    /// The access decision for this query, from the plan cache or freshly made.
    fn plan_decision(
        &mut self,
        logical: &LogicalPlan,
        shape: QueryShape,
        mode: QueryMode,
    ) -> PlanDecision {
        // The cache holds a shape-invariant *decision*; the plan is rebuilt
        // around this query's literals every time. Caching the plan itself would
        // let one query be answered with another's literals.
        //
        // It is also bypassed under a trial, and for a different reason: the
        // decision depends on which structures are visible, and the two sides of
        // an experiment differ in exactly that while sharing a shape. A cached
        // decision would hand the baseline's plan to the candidate and the
        // comparison would be of a thing against itself.
        match self.caches.plans.get(shape).filter(|_| mode.uses_caches()) {
            Some(d) => {
                let d = d.clone();
                self.telemetry.probe.record(Event::CacheProbe {
                    name: "plan_cache",
                    hit: true,
                });
                d
            }
            None => {
                if mode.uses_caches() {
                    self.telemetry.probe.record(Event::CacheProbe {
                        name: "plan_cache",
                        hit: false,
                    });
                }
                let d = adabt_exec::planner::decide(&logical.root, &self.plan_context());
                if mode.uses_caches() {
                    self.caches.plans.insert(shape, || d.clone());
                }
                d
            }
        }
    }

    /// Tell telemetry which index, if any, the chosen plan reads through.
    fn record_index_used(&mut self, collection: &str, decision: &PlanDecision) {
        // Record the access path actually chosen. An index nobody picks is
        // invisible to any amount of watching queries arrive.
        if let adabt_exec::planner::AccessDecision::IndexLookup { field, .. }
        | adabt_exec::planner::AccessDecision::IndexRange { field } = &decision.access
        {
            self.telemetry
                .probe
                .record(Event::IndexUsed { collection, field });
        }
    }

    /// Execute a physical plan under the query budget, keeping its stats as the last run's.
    fn execute_physical(
        &mut self,
        physical: &PhysicalPlan,
    ) -> Result<(Vec<(RecordId, Record)>, ExecStats)> {
        let mut stats = ExecStats::default();
        let budget = ExecBudget {
            max_ram_bytes: self.policy.constraints.max_query_ram_bytes,
            cancel: self.execution.pending_cancel.clone(),
        };
        let rows = execute_with_budget(physical, self, &mut stats, &budget)?;
        self.execution.last_stats = stats;
        Ok((rows, stats))
    }

    /// Send a query that took at least the slow-query threshold to the sink.
    fn report_if_slow(
        &mut self,
        started: Instant,
        stats: &ExecStats,
        rows: &[(RecordId, Record)],
        explain: impl FnOnce(&Self) -> String,
    ) {
        // Built in two passes because `explain` needs `&self` and the sink
        // lives behind `&mut self.execution.slow_query` — not overlapping
        // fields to the borrow checker once either goes through a method call.
        if let Some((threshold, _)) = &self.execution.slow_query {
            let elapsed = started.elapsed();
            if elapsed >= *threshold {
                let event = SlowQueryEvent {
                    elapsed,
                    rows_scanned: stats.rows_scanned,
                    rows_returned: rows.len() as u64,
                    explain: explain(self),
                };
                if let Some((_, sink)) = &mut self.execution.slow_query {
                    sink(&event);
                }
            }
        }
    }
}
