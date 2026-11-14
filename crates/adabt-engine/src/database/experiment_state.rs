//! The bookkeeping behind live experiments: which are running, which have
//! finished, and which unproven structures the current query may see.

use adabt_opt::action::Action;

use crate::experiment::{Candidates, LiveExperiment};

/// Running and finished experiments, and the candidate mask that keeps their
/// unproven structures away from traffic they are not being measured on.
pub(super) struct Experiments {
    /// Experiments running right now, on scopes that do not overlap.
    ///
    /// A `Vec` rather than an `Option` because there is no reason two changes
    /// to *different* collections should have to be proved one after the
    /// other. The reason it was an `Option` was safety, not simplicity: with a
    /// single global candidate mask and a single global "candidates visible"
    /// flag, a second experiment would have had its unproven structures
    /// exposed to the first experiment's canary traffic, and each would have
    /// been measuring the other. Both of those are now per-experiment, so the
    /// remaining requirement is only that their scopes are disjoint.
    pub(super) running: Vec<LiveExperiment>,
    /// Experiments that reached a verdict, kept so the reasoning survives them.
    pub(super) finished: Vec<LiveExperiment>,
    /// The experiment whose query is being run right now, if any.
    ///
    /// Paired with `candidate_visible`: together they name *which* candidate
    /// is allowed to be seen, rather than merely that some candidate is.
    pub(super) under_test: Option<u64>,
    /// Structures built for an experiment and not yet trusted.
    ///
    /// Held here rather than on the experiment because the mask has to outlive
    /// a borrow of it: a shadow trial moves the experiment aside to record into
    /// it, and if the mask went with it both halves of the pair would take the
    /// candidate path and the comparison would be of a thing against itself.
    pub(super) hidden: Candidates,
    /// Whether *this* query may use the hidden structures.
    pub(super) candidate_visible: bool,
    /// The experiment whose candidate is being built right now, if any.
    ///
    /// Carries the id rather than a bare flag because what gets recorded is
    /// *whose* candidate a structure is. With one experiment the id could be
    /// looked up; with several running, "the experiment" is not a thing, and
    /// attributing a structure to the wrong one would mask it for the wrong
    /// trial.
    pub(super) recording_candidate: Option<u64>,
    next_id: u64,
}

impl Experiments {
    pub(super) fn new() -> Self {
        Self {
            running: Vec::new(),
            finished: Vec::new(),
            under_test: None,
            hidden: Candidates::default(),
            candidate_visible: false,
            recording_candidate: None,
            next_id: 1,
        }
    }

    /// Hand out the id the next experiment will run under.
    pub(super) fn allocate_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Experiments that have ever been started.
    ///
    /// Counted from the id allocator rather than from a list, so it includes
    /// the ones already retired and does not grow a structure to answer.
    pub(super) fn started(&self) -> usize {
        (self.next_id - 1) as usize
    }

    /// Find a running experiment by id.
    pub(super) fn index_of(&self, id: u64) -> Option<usize> {
        self.running.iter().position(|e| e.experiment.id == id)
    }

    /// The experiment, if any, that a query on `collection` is evidence for.
    ///
    /// At most one: `begin_experiment` refuses overlapping scopes, so a query
    /// on a collection can match a collection-scoped experiment or a global
    /// one but never both.
    pub(super) fn measuring(&self, collection: &str) -> Option<u64> {
        self.running
            .iter()
            .find(|e| {
                e.phase().is_measuring() && (e.collection.is_empty() || e.collection == collection)
            })
            .map(|e| e.experiment.id)
    }

    /// Attribute a structure the controller just built to the experiment whose
    /// candidate is being built, if any.
    pub(super) fn record_candidate_action(&mut self, action: &Action) {
        if let Some(id) = self.recording_candidate {
            let for_collection = self
                .index_of(id)
                .map(|i| self.running[i].collection.clone())
                .unwrap_or_default();
            self.hidden.record(id, action, &for_collection);
        }
    }

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
    /// `under_test` says whose. Neither alone is enough: the first on its own
    /// revealed every running experiment's structures at once.
    pub(super) fn revealed(&self) -> Option<u64> {
        if self.candidate_visible {
            self.under_test
        } else {
            None
        }
    }

    /// Whether `collection`'s direct array is a candidate this query may not use.
    pub(super) fn hides_direct(&self, collection: &str) -> bool {
        self.masked()
            .is_some_and(|h| h.hides_direct(self.revealed(), collection))
    }
}
