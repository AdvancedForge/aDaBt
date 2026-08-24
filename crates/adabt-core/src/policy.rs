//! Optimization policy: what the user wants, expressed so that both the manual
//! driver and the future adaptive driver can consume it.
//!
//! Two rules here must never be softened, because the whole safety story of
//! automatic physical change rests on them:
//!
//! 1. **Guarantees are a hard eligibility filter, not a scoring penalty.** An
//!    optimization whose requirements are not met by the policy is invisible —
//!    never scored, never experimented with, never suggested. `durability:
//!    strict` hides async-durability techniques even at Level 11.
//! 2. **Constraints are hard limits; priorities are soft objectives.** A
//!    candidate that would exceed `max_ram` is infeasible no matter how much
//!    speed it buys.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    /// The user picks. The engine must not change strategy on its own.
    Manual { level: u8, overrides: Vec<Override> },
    /// The user states priorities; the engine picks and revises.
    Adaptive,
}

/// One explicit thing an expert wants the optimizer to do, on top of
/// whatever `level` already turns on.
///
/// `("column_store", true)` used to be all an override could say — an
/// on/off switch, always applied at the one scope the driver knew about:
/// `"global"`. That was never expressive enough for "index users.country
/// hash" or "compile shape X", both of which name a specific *place* to
/// act, not just a capability to flip. `scope` and `params` are what close
/// that gap, and they reuse the exact vocabulary `adabt-opt`'s own
/// `Registry`/`Optimization` machinery already has for the same two
/// concepts — `OptScope`'s `describe()` form for scope strings, and
/// per-optimization tuning numbers for params — rather than inventing a
/// second one at the policy layer.
///
/// `params` is `Vec<(String, i64)>`, not `adabt_opt::config::Params`,
/// because `adabt-core` has no dependency on `adabt-opt` and must not
/// gain one just to name a policy directive — the same reasoning that put
/// `IndexKind` here instead of in the crate that builds one. `ManualDriver`
/// converts this into a real `Params` where it is actually consumed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Override {
    pub name: String,
    /// `"global"` for a whole-database toggle; a scope string like
    /// `"users.country"` (collection.field) to name one specific place —
    /// see `adabt_opt::optimization::OptScope::describe` for the forms this
    /// mirrors. An optimization whose `ScopeKind` is not `Global` and whose
    /// override is left at `"global"` is expanded into every scope that
    /// currently qualifies, exactly as a level preset already is; naming a
    /// scope explicitly targets that one place instead.
    pub scope: String,
    pub enabled: bool,
    /// Tuning numbers for this override — for example, an explicit index
    /// kind (`IndexKind::as_ordinal`) so "index users.country hash" means
    /// hash and not whatever the workload's telemetry would have guessed.
    /// Empty for an override that only wants the default.
    pub params: Vec<(String, i64)>,
}

impl Override {
    /// A global on/off toggle — everything an override could express before
    /// `scope` and `params` existed.
    pub fn toggle(name: impl Into<String>, enabled: bool) -> Self {
        Self {
            name: name.into(),
            scope: "global".into(),
            enabled,
            params: Vec::new(),
        }
    }

    /// Name a specific scope — a collection, a `"collection.field"` pair, or
    /// whatever else the named optimization's `ScopeKind` expects — instead
    /// of defaulting to `"global"`.
    pub fn scoped(name: impl Into<String>, scope: impl Into<String>, enabled: bool) -> Self {
        Self {
            name: name.into(),
            scope: scope.into(),
            enabled,
            params: Vec::new(),
        }
    }

    pub fn with_param(mut self, key: impl Into<String>, value: i64) -> Self {
        self.params.push((key.into(), value));
        self
    }
}

impl Default for Mode {
    fn default() -> Self {
        Mode::Manual {
            level: 0,
            overrides: Vec::new(),
        }
    }
}

/// Soft objectives, each 0-10. These weight the multi-objective score; they
/// never make an infeasible candidate feasible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Priorities {
    pub speed: u8,
    pub resources: u8,
    pub freedom: u8,
}

impl Default for Priorities {
    fn default() -> Self {
        Self {
            speed: 5,
            resources: 5,
            freedom: 5,
        }
    }
}

