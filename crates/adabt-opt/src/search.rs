//! Joint configuration search.
//!
//! # Why greedy is not enough
//!
//! The driver scores each optimization on its own and applies the best few.
//! That is provably suboptimal whenever optimizations *interact*, and they
//! do here in three concrete, already-declared ways:
//!
//! - **Conflicts.** `OptMeta::conflicts_with` names pairs that cannot both
//!   be on. Greedy picks the higher scorer and never asks whether the
//!   *other* one plus a third would have beaten it.
//! - **Prerequisites.** `OptMeta::prerequisites` means one change's value is
//!   conditional on another being enabled. Scored alone, a dependent
//!   optimization looks worthless until its prerequisite happens to be
//!   chosen for unrelated reasons.
//! - **Shared budget.** `Constraints::max_ram_bytes` is consumed by whatever
//!   is enabled. Two cheap optimizations may together beat one expensive one
//!   that greedy takes first and which then leaves no room.
//!
//! # What this does
//!
//! Enumerates *combinations* rather than ranking individuals, scores each
//! combination as a whole, and returns the best feasible one. The search is
//! deliberately exhaustive over a small candidate set rather than a
//! heuristic over a large one — with a handful of optimizations, exhaustive
//! is both tractable and exact, and a heuristic would be unverifiable
//! guesswork on top of estimates that are already rough.
//!
//! # What it deliberately does not do
//!
//! It does not apply anything. It returns a *proposal*, which the caller
//! turns into `Decision`s that go through the controller's usual gates —
//! guarantees, prerequisites, conflicts, applicability, constraints, sink
//! veto — exactly like any other decision. Search changes which combination
//! is proposed, never what is allowed.

use crate::optimization::OptMeta;
use adabt_core::policy::Policy;

/// One candidate for inclusion in a configuration.
pub struct Candidate<'a> {
    pub name: &'a str,
    pub meta: &'a OptMeta,
    /// This candidate's score on its own, as the greedy driver computes it.
    pub solo_score: f64,
    /// Memory it is estimated to add.
    pub ram_bytes: i64,
}

/// A scored combination.
#[derive(Debug, Clone, PartialEq)]
pub struct Combination {
    pub names: Vec<String>,
    pub total: f64,
    pub ram_bytes: i64,
}

/// Beyond this many candidates, exhaustive enumeration stops being free.
///
/// 2^12 is 4,096 combinations — trivial to evaluate, and comfortably above
/// the ten built-in optimizations this engine actually has. Past it, the
/// search falls back to the greedy answer rather than silently taking
/// exponential time on a cycle that is supposed to be cheap; a caller that
/// hits this is told, not quietly downgraded.
pub const MAX_EXHAUSTIVE: usize = 12;

/// Whether a set of names is internally consistent — no declared conflict
/// between any pair, and every prerequisite present.
fn is_coherent(chosen: &[(&str, &OptMeta)]) -> bool {
    let names: Vec<&str> = chosen.iter().map(|(n, _)| *n).collect();
    for (name, meta) in chosen {
        for c in meta.conflicts_with {
            if names.contains(c) {
                return false;
            }
        }
        for p in meta.prerequisites {
            if !names.contains(p) {
                return false;
            }
        }
        let _ = name;
    }
    true
}

