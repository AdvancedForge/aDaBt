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
    Manual {
        level: u8,
        overrides: Vec<(String, bool)>,
    },
    /// The user states priorities; the engine picks and revises.
    Adaptive,
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
    pub max_ram_bytes: Option<u64>,
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
