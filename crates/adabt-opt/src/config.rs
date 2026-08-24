//! The granular optimization configuration.
//!
//! **The config is the real state; a level is only sugar.** A level resolves
//! into one of these at startup, explicit user overrides are applied on top, and
//! from that point nothing in the engine ever asks what level it is running at.
//!
//! That inversion is what stops the codebase filling with `if level >= 7`. It
//! also lets the adaptive driver move to a configuration no level names, which
//! it must be able to do — the workload does not care about round numbers.

use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OptimizationConfig {
    /// Enabled optimizations, keyed by `name` and scope description.
    enabled: BTreeMap<(String, String), Params>,
}

/// Tunable numbers attached to an enabled optimization.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Params {
    values: BTreeMap<String, i64>,
}

impl Params {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn with(mut self, k: impl Into<String>, v: i64) -> Self {
        self.values.insert(k.into(), v);
        self
    }
    pub fn get(&self, k: &str) -> Option<i64> {
        self.values.get(k).copied()
    }
    pub fn get_or(&self, k: &str, default: i64) -> i64 {
        self.get(k).unwrap_or(default)
    }
    pub fn iter(&self) -> impl Iterator<Item = (&str, i64)> {
        self.values.iter().map(|(k, v)| (k.as_str(), *v))
    }
}

impl OptimizationConfig {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn enable(&mut self, name: impl Into<String>, scope: impl Into<String>, params: Params) {
        self.enabled.insert((name.into(), scope.into()), params);
    }

    pub fn disable(&mut self, name: &str, scope: &str) -> bool {
        self.enabled
            .remove(&(name.to_string(), scope.to_string()))
            .is_some()
    }

    pub fn is_enabled(&self, name: &str, scope: &str) -> bool {
        self.enabled
            .contains_key(&(name.to_string(), scope.to_string()))
    }

    /// Whether the optimization is on anywhere.
    pub fn is_enabled_anywhere(&self, name: &str) -> bool {
        self.enabled.keys().any(|(n, _)| n == name)
    }

    pub fn params(&self, name: &str, scope: &str) -> Option<&Params> {
        self.enabled.get(&(name.to_string(), scope.to_string()))
    }

    pub fn entries(&self) -> impl Iterator<Item = (&str, &str, &Params)> {
        self.enabled
            .iter()
            .map(|((n, s), p)| (n.as_str(), s.as_str(), p))
    }

    pub fn len(&self) -> usize {
        self.enabled.len()
    }
    pub fn is_empty(&self) -> bool {
        self.enabled.is_empty()
    }

    /// Names enabled here but not in `other`, and vice versa.
    pub fn diff(&self, other: &OptimizationConfig) -> ConfigDiff {
        let mine: Vec<_> = self.enabled.keys().cloned().collect();
        let theirs: Vec<_> = other.enabled.keys().cloned().collect();
        ConfigDiff {
            added: theirs
                .iter()
                .filter(|k| !mine.contains(k))
                .cloned()
                .collect(),
            removed: mine
                .iter()
                .filter(|k| !theirs.contains(k))
                .cloned()
                .collect(),
            retuned: mine
                .iter()
                .filter(|k| theirs.contains(k) && self.enabled[*k] != other.enabled[*k])
                .cloned()
                .collect(),
        }
    }

    pub fn describe(&self) -> String {
        if self.enabled.is_empty() {
            return "no optimizations enabled".to_string();
        }
        self.entries()
            .map(|(n, s, p)| {
                let params: Vec<String> = p.iter().map(|(k, v)| format!("{k}={v}")).collect();
                if params.is_empty() {
                    format!("{n}[{s}]")
                } else {
                    format!("{n}[{s}]({})", params.join(","))
                }
            })
            .collect::<Vec<_>>()
            .join(", ")
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConfigDiff {
    pub added: Vec<(String, String)>,
    pub removed: Vec<(String, String)>,
    pub retuned: Vec<(String, String)>,
}

impl ConfigDiff {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.retuned.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enable_and_disable_round_trip() {
        let mut c = OptimizationConfig::new();
        assert!(!c.is_enabled("plan_cache", "global"));
        c.enable("plan_cache", "global", Params::new().with("entries", 256));
        assert!(c.is_enabled("plan_cache", "global"));
        assert_eq!(
            c.params("plan_cache", "global").unwrap().get("entries"),
            Some(256)
        );
        assert!(c.disable("plan_cache", "global"));
        assert!(!c.is_enabled("plan_cache", "global"));
        assert!(
            !c.disable("plan_cache", "global"),
            "second disable should report nothing"
        );
    }

    #[test]
    fn the_same_optimization_can_be_enabled_per_scope() {
        let mut c = OptimizationConfig::new();
        c.enable("auto_index", "users.country", Params::new());
        c.enable("auto_index", "orders.status", Params::new());
        assert_eq!(c.len(), 2);
        assert!(c.is_enabled_anywhere("auto_index"));
        assert!(c.is_enabled("auto_index", "users.country"));
        assert!(!c.is_enabled("auto_index", "users.age"));
    }

    #[test]
    fn params_fall_back_to_a_default() {
        let p = Params::new().with("entries", 10);
        assert_eq!(p.get_or("entries", 99), 10);
        assert_eq!(p.get_or("missing", 99), 99);
    }

    #[test]
    fn diff_reports_additions_removals_and_retunes() {
        let mut a = OptimizationConfig::new();
        a.enable("kept", "global", Params::new());
        a.enable("removed", "global", Params::new());
        a.enable("retuned", "global", Params::new().with("n", 1));

        let mut b = OptimizationConfig::new();
        b.enable("kept", "global", Params::new());
        b.enable("added", "global", Params::new());
        b.enable("retuned", "global", Params::new().with("n", 2));

        let d = a.diff(&b);
        assert_eq!(d.added, vec![("added".to_string(), "global".to_string())]);
        assert_eq!(
            d.removed,
            vec![("removed".to_string(), "global".to_string())]
        );
        assert_eq!(
            d.retuned,
            vec![("retuned".to_string(), "global".to_string())]
        );
        assert!(!d.is_empty());
    }

    #[test]
    fn an_identical_config_diffs_to_nothing() {
        let mut a = OptimizationConfig::new();
        a.enable("x", "global", Params::new().with("n", 1));
        assert!(a.diff(&a.clone()).is_empty());
    }

    #[test]
    fn describe_is_readable_and_handles_the_empty_case() {
        let mut c = OptimizationConfig::new();
        assert_eq!(c.describe(), "no optimizations enabled");
        c.enable("plan_cache", "global", Params::new().with("entries", 64));
        assert_eq!(c.describe(), "plan_cache[global](entries=64)");
    }
}
