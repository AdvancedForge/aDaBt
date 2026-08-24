//! The decision log.
//!
//! Every change to physical strategy is recorded here, **including changes a
//! human asked for**. "The user set level 5" is as much a decision worth
//! explaining later as anything the adaptive driver will do, and a log that only
//! covers automatic changes cannot answer "why is this index here".
//!
//! Records are structured. The human-readable explanation is *generated from*
//! the record, never stored as prose, so explanations stay queryable and cannot
//! drift away from the decision they describe.

use crate::action::ChangePlan;
use crate::cost::CostEstimate;
use std::collections::HashSet;

/// A request to change one optimization's state.
///
/// `params` carries the *intent* — what tuning was asked for — not the
/// mechanism used to achieve it. The controller stores these in the
/// configuration, so that comparing a current configuration against a target
/// compares like with like.
///
/// Deriving the stored params from the actions instead puts intent and
/// mechanism in two different vocabularies, and the difference between them
/// never converges: the driver re-proposes the same retune on every cycle,
/// forever. In manual mode that is a direct violation of the promise that the
/// engine does not change strategy on its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub optimization: &'static str,
    pub scope: String,
    pub action: DecisionAction,
    pub params: crate::config::Params,
    /// Why the driver asked for this.
    pub trigger: String,
}

impl Decision {
    pub fn new(
        optimization: &'static str,
        scope: impl Into<String>,
        action: DecisionAction,
        trigger: impl Into<String>,
    ) -> Self {
        Self {
            optimization,
            scope: scope.into(),
            action,
            params: crate::config::Params::default(),
            trigger: trigger.into(),
        }
    }

