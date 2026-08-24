//! The optimization registry.
//!
//! Registration is explicit — `register_builtins(&mut registry)` — rather than
//! collected by a linker trick. The set of optimizations is then greppable, its
//! order is deterministic, and a test can register three fakes without the real
//! ones appearing behind its back.

use crate::optimization::{OptMeta, Optimization};
use std::collections::HashMap;

#[derive(Default)]
pub struct Registry {
    order: Vec<&'static str>,
    items: HashMap<&'static str, Box<dyn Optimization>>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register an optimization. Panics on a duplicate name: two optimizations
    /// answering to one name would make the config ambiguous, and silently
    /// keeping one of them would be worse than failing loudly at startup.
    pub fn register(&mut self, opt: Box<dyn Optimization>) {
        let name = opt.meta().name;
        assert!(
            !self.items.contains_key(name),
            "duplicate optimization name: {name}"
        );
        self.order.push(name);
        self.items.insert(name, opt);
    }

    pub fn get(&self, name: &str) -> Option<&dyn Optimization> {
        self.items.get(name).map(|b| b.as_ref())
    }

    pub fn contains(&self, name: &str) -> bool {
        self.items.contains_key(name)
    }

    /// Check a manual policy's `overrides` name real optimizations, before
    /// any of them are applied.
    ///
    /// `ManualDriver::decide` runs every optimize cycle and silently skips
    /// an override naming something unregistered — necessarily, since it has
    /// no way to report an error mid-cycle and a stale name from a build
    /// that has since dropped an optimization must not crash a running
    /// database. That silence is fine for *drift*; it is a poor way to learn
    /// about a *typo* in a policy nobody has applied yet. This exists to be
    /// called once, at the one point a caller can still act on a bad name —
    /// opening the database — checked with an error message specific enough
    /// that "auto_indx" (missing an `e`) is fixed on the first try rather
    /// than found by wondering why an index never appeared.
    pub fn validate_overrides(
        &self,
        overrides: &[adabt_core::policy::Override],
    ) -> adabt_core::error::Result<()> {
        let mut unknown: Vec<&str> = overrides
            .iter()
            .map(|ov| ov.name.as_str())
            .filter(|name| !self.contains(name))
            .collect();
        if unknown.is_empty() {
            return Ok(());
        }
        unknown.sort_unstable();
        unknown.dedup();
        Err(adabt_core::error::Error::InvalidOptimization(format!(
            "unknown optimization(s) in policy overrides: {}",
            unknown.join(", ")
        )))
    }

    pub fn names(&self) -> &[&'static str] {
        &self.order
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &dyn Optimization> {
        self.order.iter().map(|n| self.items[n].as_ref())
    }

    pub fn meta(&self, name: &str) -> Option<&OptMeta> {
        self.get(name).map(|o| o.meta())
    }

    /// Names in dependency order: an optimization always follows its
    /// prerequisites.
    ///
    /// Returns `Err` naming the cycle if the prerequisites are circular, rather
    /// than looping or silently dropping one.
    pub fn dependency_order(&self) -> Result<Vec<&'static str>, String> {
        let mut visited: HashMap<&'static str, u8> = HashMap::new();
        let mut out = Vec::new();

        fn visit(
            reg: &Registry,
            name: &'static str,
            visited: &mut HashMap<&'static str, u8>,
            out: &mut Vec<&'static str>,
            path: &mut Vec<&'static str>,
        ) -> Result<(), String> {
            match visited.get(name) {
                Some(2) => return Ok(()),
                Some(1) => {
                    path.push(name);
                    return Err(format!("prerequisite cycle: {}", path.join(" -> ")));
                }
                _ => {}
            }
            visited.insert(name, 1);
            path.push(name);
            if let Some(meta) = reg.meta(name) {
                for p in meta.prerequisites {
                    // An unknown prerequisite is a registration bug; report it
                    // rather than quietly treating it as satisfied.
                    if !reg.contains(p) {
                        return Err(format!("{name} requires unregistered `{p}`"));
                    }
                    visit(reg, reg.meta(p).unwrap().name, visited, out, path)?;
                }
            }
            path.pop();
            visited.insert(name, 2);
            out.push(name);
            Ok(())
        }

        for name in &self.order {
            let mut path = Vec::new();
            visit(self, name, &mut visited, &mut out, &mut path)?;
        }
        Ok(out)
    }

