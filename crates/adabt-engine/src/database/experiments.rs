//! Live experiments: starting, advancing, promoting and reverting them,
//! building candidates, routing queries through a running experiment, and the
//! shadow-pair execution both halves of a comparison go through.

use adabt_core::error::{Error, Result};
use adabt_core::ids::RecordId;
use adabt_core::record::Record;
use adabt_core::store::LogicalStore;
use adabt_ir::plan::LogicalPlan;
use adabt_opt::decision::{Decision, DecisionAction, Source as DecisionSource, Verdict};
use adabt_opt::experiment::{Guardrails, Phase};
use std::time::Instant;

use crate::experiment::LiveExperiment;

use super::{Database, QueryMode};

impl Database {
    // -- experiments -------------------------------------------------------

    /// Find a running experiment by id.
    pub(super) fn experiment_index(&self, id: u64) -> Option<usize> {
        self.experiments.iter().position(|e| e.experiment.id == id)
    }

    /// Begin proving a change against live traffic.
    ///
    /// Nothing is built yet: the experiment starts in `Proposed` and each call
    /// to [`Database::advance_experiments`] moves it one step, so the caller
    /// controls the pace at which evidence is demanded.
    ///
    /// Several may run at once, as long as their scopes do not overlap.
    pub fn begin_experiment(&mut self, decision: Decision, guardrails: Guardrails) -> Result<u64> {
        if decision.action != DecisionAction::Enable {
            return Err(Error::InvalidOptimization(format!(
                "only an enable can be experimented on, not a {}",
                decision.action.as_str()
            )));
        }
        let inputs = self.opt_inputs()?;
        let plan = {
            let opt = self.registry.get(decision.optimization).ok_or_else(|| {
                Error::InvalidOptimization(format!("{} is not registered", decision.optimization))
            })?;
            opt.plan_enable(&inputs.ctx(), &decision.scope, &decision.params)
        };
        if plan.apply.is_empty() {
            return Err(Error::InvalidOptimization(format!(
                "{} has nothing to do for {}",
                decision.optimization, decision.scope
            )));
        }
        if let Some(bad) = plan.apply.iter().find(|a| !a.is_shadowable()) {
            return Err(Error::InvalidOptimization(format!(
                "{} cannot be experimented on: {} leaves no old path to compare against",
                decision.optimization,
                bad.describe()
            )));
        }

        // Which collection's queries count as evidence. A scope of `global`, or
        // one naming something that is not a collection, means every query does.
        let candidate_collection = decision
            .scope
            .split('.')
            .next()
            .filter(|c| self.store.collection_names().iter().any(|n| n == c))
            .unwrap_or("")
            .to_string();

        if let Some(clash) = self
            .experiments
            .iter()
            .find(|e| crate::experiment::scopes_overlap(&e.collection, &candidate_collection))
        {
            let where_ = if candidate_collection.is_empty() {
                "globally".to_string()
            } else {
                format!("on {candidate_collection}")
            };
            return Err(Error::InvalidOptimization(format!(
                "experiment #{} already covers the same traffic ({}), so running \
                 one {where_} too would make each measure the other",
                clash.experiment.id,
                if clash.collection.is_empty() {
                    "every collection".to_string()
                } else {
                    clash.collection.clone()
                }
            )));
        }

        let id = self.next_experiment_id;
        self.next_experiment_id += 1;
        self.experiments.push(LiveExperiment::new(
            id,
            decision,
            candidate_collection,
            guardrails,
        ));
        Ok(id)
    }

    /// The oldest running experiment, if any.
    ///
    /// Kept for the common case of driving one at a time; see
    /// [`Database::experiments`] when several may be running.
    pub fn experiment(&self) -> Option<&LiveExperiment> {
        self.experiments.first()
    }

    /// Every experiment running right now, oldest first.
    pub fn experiments(&self) -> impl Iterator<Item = &LiveExperiment> {
        self.experiments.iter()
    }

    /// Changes the controller accepted, over the database's whole life.
    pub fn decision_count(&self) -> usize {
        self.controller
            .log()
            .records()
            .iter()
            .filter(|r| r.verdict.succeeded())
            .count()
    }

    /// Experiments that have ever been started.
    ///
    /// Counted from the id allocator rather than from a list, so it includes
    /// the ones already retired and does not grow a structure to answer.
    pub fn experiments_started(&self) -> usize {
        (self.next_experiment_id - 1) as usize
    }

    /// Experiments that ended by being promoted.
    pub fn promoted_count(&self) -> usize {
        self.finished
            .iter()
            .filter(|e| e.phase() == Phase::Promoted)
            .count()
    }

    /// Experiments that ended by being reverted.
    pub fn reverted_count(&self) -> usize {
        self.finished
            .iter()
            .filter(|e| e.phase() == Phase::Reverted)
            .count()
    }

    /// Experiments that reached a verdict, oldest first.
    pub fn finished_experiments(&self) -> &[LiveExperiment] {
        &self.finished
    }