    pub fn with_params(mut self, params: crate::config::Params) -> Self {
        self.params = params;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecisionAction {
    Enable,
    Disable,
    Retune,
}

impl DecisionAction {
    pub fn as_str(&self) -> &'static str {
        match self {
            DecisionAction::Enable => "enable",
            DecisionAction::Disable => "disable",
            DecisionAction::Retune => "retune",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Applied,
    /// The policy's guarantees forbid it. Not a cost decision.
    ForbiddenByGuarantees,
    /// A hard resource ceiling would be exceeded.
    ExceedsConstraints,
    /// Conditions are not met.
    NotApplicable,
    /// Another enabled optimization conflicts.
    Conflicts,
    /// A prerequisite is not enabled.
    MissingPrerequisite,
    /// Tried before under a similar workload and rejected.
    PreviouslyRejected,
    Failed,
}

impl Verdict {
    pub fn as_str(&self) -> &'static str {
        match self {
            Verdict::Applied => "applied",
            Verdict::ForbiddenByGuarantees => "forbidden by guarantees",
            Verdict::ExceedsConstraints => "exceeds constraints",
            Verdict::NotApplicable => "not applicable",
            Verdict::Conflicts => "conflicts with an enabled optimization",
            Verdict::MissingPrerequisite => "missing prerequisite",
            Verdict::PreviouslyRejected => "previously rejected",
            Verdict::Failed => "failed",
        }
    }
    pub fn succeeded(&self) -> bool {
        *self == Verdict::Applied
    }
}

#[derive(Debug, Clone)]
pub struct DecisionRecord {
    pub sequence: u64,
    pub decision: Decision,
    pub verdict: Verdict,
    /// Populated when the verdict is not `Applied`.
    pub detail: Option<String>,
    pub estimate: Option<CostEstimate>,
    pub plan: ChangePlan,
    /// Whether a human or the optimizer asked. Both go through this log.
    pub source: Source,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Manual,
    Adaptive,
}

impl Source {
    pub fn as_str(&self) -> &'static str {
        match self {
            Source::Manual => "manual",
            Source::Adaptive => "adaptive",
        }
    }
}

impl DecisionRecord {
    /// Render the record as prose.
    ///
    /// Generated on demand rather than stored, so the explanation can never
    /// disagree with the structured facts it describes.
    pub fn explain(&self) -> String {
        let mut s = String::new();
        s.push_str(&format!(
            "#{} {} {} [{}] — {}\n",
            self.sequence,
            self.decision.action.as_str(),
            self.decision.optimization,
            self.decision.scope,
            self.verdict.as_str()
        ));
        s.push_str(&format!("  requested by: {}\n", self.source.as_str()));
        s.push_str(&format!("  because: {}\n", self.decision.trigger));
        if let Some(d) = &self.detail {
            s.push_str(&format!("  detail: {d}\n"));
        }
        if let Some(e) = &self.estimate {
            if e.p50_delta.0 != 1.0 {
                s.push_str(&format!(
                    "  estimated p50: {:+.1}%\n",
                    e.p50_delta.percent_change()
                ));
            }
            if e.p99_delta.0 != 1.0 {
                s.push_str(&format!(
                    "  estimated p99: {:+.1}%\n",
                    e.p99_delta.percent_change()
                ));
            }
            if e.ram_bytes != 0 {
                s.push_str(&format!(
                    "  estimated RAM: {:+.1} MB\n",
                    e.ram_bytes as f64 / 1e6
                ));
            }
            if e.maintain_cost > 0.0 {
                s.push_str(&format!(
                    "  write overhead: {:.1}%\n",
                    e.maintain_cost * 100.0
                ));
            }
            s.push_str(&format!("  confidence: {:.0}%\n", e.confidence * 100.0));
        }
        if !self.plan.is_empty() {
            s.push_str(&format!("  actions: {}\n", self.plan.describe()));
        }
        s
    }
}

/// Append-only history plus a memory of what did not work.
#[derive(Default)]
pub struct DecisionLog {
    records: Vec<DecisionRecord>,
    /// `(optimization, scope)` pairs already rejected. Prevents the optimizer
    /// from proposing the same losing idea on every cycle.
    rejected: HashSet<(&'static str, String)>,
    next_sequence: u64,
}

impl DecisionLog {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(
        &mut self,
        decision: Decision,
        verdict: Verdict,
        detail: Option<String>,
        estimate: Option<CostEstimate>,
        plan: ChangePlan,
        source: Source,
    ) -> u64 {
        let sequence = self.next_sequence;
        self.next_sequence += 1;
        if !verdict.succeeded() && verdict != Verdict::PreviouslyRejected {
            self.rejected
                .insert((decision.optimization, decision.scope.clone()));
        }
        // A later success clears the memory: conditions change, and a candidate
        // that becomes viable must not be blocked by its own past.
        if verdict.succeeded() {
            self.rejected
                .remove(&(decision.optimization, decision.scope.clone()));
        }
        self.records.push(DecisionRecord {
            sequence,
            decision,
            verdict,
            detail,
            estimate,
            plan,
            source,
        });
        sequence
    }

    pub fn was_rejected(&self, optimization: &'static str, scope: &str) -> bool {
        self.rejected.contains(&(optimization, scope.to_string()))
    }