impl Priorities {
    pub fn clamped(self) -> Self {
        Self {
            speed: self.speed.min(10),
            resources: self.resources.min(10),
            freedom: self.freedom.min(10),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Durability {
    /// Every commit is fsynced before it is acknowledged.
    Strict,
    /// Commits are batched and fsynced together; a crash may lose the last group.
    GroupCommit,
    /// Acknowledged before the write reaches stable storage.
    Relaxed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Consistency {
    Strict,
    Snapshot,
    Eventual,
}

/// What the database promises regardless of how it is optimized.
///
/// Ordering matters: each enum is declared strongest-first, so `<=` means
/// "at least as strong as", which is what the eligibility check needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Guarantees {
    pub durability: Durability,
    pub consistency: Consistency,
}

impl Default for Guarantees {
    fn default() -> Self {
        Self {
            durability: Durability::Strict,
            consistency: Consistency::Strict,
        }
    }
}

/// What an optimization needs the policy to permit before it may be considered.
///
/// An optimization that weakens durability declares the weakest durability it
/// can live with; if the policy demands something stronger, it is ineligible.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GuaranteeRequirements {
    /// The strongest durability this optimization can still honour.
    pub max_durability: Option<Durability>,
    /// The strongest consistency this optimization can still honour.
    pub max_consistency: Option<Consistency>,
}

impl GuaranteeRequirements {
    /// No weakening required: usable under any policy.
    pub const ANY: Self = Self {
        max_durability: None,
        max_consistency: None,
    };

    /// Whether `g` permits this optimization. Called before scoring, and its
    /// result is never traded off against a benefit.
    pub fn satisfied_by(&self, g: &Guarantees) -> bool {
        if let Some(maxd) = self.max_durability {
            // Durability is declared strongest-first, so a policy demanding
            // something stronger than we support sorts *before* our maximum.
            if g.durability < maxd {
                return false;
            }
        }
        if let Some(maxc) = self.max_consistency {
            if g.consistency < maxc {
                return false;
            }
        }
        true
    }
}

/// Hard resource ceilings. `None` means unbounded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Constraints {
    /// Extra memory an *optimization* may commit while building a new
    /// derived representation — read only by `adabt-opt`'s cost model, never
    /// by query execution. A tight ceiling here says "don't let the driver
    /// build expensive structures," not "don't let a query buffer much,"
    /// which is a different question with its own field below: a policy
    /// tuned to keep the optimizer thrifty is not thereby also asking every
    /// sort and aggregate to run in a few kilobytes.
    pub max_ram_bytes: Option<u64>,
    /// Ceiling on what one query may buffer at once — read only by
    /// `adabt_exec::exec`'s `Sort`/`Aggregate` operators, via
    /// `Database::query`/`ShardedDatabase::query`. See `max_ram_bytes` for
    /// why this is not the same field.
    pub max_query_ram_bytes: Option<u64>,
    pub max_storage_bytes: Option<u64>,
    pub max_cpu_cores: Option<u32>,
    pub max_build_secs: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Policy {
    pub mode: Mode,
    pub priority: Priorities,
    pub guarantees: Guarantees,
    pub constraints: Constraints,
}

impl Policy {
    /// The default posture: conventional, general-purpose, fully safe.
    pub fn conventional() -> Self {
        Self::default()
    }

    pub fn manual(level: u8) -> Self {
        Policy {
            mode: Mode::Manual {
                level,
                overrides: Vec::new(),
            },
            ..Default::default()
        }
    }

    pub fn level(&self) -> u8 {
        match &self.mode {
            Mode::Manual { level, .. } => *level,
            // Adaptive mode has no level; it works from the granular config.
            Mode::Adaptive => 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strict_durability_hides_relaxed_optimizations() {
        let async_persist = GuaranteeRequirements {
            max_durability: Some(Durability::Relaxed),
            ..Default::default()
        };
        let strict = Guarantees::default();
        assert!(!async_persist.satisfied_by(&strict));

        let relaxed = Guarantees {
            durability: Durability::Relaxed,
            ..Guarantees::default()
        };
        assert!(async_persist.satisfied_by(&relaxed));

        // Group commit sits between the two: still too weak for a strict policy.
        let group = Guarantees {
            durability: Durability::GroupCommit,
            ..Guarantees::default()
        };
        assert!(!async_persist.satisfied_by(&group));
    }

    #[test]
    fn guarantee_free_optimizations_are_always_eligible() {
        let strict = Guarantees::default();
        assert!(GuaranteeRequirements::ANY.satisfied_by(&strict));
    }

    #[test]
    fn consistency_is_gated_independently_of_durability() {
        let req = GuaranteeRequirements {
            max_durability: None,
            max_consistency: Some(Consistency::Eventual),
        };
        assert!(!req.satisfied_by(&Guarantees::default()));
        assert!(req.satisfied_by(&Guarantees {
            durability: Durability::Strict,
            consistency: Consistency::Eventual,
        }));
    }
}