    pub fn explain_experiment(&self) -> String {
        if self.experiments.is_empty() {
            return match self.finished.last() {
                Some(e) => e.explain(),
                None => "no experiments\n".to_string(),
            };
        }
        self.experiments
            .iter()
            .map(|e| e.explain())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Fold in the evidence so far and let every experiment's state machine
    /// act on it.
    ///
    /// Returns each experiment's id and the phase now in effect. Each call is
    /// one opportunity to move, and a machine holds where it is when the
    /// evidence is not yet enough — so calling this on a timer is safe and
    /// calling it too often merely produces `Inconclusive` until the samples
    /// arrive.
    pub fn advance_experiments(&mut self) -> Result<Vec<(u64, Phase)>> {
        let ids: Vec<u64> = self.experiments.iter().map(|e| e.experiment.id).collect();
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            // Re-found each time: retiring one experiment removes it from the
            // vector, so an index taken before the loop would drift.
            let Some(i) = self.experiment_index(id) else {
                continue;
            };
            let ram = self.derived_memory_bytes() as u64;
            let live = &mut self.experiments[i];
            live.fold(ram);
            let before = live.phase();
            let after = live.experiment.advance();
            if after != before {
                self.enter_phase(id, after)?;
            }
            out.push((id, after));
        }
        Ok(out)
    }

    /// Advance the oldest running experiment, for callers driving one.
    pub fn advance_experiment(&mut self) -> Result<Option<Phase>> {
        Ok(self.advance_experiments()?.first().map(|(_, p)| *p))
    }

    /// Stop every running experiment and undo whatever each built.
    pub fn abort_experiment(&mut self, why: &str) -> Result<()> {
        let ids: Vec<u64> = self.experiments.iter().map(|e| e.experiment.id).collect();
        for id in ids {
            self.abort_experiment_by_id(id, why)?;
        }
        Ok(())
    }

    /// Stop one experiment and undo whatever it built.
    pub fn abort_experiment_by_id(&mut self, id: u64, why: &str) -> Result<()> {
        if let Some(i) = self.experiment_index(id) {
            self.experiments[i].experiment.abort(why);
        }
        self.retire_experiment(id, why.to_string(), false)
    }

    fn enter_phase(&mut self, id: u64, phase: Phase) -> Result<()> {
        if phase.is_measuring() {
            if let Some(i) = self.experiment_index(id) {
                self.experiments[i].reset_measurements();
            }
        }
        match phase {
            Phase::Building => self.build_candidate(id),
            Phase::Promoted => self.retire_experiment(id, "promoted".into(), true),
            Phase::Reverted => {
                // The reason is read from the experiment, which recorded it when
                // it aborted. Re-assessing here would ask a reverted experiment
                // how it is doing and be told, accurately and uselessly, that it
                // is serving no traffic.
                let why = self
                    .experiment_index(id)
                    .map(|i| self.experiments[i].experiment.outcome())
                    .unwrap_or_else(|| "reverted".into());
                self.retire_experiment(id, why, false)
            }
            _ => Ok(()),
        }
    }

    /// Build one experiment's candidate where the planner cannot see it.
    fn build_candidate(&mut self, id: u64) -> Result<()> {
        let Some(i) = self.experiment_index(id) else {
            return Ok(());
        };
        let decision = self.experiments[i].experiment.decision.clone();
        let ram_before = self.derived_memory_bytes() as u64;
        self.experiments[i].set_ram_before(ram_before);
        // Mask first, build second. The other order would leave a window in
        // which the structure exists and is usable, and one query through it is
        // one query the experiment was supposed to be protecting.
        self.candidate_visible = false;
        self.recording_candidate = Some(id);
        let inputs = self.opt_inputs()?;
        let mut trial = decision.clone();
        trial.trigger = format!("trialled by experiment #{id}: {}", decision.trigger);
        let report = self.run_decisions(vec![trial], DecisionSource::Adaptive, inputs);
        self.recording_candidate = None;
        let report = report?;

        // Asked of *this* experiment, not of the mask as a whole: with another
        // experiment running, a globally non-empty mask says nothing about
        // whether this one built anything worth comparing.
        if !report.all_applied() || self.hidden.is_empty_for(id) {
            // Either the controller refused, or it applied something with
            // nothing to mask. Both mean there is no comparison to run, and
            // measuring one anyway would compare the database against itself
            // and promote on the noise.
            let why = if report.all_applied() {
                "the change built nothing that can be held beside the current representation"
                    .to_string()
            } else {
                let refused: Vec<String> = report
                    .rejected
                    .iter()
                    .map(|(l, v)| format!("{l}: {}", v.as_str()))
                    .collect();
                format!("the controller refused it ({})", refused.join("; "))
            };
            self.abort_experiment_by_id(id, &why)?;
        }
        Ok(())
    }

