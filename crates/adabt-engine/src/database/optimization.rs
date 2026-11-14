//! Driving the optimization controller: policy and level changes,
//! `optimize`/`optimize_verified`, proposing and running decisions, and
//! gathering the inputs the controller reasons over.

use adabt_core::error::Result;
use adabt_core::index_kind::IndexKind;
use adabt_core::policy::{Mode, Policy};
use adabt_core::schema::SchemaMode;
use adabt_core::store::LogicalStore;
use adabt_opt::controller::ApplyReport;
use adabt_opt::decision::{Decision, DecisionAction, Source as DecisionSource};
use adabt_opt::driver::{ManualDriver, OptimizationDriver};
use adabt_opt::experiment::Guardrails;
use adabt_opt::optimization::RecordBytes;
use adabt_opt::{OptimizationConfig, OptimizationController, Registry};
use adabt_storage::heap::RecordFootprint;
use adabt_telemetry::{Event, Probe};

use super::{Database, OptInputs};

impl Database {
    pub fn registry(&self) -> &Registry {
        &self.optimizer.registry
    }

    pub fn config(&self) -> &OptimizationConfig {
        self.optimizer.controller.config()
    }

    // -- optimization ------------------------------------------------------

    /// Change the policy and bring the physical configuration in line with it.
    pub fn set_policy(&mut self, policy: Policy) -> Result<ApplyReport> {
        self.policy = policy;
        self.optimize()
    }

    pub fn set_level(&mut self, level: u8) -> Result<ApplyReport> {
        let overrides = match &self.policy.mode {
            Mode::Manual { overrides, .. } => overrides.clone(),
            Mode::Adaptive => Vec::new(),
        };
        self.policy.mode = Mode::Manual { level, overrides };
        self.optimize()
    }

    /// Run one optimization cycle: ask the driver what should change, then put
    /// it through the controller.
    ///
    /// Manual and adaptive differ only in which driver is consulted. Everything
    /// after that — gating, application, logging — is one path.
    pub fn optimize(&mut self) -> Result<ApplyReport> {
        let inputs = self.opt_inputs()?;
        let decisions = self.propose(&inputs);
        let source = self.decision_source();
        let report = self.run_decisions(decisions, source, inputs);
        self.forget_a_little();
        report
    }

    /// Let telemetry forget a little of what it has counted.
    ///
    /// Once per cycle, and only in adaptive mode. Cumulative counters answer
    /// "was this ever useful"; the driver needs "is this useful now", and the
    /// difference is the whole of whether the optimizer can follow a workload
    /// that changes or merely accumulate structures for workloads that have
    /// ended.
    ///
    /// Manual mode does not forget, because nothing in manual mode is deciding
    /// anything from the counts and a user reading them wants the totals.
    fn forget_a_little(&self) {
        if matches!(self.policy.mode, Mode::Adaptive) {
            let (n, d) = adabt_opt::adaptive::TELEMETRY_DECAY;
            self.telemetry.probe.decay(n, d);
        }
    }

    /// Run one optimization cycle, proving whatever can be proved.
    ///
    /// A decision that builds a derived representation is put through an
    /// experiment instead of being applied outright: built hidden, compared
    /// against the current representation on identical queries, ramped through
    /// canary percentages, and kept only if it earns it. Everything else is
    /// applied the ordinary way.
    ///
    /// The split is not caution, it is capability. Only a change that can exist
    /// *beside* the current representation leaves an old path to compare
    /// against; a change that rewrites the primary has nothing left to be
    /// compared with, so there is no experiment to run and pretending otherwise
    /// would mean measuring the database against itself.
    ///
    /// At most one experiment runs at a time — two would each measure the other
    /// — so at most one shadowable decision starts per cycle and the rest wait.
    /// Call [`Database::advance_experiment`] to give the one in flight the
    /// chance to move on the evidence collected since.
    pub fn optimize_verified(&mut self, guardrails: Guardrails) -> Result<ApplyReport> {
        let inputs = self.opt_inputs()?;
        let mut decisions = self.propose(&inputs);
        let source = self.decision_source();

        // One new experiment per cycle at most. Several may be *running*, but
        // starting more than one at a time would mean proposing changes from
        // one set of inputs and judging them against a database that the other
        // has already altered.
        let held = if self.experiments.running.is_empty() {
            decisions
                .iter()
                .position(|d| self.is_shadowable(d, &inputs))
                .map(|i| decisions.remove(i))
        } else {
            None
        };
        let report = self.run_decisions(decisions, source, inputs)?;
        if let Some(d) = held {
            // Deliberately not folded into `report`: the change has not been
            // applied, it has been *proposed*, and reporting it as applied is
            // exactly the confusion the experiment exists to prevent.
            self.begin_experiment(d, guardrails)?;
        }
        // Both entry points are one optimization cycle and both must forget.
        // Putting this on `optimize` alone meant a database driven through
        // `optimize_verified` — which is to say, one actually using the
        // experiment loop — never forgot anything and never retracted anything.
        self.forget_a_little();
        Ok(report)
    }