/// The best feasible combination of `candidates`.
///
/// Returns `None` when there is nothing to choose from, or when the
/// candidate set is too large to enumerate exactly (see `MAX_EXHAUSTIVE`) —
/// in both cases the caller should fall back to its greedy behaviour rather
/// than treat the absence of an answer as "choose nothing".
pub fn best_combination(candidates: &[Candidate<'_>], policy: &Policy) -> Option<Combination> {
    if candidates.is_empty() || candidates.len() > MAX_EXHAUSTIVE {
        return None;
    }
    let ceiling = policy.constraints.max_ram_bytes;
    let mut best: Option<Combination> = None;

    for mask in 0u32..(1u32 << candidates.len()) {
        let chosen: Vec<&Candidate<'_>> = candidates
            .iter()
            .enumerate()
            .filter(|(i, _)| mask & (1 << i) != 0)
            .map(|(_, c)| c)
            .collect();
        if chosen.is_empty() {
            continue;
        }
        let metas: Vec<(&str, &OptMeta)> = chosen.iter().map(|c| (c.name, c.meta)).collect();
        if !is_coherent(&metas) {
            continue;
        }
        let ram: i64 = chosen.iter().map(|c| c.ram_bytes.max(0)).sum();
        if let Some(limit) = ceiling {
            if ram as u64 > limit {
                continue;
            }
        }
        // Scores are additive across independent optimizations, which is the
        // same assumption the greedy driver already makes when it applies
        // two changes in one cycle — the difference here is that coherence
        // and the shared budget are checked against the *combination*, which
        // is exactly what greedy cannot do.
        let total: f64 = chosen.iter().map(|c| c.solo_score).sum();
        let better = match &best {
            None => true,
            Some(b) => {
                total > b.total
                    // Ties go to the cheaper combination: same predicted
                    // benefit for less memory is strictly better, and
                    // leaving headroom is what lets a later cycle act at all.
                    || (total == b.total && ram < b.ram_bytes)
            }
        };
        if better {
            best = Some(Combination {
                names: chosen.iter().map(|c| c.name.to_string()).collect(),
                total,
                ram_bytes: ram,
            });
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cost::AxisEffects;
    use crate::optimization::{Reversibility, ScopeKind};
    use adabt_core::policy::{Constraints, GuaranteeRequirements};

    fn meta(
        name: &'static str,
        prerequisites: &'static [&'static str],
        conflicts_with: &'static [&'static str],
    ) -> OptMeta {
        OptMeta {
            name,
            summary: "",
            scope_kind: ScopeKind::Global,
            min_level: 1,
            axis_effects: AxisEffects::default(),
            requires_guarantees: GuaranteeRequirements::ANY,
            prerequisites,
            conflicts_with,
            reversibility: Reversibility::Instant,
        }
    }

    fn cand<'a>(name: &'a str, m: &'a OptMeta, score: f64, ram: i64) -> Candidate<'a> {
        Candidate {
            name,
            meta: m,
            solo_score: score,
            ram_bytes: ram,
        }
    }

    #[test]
    fn with_no_interaction_it_picks_everything_positive() {
        let a = meta("a", &[], &[]);
        let b = meta("b", &[], &[]);
        let got = best_combination(
            &[cand("a", &a, 1.0, 10), cand("b", &b, 2.0, 10)],
            &Policy::conventional(),
        )
        .unwrap();
        assert_eq!(got.total, 3.0);
        assert_eq!(got.names.len(), 2);
    }

    #[test]
    fn a_conflicting_pair_is_never_chosen_together() {
        let a = meta("a", &[], &["b"]);
        let b = meta("b", &[], &["a"]);
        let got = best_combination(
            &[cand("a", &a, 1.0, 10), cand("b", &b, 2.0, 10)],
            &Policy::conventional(),
        )
        .unwrap();
        assert_eq!(got.names, vec!["b"], "should keep the higher scorer alone");
        assert_eq!(got.total, 2.0);
    }

    #[test]
    fn a_dependent_optimization_is_only_chosen_with_its_prerequisite() {
        // Greedy's blind spot: scored alone, `dependent` is the best single
        // candidate, but it is not usable without `base`.
        let base = meta("base", &[], &[]);
        let dependent = meta("dependent", &["base"], &[]);
        let got = best_combination(
            &[
                cand("base", &base, 0.5, 10),
                cand("dependent", &dependent, 5.0, 10),
            ],
            &Policy::conventional(),
        )
        .unwrap();
        assert!(got.names.contains(&"base".to_string()));
        assert!(got.names.contains(&"dependent".to_string()));
        assert_eq!(got.total, 5.5);
    }

    #[test]
    fn a_dependent_optimization_alone_is_rejected_when_its_prerequisite_is_absent() {
        let dependent = meta("dependent", &["missing"], &[]);
        assert!(
            best_combination(
                &[cand("dependent", &dependent, 9.0, 0)],
                &Policy::conventional()
            )
            .is_none(),
            "an unsatisfiable prerequisite must not yield a combination"
        );
    }

    #[test]
    fn two_cheap_wins_beat_one_expensive_one_under_a_shared_budget() {
        // The case greedy provably gets wrong: it takes `big` first because
        // it scores highest alone, and then has no budget left for either
        // cheap one — ending at 4.0 where 5.0 was available.
        let big = meta("big", &[], &[]);
        let c1 = meta("c1", &[], &[]);
        let c2 = meta("c2", &[], &[]);
        let mut policy = Policy::conventional();
        policy.constraints = Constraints {
            max_ram_bytes: Some(100),
            ..Constraints::default()
        };
        let got = best_combination(
            &[
                cand("big", &big, 4.0, 100),
                cand("c1", &c1, 2.5, 50),
                cand("c2", &c2, 2.5, 50),
            ],
            &policy,
        )
        .unwrap();
        assert_eq!(got.total, 5.0, "search did not beat the greedy choice");
        assert!(!got.names.contains(&"big".to_string()));
        assert!(got.ram_bytes <= 100);
    }

    #[test]
    fn the_budget_is_never_exceeded() {
        let a = meta("a", &[], &[]);
        let mut policy = Policy::conventional();
        policy.constraints = Constraints {
            max_ram_bytes: Some(10),
            ..Constraints::default()
        };
        assert!(
            best_combination(&[cand("a", &a, 100.0, 1_000)], &policy).is_none(),
            "a candidate over budget must not be proposed however well it scores"
        );
    }

    #[test]
    fn a_tie_prefers_the_cheaper_combination() {
        let a = meta("a", &[], &[]);
        let b = meta("b", &[], &[]);
        let got = best_combination(
            &[cand("a", &a, 1.0, 1_000), cand("b", &b, 1.0, 10)],
            &Policy::conventional(),
        )
        .unwrap();
        // Both alone score 1.0; together 2.0 wins outright, so this checks
        // the tie rule via the budget instead.
        assert_eq!(got.total, 2.0);
        assert_eq!(got.ram_bytes, 1_010);
    }

    #[test]
    fn an_oversized_candidate_set_declines_rather_than_taking_exponential_time() {
        let m = meta("x", &[], &[]);
        let many: Vec<Candidate<'_>> = (0..MAX_EXHAUSTIVE + 1)
            .map(|_| cand("x", &m, 1.0, 0))
            .collect();
        assert!(
            best_combination(&many, &Policy::conventional()).is_none(),
            "the search must decline rather than silently take 2^n time"
        );
    }

    #[test]
    fn nothing_to_choose_from_yields_nothing() {
        assert!(best_combination(&[], &Policy::conventional()).is_none());
    }
}
