//! The `Optimization` trait and its metadata.

use adabt_core::policy::{GuaranteeRequirements, Policy};
use adabt_telemetry::Snapshot;

use crate::action::ChangePlan;
use crate::config::Params;
use crate::cost::{AxisEffects, CostEstimate};

/// How hard a change is to take back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Reversibility {
    /// Undone by flipping a flag.
    Instant,
    /// Undone, but the structure must be rebuilt to get back.
    RebuildRequired,
    /// Cannot be undone. Never applied automatically.
    Destructive,
}

/// What a change applies to.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum OptScope {
    Global,
    Collection(String),
    Field { collection: String, field: String },
    QueryShape(u64),
}

impl OptScope {
    pub fn describe(&self) -> String {
        match self {
            OptScope::Global => "global".into(),
            OptScope::Collection(c) => c.clone(),
            OptScope::Field { collection, field } => format!("{collection}.{field}"),
            OptScope::QueryShape(s) => format!("shape:{s:016x}"),
        }
    }
}

/// Whether an optimization may be used, and if not, why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Applicability {
    Applicable,
    /// Conditions are not met yet, but might be later.
    NotYet(String),
    /// Structurally impossible here, and will not become possible.
    Ineligible(String),
}

impl Applicability {
    pub fn is_applicable(&self) -> bool {
        matches!(self, Applicability::Applicable)
    }
    pub fn reason(&self) -> Option<&str> {
        match self {
            Applicability::Applicable => None,
            Applicability::NotYet(r) | Applicability::Ineligible(r) => Some(r),
        }
    }
}

#[derive(Debug, Clone)]
pub struct OptMeta {
    pub name: &'static str,
    pub summary: &'static str,
    pub scope_kind: ScopeKind,
    /// Lowest optimization level at which this is on by default.
    pub min_level: u8,
    pub axis_effects: AxisEffects,
    /// What the policy must permit. Checked as a *filter* before any scoring.
    pub requires_guarantees: GuaranteeRequirements,
    pub prerequisites: &'static [&'static str],
    pub conflicts_with: &'static [&'static str],
    pub reversibility: Reversibility,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeKind {
    Global,
    PerCollection,
    PerField,
    PerQueryShape,
}

/// What an optimization is allowed to look at when deciding.
pub struct OptContext<'a> {
    pub policy: &'a Policy,
    pub telemetry: &'a Snapshot,
    /// Collections and their approximate row counts.
    pub collections: &'a [(String, usize)],
    /// Fields per collection that queries have filtered on, with how often.
    pub filtered_fields: &'a [(String, String, u64)],
    /// Whether a collection's schema permits constant-size records.
    pub fixed_size_collections: &'a [String],
    /// Highest record id in use per collection.
    ///
    /// Needed to judge *density* before any directly-addressed array exists: a
    /// flat array spans `max_id + 1` slots regardless of how many are occupied,
    /// so a sparse id space makes it mostly wasted memory.
    pub max_ids: &'a [(String, u64)],
    /// Indexes that already exist, as (collection, field, kind).
    pub existing_indexes: &'a [(String, String, adabt_core::index_kind::IndexKind)],
    /// Approximate bytes the database currently occupies.
    ///
    /// The scale resource savings are judged against. An absolute reference
    /// makes small databases un-optimizable: halving a 600KB store registers as
    /// nothing against a fixed 8GiB yardstick, while the CPU it costs is
    /// charged in full. "Fraction of what this database costs" is the unit that
    /// means the same thing at every size.
    pub current_bytes: u64,
}

impl OptContext<'_> {
    pub fn rows_in(&self, collection: &str) -> usize {
        self.collections
            .iter()
            .find(|(c, _)| c == collection)
            .map(|(_, n)| *n)
            .unwrap_or(0)
    }

    pub fn has_index(&self, collection: &str, field: &str) -> bool {
        self.existing_indexes
            .iter()
            .any(|(c, f, _)| c == collection && f == field)
    }

    pub fn is_fixed_size(&self, collection: &str) -> bool {
        self.fixed_size_collections.iter().any(|c| c == collection)
    }

    /// Occupied fraction of a collection's id space.
    ///
    /// Returns 1.0 for an unknown or empty collection: with nothing to be
    /// sparse about, density should not be the reason to refuse.
    pub fn density_of(&self, collection: &str) -> f64 {
        let rows = self.rows_in(collection);
        if rows == 0 {
            return 1.0;
        }
        match self.max_ids.iter().find(|(c, _)| c == collection) {
            Some((_, max_id)) => rows as f64 / (*max_id as f64 + 1.0),
            None => 1.0,
        }
    }
}