    /// Optimizations whose `min_level` is at or below `level`.
    pub fn at_level(&self, level: u8) -> Vec<&'static str> {
        self.order
            .iter()
            .filter(|n| self.meta(n).is_some_and(|m| m.min_level <= level))
            .copied()
            .collect()
    }

    /// Names that conflict with `name` and are present in `enabled`.
    pub fn conflicts_among(&self, name: &str, enabled: &[String]) -> Vec<String> {
        let Some(meta) = self.meta(name) else {
            return Vec::new();
        };
        let mut out: Vec<String> = meta
            .conflicts_with
            .iter()
            .filter(|c| enabled.iter().any(|e| e == *c))
            .map(|c| c.to_string())
            .collect();
        // Conflict is symmetric even when only one side declares it: an
        // optimization must not slip through because the other named it first.
        for e in enabled {
            if let Some(m) = self.meta(e) {
                if m.conflicts_with.contains(&name) && !out.contains(e) {
                    out.push(e.clone());
                }
            }
        }
        out.sort();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::ChangePlan;
    use crate::config::Params;
    use crate::cost::{AxisEffects, CostEstimate};
    use crate::optimization::{Applicability, OptContext, Reversibility, ScopeKind};
    use adabt_core::policy::GuaranteeRequirements;

    struct Fake(OptMeta);

    impl Fake {
        fn make(
            name: &'static str,
            min_level: u8,
            prerequisites: &'static [&'static str],
            conflicts_with: &'static [&'static str],
        ) -> Box<dyn Optimization> {
            Box::new(Fake(OptMeta {
                name,
                summary: "",
                scope_kind: ScopeKind::Global,
                min_level,
                axis_effects: AxisEffects::default(),
                requires_guarantees: GuaranteeRequirements::ANY,
                prerequisites,
                conflicts_with,
                reversibility: Reversibility::Instant,
            }))
        }
    }

    impl Optimization for Fake {
        fn meta(&self) -> &OptMeta {
            &self.0
        }
        fn applicability(&self, _: &OptContext<'_>) -> Applicability {
            Applicability::Applicable
        }
        fn estimate(&self, _: &OptContext<'_>) -> CostEstimate {
            CostEstimate::neutral()
        }
        fn plan_enable(&self, _: &OptContext<'_>, _scope: &str, _params: &Params) -> ChangePlan {
            ChangePlan::default()
        }
        fn plan_disable(&self, _: &OptContext<'_>, _scope: &str, _params: &Params) -> ChangePlan {
            ChangePlan::default()
        }
    }

    #[test]
    fn registration_and_lookup_work() {
        let mut r = Registry::new();
        r.register(Fake::make("a", 1, &[], &[]));
        r.register(Fake::make("b", 2, &[], &[]));
        assert_eq!(r.len(), 2);
        assert!(r.contains("a"));
        assert!(r.get("missing").is_none());
        assert_eq!(r.names(), &["a", "b"]);
    }

    #[test]
    #[should_panic(expected = "duplicate optimization name")]
    fn a_duplicate_name_fails_loudly() {
        let mut r = Registry::new();
        r.register(Fake::make("a", 1, &[], &[]));
        r.register(Fake::make("a", 1, &[], &[]));
    }

    #[test]
    fn dependency_order_puts_prerequisites_first() {
        let mut r = Registry::new();
        r.register(Fake::make("late", 1, &["early"], &[]));
        r.register(Fake::make("early", 1, &[], &[]));
        let order = r.dependency_order().unwrap();
        let pos = |n| order.iter().position(|x| *x == n).unwrap();
        assert!(pos("early") < pos("late"));
    }

    #[test]
    fn a_prerequisite_cycle_is_reported_rather_than_looping() {
        let mut r = Registry::new();
        r.register(Fake::make("a", 1, &["b"], &[]));
        r.register(Fake::make("b", 1, &["a"], &[]));
        let err = r.dependency_order().unwrap_err();
        assert!(err.contains("cycle"), "{err}");
    }

    #[test]
    fn an_unregistered_prerequisite_is_an_error_not_a_silent_pass() {
        let mut r = Registry::new();
        r.register(Fake::make("a", 1, &["nonexistent"], &[]));
        let err = r.dependency_order().unwrap_err();
        assert!(err.contains("unregistered"), "{err}");
    }

    #[test]
    fn at_level_includes_everything_up_to_that_level() {
        let mut r = Registry::new();
        r.register(Fake::make("l1", 1, &[], &[]));
        r.register(Fake::make("l3", 3, &[], &[]));
        r.register(Fake::make("l10", 10, &[], &[]));
        assert_eq!(r.at_level(0), Vec::<&str>::new());
        assert_eq!(r.at_level(1), vec!["l1"]);
        assert_eq!(r.at_level(3), vec!["l1", "l3"]);
        assert_eq!(r.at_level(11), vec!["l1", "l3", "l10"]);
    }

    #[test]
    fn conflicts_are_symmetric_even_when_declared_on_one_side() {
        let mut r = Registry::new();
        r.register(Fake::make("row_store", 1, &[], &["column_store"]));
        r.register(Fake::make("column_store", 4, &[], &[]));
        // Declared side.
        assert_eq!(
            r.conflicts_among("row_store", &["column_store".to_string()]),
            vec!["column_store".to_string()]
        );
        // Undeclared side must still be caught.
        assert_eq!(
            r.conflicts_among("column_store", &["row_store".to_string()]),
            vec!["row_store".to_string()]
        );
    }

    #[test]
    fn no_conflict_is_reported_when_the_other_side_is_not_enabled() {
        let mut r = Registry::new();
        r.register(Fake::make("a", 1, &[], &["b"]));
        r.register(Fake::make("b", 1, &[], &[]));
        assert!(r.conflicts_among("a", &[]).is_empty());
    }
}