    /// End one experiment, keeping or undoing what it built.
    fn retire_experiment(&mut self, id: u64, why: String, promoted: bool) -> Result<()> {
        let Some(i) = self.experiment_index(id) else {
            return Ok(());
        };
        let mut live = self.experiments.remove(i);
        live.candidates = self.hidden.clone();
        let decision = live.experiment.decision.clone();

        if !promoted && !self.hidden.is_empty_for(id) {
            // Undo through the controller, which knows the exact inverse of what
            // it applied. Reverting is instant because every candidate is a
            // derived representation: dropping one costs a deallocation and
            // loses nothing that cannot be rebuilt from the primary.
            let mut undo = decision.clone();
            undo.action = DecisionAction::Disable;
            undo.trigger = format!("experiment #{id} reverted: {why}");
            let inputs = self.opt_inputs()?;
            self.run_decisions(vec![undo], DecisionSource::Adaptive, inputs)?;
        }
        // Unmasking is what promotion *is*. Nothing is rebuilt and nothing
        // moves: the structure was real the whole time, and all that changes
        // is that the planner is now allowed to know about it.
        //
        // Only *this* experiment's entries are dropped. Clearing the whole
        // mask — which is what this did — would unmask any other running
        // experiment's unproven structure the instant an unrelated
        // experiment finished, exposing it to live traffic it was
        // specifically being kept away from.
        self.hidden.forget(id);
        self.candidate_visible = false;
        self.experiment_under_test = None;

        let detail = if promoted {
            format!(
                "experiment #{id} promoted — {}, {} canary queries on the candidate",
                live.shadow.describe(),
                live.experiment.candidate.samples
            )
        } else {
            format!("experiment #{id} reverted — {why}")
        };
        let verdict = if promoted {
            Verdict::Applied
        } else {
            Verdict::Failed
        };
        self.controller
            .note(decision, verdict, detail, DecisionSource::Adaptive);
        self.finished.push(live);
        Ok(())
    }

    /// The experiment, if any, that this query is evidence for.
    ///
    /// At most one: `begin_experiment` refuses overlapping scopes, so a query
    /// on a collection can match a collection-scoped experiment or a global
    /// one but never both.
    fn experiment_for(&self, collection: &str) -> Option<u64> {
        self.experiments
            .iter()
            .find(|e| {
                e.phase().is_measuring() && (e.collection.is_empty() || e.collection == collection)
            })
            .map(|e| e.experiment.id)
    }

    /// Route one query according to the phase its experiment is in.
    pub(super) fn experiment_query(
        &mut self,
        logical: &LogicalPlan,
    ) -> Result<Vec<(RecordId, Record)>> {
        let Some(id) = self.experiment_for(logical.collection()) else {
            return self.query_in(logical, QueryMode::Normal);
        };
        let Some(i) = self.experiment_index(id) else {
            return self.query_in(logical, QueryMode::Normal);
        };
        // Taken out for the duration so the query path can borrow `self`
        // mutably, and put back before returning on every path.
        let mut live = self.experiments.remove(i);
        // Names whose candidate may be revealed while this query runs. Without
        // it, `candidate_visible` would expose every running experiment's
        // structures at once and each would be measuring the others.
        self.experiment_under_test = Some(id);

        let out = match live.phase() {
            // Both paths, same query, same state. The caller receives the
            // baseline: the candidate is being judged, not trusted.
            Phase::Shadow => {
                let r = crate::shadow::trial(self, logical, &mut live.shadow);
                if let Some(t) = live.shadow.last_trial() {
                    live.record_baseline(t.baseline_nanos, r.is_ok());
                    live.record_candidate(t.candidate_nanos, r.is_ok());
                }
                r
            }
            // One path, and its answer is returned. Correctness was settled in
            // shadow; what a canary measures is latency under the cache state a
            // real workload actually produces.
            Phase::Canary(percent) => {
                let use_candidate = live.route(percent);
                self.candidate_visible = use_candidate;
                let started = Instant::now();
                let r = self.query_in(logical, QueryMode::Served);
                let nanos = started.elapsed().as_nanos() as u64;
                self.candidate_visible = false;
                if use_candidate {
                    live.record_candidate(nanos, r.is_ok());
                } else {
                    live.record_baseline(nanos, r.is_ok());
                }
                r
            }
            _ => self.query_in(logical, QueryMode::Normal),
        };
        self.experiment_under_test = None;
        // Back where it was, so age order — and therefore `experiment()` —
        // stays stable across queries.
        self.experiments.insert(i.min(self.experiments.len()), live);
        out
    }
}

/// The two ways this database can answer a query while an experiment is running.
///
/// Both go through the ordinary query path. The only difference between them is
/// one boolean, which is the point: if the candidate needed a separate execution
/// path to be measured, the measurement would be of that path rather than of the
/// change.
impl crate::shadow::ShadowPair for Database {
    fn baseline(&mut self, plan: &LogicalPlan) -> Result<Vec<(RecordId, Record)>> {
        self.candidate_visible = false;
        self.query_in(plan, QueryMode::Trial)
    }

    fn candidate(&mut self, plan: &LogicalPlan) -> Result<Vec<(RecordId, Record)>> {
        self.candidate_visible = true;
        let out = self.query_in(plan, QueryMode::Trial);
        self.candidate_visible = false;
        out
    }
}