/// A physical strategy that can be turned on, turned off, and measured.
///
/// Everything here is deliberately *descriptive*: an optimization reports what
/// it would do and what that would cost, and produces a `ChangePlan`. It never
/// touches the engine directly. That is what lets the same definition serve the
/// manual driver today and the adaptive driver later without change.
pub trait Optimization: Send + Sync {
    fn meta(&self) -> &OptMeta;

    /// Whether this is *possible* in the current state.
    ///
    /// Answers "can this be done", never "is this a good idea". Worth is the
    /// estimate's job, and the scorer's after that.
    ///
    /// The distinction is not pedantic — it has been got wrong twice here, the
    /// same way both times. Bulk-loading a database makes the workload look
    /// 100% writes, so any applicability check reading `telemetry.write_fraction`
    /// makes a freshly loaded database refuse its own optimizations forever, on
    /// the strength of its loading phase. A transient workload must not be able
    /// to permanently veto anything. Structural facts — a schema shape, a row
    /// count, an index that already exists — are fair game here; workload
    /// mixes are not.
    fn applicability(&self, ctx: &OptContext<'_>) -> Applicability;

    /// The scopes this optimization could be applied to, independently.
    ///
    /// A global optimization has one. `auto_index` has one per field worth
    /// indexing, which is the point: M8 operated everything at `"global"`, so
    /// retracting `auto_index` dropped *every* index rather than the one not
    /// earning its keep, and it could only retract when all of them were
    /// useless at once.
    fn candidate_scopes(&self, _ctx: &OptContext<'_>) -> Vec<String> {
        vec!["global".to_string()]
    }

    /// Expected effect. May be wrong, which is what `confidence` is for.
    fn estimate(&self, ctx: &OptContext<'_>) -> CostEstimate;

    /// The actions that would enable it for one scope, and the actions that
    /// would undo them.
    ///
    /// `params` is the decision's own tuning numbers — what a caller asked
    /// for explicitly via a manual override's params, or what a previous
    /// `Retune` left in place. Most optimizations have nothing to read here
    /// and ignore it; `auto_index` is the one that does not, honoring an
    /// explicit `kind` rather than guessing one from telemetry.
    fn plan_enable(&self, ctx: &OptContext<'_>, scope: &str, params: &Params) -> ChangePlan;

    /// The actions that would disable it for one scope.
    fn plan_disable(&self, ctx: &OptContext<'_>, scope: &str, params: &Params) -> ChangePlan;
}

/// Whether the policy's guarantees permit this optimization at all.
///
/// A hard filter, evaluated before anything is scored. An optimization the
/// policy forbids is not "expensive" — it is invisible.
pub fn permitted_by(meta: &OptMeta, policy: &Policy) -> bool {
    meta.requires_guarantees.satisfied_by(&policy.guarantees)
}

#[cfg(test)]
mod tests {
    use super::*;
    use adabt_core::policy::{Durability, Guarantees};

    fn meta(req: GuaranteeRequirements) -> OptMeta {
        OptMeta {
            name: "test",
            summary: "",
            scope_kind: ScopeKind::Global,
            min_level: 1,
            axis_effects: AxisEffects::default(),
            requires_guarantees: req,
            prerequisites: &[],
            conflicts_with: &[],
            reversibility: Reversibility::Instant,
        }
    }

    #[test]
    fn a_guarantee_free_optimization_is_permitted_under_any_policy() {
        assert!(permitted_by(
            &meta(GuaranteeRequirements::ANY),
            &Policy::conventional()
        ));
    }

    #[test]
    fn strict_durability_makes_a_relaxed_optimization_invisible() {
        let m = meta(GuaranteeRequirements {
            max_durability: Some(Durability::Relaxed),
            max_consistency: None,
        });
        assert!(!permitted_by(&m, &Policy::conventional()));

        let mut relaxed = Policy::conventional();
        relaxed.guarantees = Guarantees {
            durability: Durability::Relaxed,
            ..Guarantees::default()
        };
        assert!(permitted_by(&m, &relaxed));
    }

    #[test]
    fn scopes_describe_themselves() {
        assert_eq!(OptScope::Global.describe(), "global");
        assert_eq!(OptScope::Collection("users".into()).describe(), "users");
        assert_eq!(
            OptScope::Field {
                collection: "users".into(),
                field: "country".into()
            }
            .describe(),
            "users.country"
        );
    }

    #[test]
    fn applicability_carries_its_reason() {
        assert!(Applicability::Applicable.is_applicable());
        assert_eq!(Applicability::Applicable.reason(), None);
        let n = Applicability::NotYet("too few rows".into());
        assert!(!n.is_applicable());
        assert_eq!(n.reason(), Some("too few rows"));
    }

    #[test]
    fn reversibility_orders_from_cheap_to_impossible() {
        assert!(Reversibility::Instant < Reversibility::RebuildRequired);
        assert!(Reversibility::RebuildRequired < Reversibility::Destructive);
    }
}