    /// Ask the driver the policy names what should change.
    fn propose(&mut self, inputs: &OptInputs) -> Vec<Decision> {
        let ctx = inputs.ctx();
        // Whatever an experiment is proving is off-limits this cycle. It is
        // hidden from the planner, so its usage figures describe the experiment
        // rather than the workload — and a driver that reads them as a verdict
        // retracts the candidate mid-trial, after which the experiment promotes
        // something that is no longer there.
        let under_experiment: Vec<(&'static str, String)> = self
            .experiments
            .running
            .iter()
            .map(|e| {
                (
                    e.experiment.decision.optimization,
                    e.experiment.decision.scope.clone(),
                )
            })
            .collect();
        let input = adabt_opt::DriverInput {
            registry: &self.optimizer.registry,
            current: self.optimizer.controller.config(),
            policy: &inputs.policy,
            telemetry: &inputs.snapshot,
            ctx: &ctx,
            under_experiment: &under_experiment,
            pinned: &self.pinned_scopes(),
        };
        // The adaptive driver keeps its state across cycles — cooldowns, what it
        // last changed — so it lives on the database rather than being
        // constructed per call.
        match self.policy.mode {
            Mode::Adaptive => self.optimizer.adaptive.decide(input),
            Mode::Manual { .. } => ManualDriver.decide(input),
        }
    }

    /// Structures the optimizer may not retract because correctness rests on
    /// them rather than on their speed.
    ///
    /// Every unique constraint's backing index, in the same `name.field` scope
    /// format `auto_index` itself uses — the adaptive driver compares these as
    /// plain strings, so the format has to match exactly or the pin is silently
    /// a no-op.
    fn pinned_scopes(&self) -> Vec<(&'static str, String)> {
        self.unique_constraints
            .iter()
            .map(|(c, f)| ("auto_index", format!("{c}.{f}")))
            .collect()
    }

    fn decision_source(&self) -> DecisionSource {
        match self.policy.mode {
            Mode::Adaptive => DecisionSource::Adaptive,
            Mode::Manual { .. } => DecisionSource::Manual,
        }
    }

    /// Whether this decision leaves an old path to compare a new one against.
    fn is_shadowable(&self, decision: &Decision, inputs: &OptInputs) -> bool {
        decision.action == DecisionAction::Enable
            && self
                .optimizer
                .registry
                .get(decision.optimization)
                .is_some_and(|o| {
                    let plan = o.plan_enable(&inputs.ctx(), &decision.scope, &decision.params);
                    !plan.apply.is_empty() && plan.apply.iter().all(|a| a.is_shadowable())
                })
    }

    /// Gather everything the optimizer needs to describe this database.
    /// Fold the compiled paths' traffic into telemetry. Those calls skip the
    /// per-call events the general path records, so without this the
    /// adaptive driver would see a shape's traffic vanish the moment it got
    /// fast, and stop reasoning about it.
    fn report_compiled_traffic(&mut self) {
        for (shape, collection, tally) in self.caches.compiled.drain_tallies() {
            self.telemetry.probe.record(Event::Ops {
                collection: &collection,
                // The kind the general path records for every query, so a
                // shape looks the same whichever path answered it.
                kind: adabt_telemetry::OpKind::Scan,
                shape: adabt_telemetry::QueryShape(shape.0),
                count: tally.calls,
                nanos_each: tally.mean_nanos(),
                rows: tally.rows,
            });
        }
    }

    pub(super) fn opt_inputs(&mut self) -> Result<OptInputs> {
        // What the result cache holds, or would hold if it were on: the
        // memory half of judging it, measured rather than read off capacity.
        self.telemetry.probe.record(Event::CacheResident {
            name: "result_cache",
            bytes: self.caches.results.resident_bytes() as u64,
        });
        self.report_compiled_traffic();
        let snapshot = self.telemetry.probe.snapshot();
        let collections = self.collection_sizes()?;
        let filtered = snapshot.most_filtered_fields();
        // Sampled, not summed over every record: this runs every cycle, and
        // inside the windows the optimizer measures.
        let footprint = self.store.record_footprint()?;
        let stored = self.store.sampled_stored_bytes()?;
        Ok(OptInputs {
            fixed: self.fixed_size_collections(),
            existing: self.existing_index_tuples(),
            max_ids: self.max_ids()?,
            current_bytes: stored + self.derived_memory_bytes() as u64,
            record_bytes: record_bytes(footprint),
            policy: self.policy.clone(),
            snapshot,
            collections,
            filtered,
        })
    }

    /// Put decisions through the controller.
    ///
    /// Shared with the experiment runner deliberately. A change built for a
    /// trial is gated exactly as one applied outright — same guarantee filter,
    /// same constraints, same log — because an experiment that skipped the
    /// gates would be a way around them, and "nothing bypasses the controller"
    /// would become "nothing except the interesting case".
    pub(super) fn run_decisions(
        &mut self,
        decisions: Vec<Decision>,
        source: DecisionSource,
        inputs: OptInputs,
    ) -> Result<ApplyReport> {
        if decisions.is_empty() {
            return Ok(ApplyReport::default());
        }
        // The controller needs `&mut self` as a sink while borrowing registry
        // and policy, so both are moved aside for the call.
        let registry = std::mem::take(&mut self.optimizer.registry);
        let mut controller = std::mem::replace(
            &mut self.optimizer.controller,
            OptimizationController::new(),
        );
        let ctx = inputs.ctx();
        let report = controller.apply(
            decisions,
            adabt_opt::controller::ApplyEnv {
                registry: &registry,
                policy: &inputs.policy,
                ctx: &ctx,
            },
            self,
            source,
        );
        self.optimizer.registry = registry;
        self.optimizer.controller = controller;
        report
    }

    fn collection_sizes(&mut self) -> Result<Vec<(String, usize)>> {
        let names = self.store.collection_names();
        let mut out = Vec::with_capacity(names.len());
        for n in names {
            let c = self.store.count(&n)?;
            out.push((n, c));
        }
        Ok(out)
    }

    /// Highest record id per collection, for density estimates.
    fn max_ids(&mut self) -> Result<Vec<(String, u64)>> {
        let mut out = Vec::new();
        for n in self.store.collection_names() {
            let max = self.store.scan(&n)?.last().map(|(id, _)| id.0).unwrap_or(0);
            out.push((n, max));
        }
        Ok(out)
    }

    pub(super) fn fixed_size_collections(&self) -> Vec<String> {
        self.store
            .collection_names()
            .into_iter()
            .filter(|n| {
                self.store
                    .schema_of(n)
                    .map(|s| s.mode() == SchemaMode::Fixed)
                    .unwrap_or(false)
            })
            .collect()
    }

    fn existing_index_tuples(&self) -> Vec<(String, String, IndexKind)> {
        self.index_specs()
            .into_iter()
            .map(|s| (s.collection, s.field, s.kind))
            .collect()
    }
}

/// What each collection's records take uncompressed, and the ratio
/// compression was measured to store them at.
///
/// Both are read the same way whether compression is on or off, which is
/// what keeps its estimate from claiming less once it has been made.
fn record_bytes(footprint: Vec<(String, RecordFootprint)>) -> Vec<(String, RecordBytes)> {
    footprint
        .into_iter()
        .map(|(name, fp)| {
            (
                name,
                RecordBytes {
                    raw: fp.raw_bytes,
                    compression_ratio: fp.compression_ratio,
                },
            )
        })
        .collect()
}