    pub fn records(&self) -> &[DecisionRecord] {
        &self.records
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Every record touching one optimization, newest last.
    pub fn history_of(&self, optimization: &str) -> Vec<&DecisionRecord> {
        self.records
            .iter()
            .filter(|r| r.decision.optimization == optimization)
            .collect()
    }

    /// Why the database is the way it is, in full.
    pub fn explain_all(&self) -> String {
        if self.records.is_empty() {
            return "no decisions recorded\n".to_string();
        }
        self.records.iter().map(|r| r.explain()).collect()
    }

    /// Why one optimization is or is not in place.
    pub fn explain(&self, optimization: &str) -> String {
        let h = self.history_of(optimization);
        if h.is_empty() {
            return format!("no decisions recorded for {optimization}\n");
        }
        h.iter().map(|r| r.explain()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::Action;
    use adabt_core::index_kind::IndexKind;

    fn decision(name: &'static str, scope: &str) -> Decision {
        Decision {
            optimization: name,
            scope: scope.to_string(),
            action: DecisionAction::Enable,
            params: Default::default(),
            trigger: "4.2M queries/hour filtered on this field".into(),
        }
    }

    fn plan() -> ChangePlan {
        ChangePlan::new(
            vec![Action::CreateIndex {
                collection: "users".into(),
                field: "country".into(),
                kind: IndexKind::Hash,
            }],
            vec![Action::DropIndex {
                collection: "users".into(),
                field: "country".into(),
                kind: IndexKind::Hash,
            }],
        )
    }

    #[test]
    fn manual_decisions_are_logged_too() {
        let mut log = DecisionLog::new();
        log.record(
            decision("auto_index", "users.country"),
            Verdict::Applied,
            None,
            None,
            plan(),
            Source::Manual,
        );
        let text = log.explain_all();
        assert!(text.contains("requested by: manual"), "{text}");
    }

    #[test]
    fn an_explanation_is_generated_from_the_record() {
        let mut log = DecisionLog::new();
        let est = CostEstimate::faster(0.36, 0.36)
            .with_ram(3_200_000_000)
            .with_maintenance(0.02)
            .with_confidence(0.7);
        log.record(
            decision("auto_index", "users.country"),
            Verdict::Applied,
            None,
            Some(est),
            plan(),
            Source::Adaptive,
        );
        let e = log.explain("auto_index");
        assert!(e.contains("enable auto_index [users.country]"), "{e}");
        assert!(e.contains("applied"), "{e}");
        assert!(e.contains("4.2M queries/hour"), "{e}");
        assert!(e.contains("-64.0%"), "{e}");
        assert!(e.contains("3200.0 MB"), "{e}");
        assert!(e.contains("confidence: 70%"), "{e}");
        assert!(e.contains("create hash index on users.country"), "{e}");
    }

    #[test]
    fn a_rejection_records_why_and_is_remembered() {
        let mut log = DecisionLog::new();
        log.record(
            decision("async_durability", "global"),
            Verdict::ForbiddenByGuarantees,
            Some("policy requires durability: strict".into()),
            None,
            ChangePlan::default(),
            Source::Adaptive,
        );
        assert!(log.was_rejected("async_durability", "global"));
        let e = log.explain("async_durability");
        assert!(e.contains("forbidden by guarantees"), "{e}");
        assert!(e.contains("policy requires durability: strict"), "{e}");
    }

    #[test]
    fn rejection_memory_is_scoped_not_global() {
        let mut log = DecisionLog::new();
        log.record(
            decision("auto_index", "users.country"),
            Verdict::NotApplicable,
            None,
            None,
            ChangePlan::default(),
            Source::Adaptive,
        );
        assert!(log.was_rejected("auto_index", "users.country"));
        assert!(
            !log.was_rejected("auto_index", "orders.status"),
            "one bad scope must not blacklist another"
        );
    }

    #[test]
    fn a_later_success_clears_the_rejection_memory() {
        // Conditions change. A candidate that becomes viable must not be
        // blocked forever by having once been wrong.
        let mut log = DecisionLog::new();
        log.record(
            decision("auto_index", "users.country"),
            Verdict::NotApplicable,
            None,
            None,
            ChangePlan::default(),
            Source::Adaptive,
        );
        assert!(log.was_rejected("auto_index", "users.country"));
        log.record(
            decision("auto_index", "users.country"),
            Verdict::Applied,
            None,
            None,
            plan(),
            Source::Adaptive,
        );
        assert!(!log.was_rejected("auto_index", "users.country"));
    }

    #[test]
    fn sequence_numbers_increase_and_history_is_ordered() {
        let mut log = DecisionLog::new();
        for _ in 0..5 {
            log.record(
                decision("x", "global"),
                Verdict::Applied,
                None,
                None,
                ChangePlan::default(),
                Source::Manual,
            );
        }
        let h = log.history_of("x");
        assert_eq!(h.len(), 5);
        assert!(h.windows(2).all(|w| w[0].sequence < w[1].sequence));
    }

    #[test]
    fn an_empty_log_explains_itself() {
        let log = DecisionLog::new();
        assert!(log.is_empty());
        assert!(log.explain_all().contains("no decisions"));
        assert!(log
            .explain("anything")
            .contains("no decisions recorded for anything"));
    }
}
