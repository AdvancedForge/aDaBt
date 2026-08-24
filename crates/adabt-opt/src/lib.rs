//! The optimization framework.
//!
//! This crate depends on `adabt-core`, `adabt-ir` and `adabt-telemetry` — and on
//! nothing physical. There is no storage, no index, no execution here. An
//! optimization *describes* what should change as a list of `Action`s; an engine
//! elsewhere carries them out.
//!
//! The layering is the point. Manual selection and adaptive selection are two
//! implementations of `OptimizationDriver`, and both feed the same
//! `OptimizationController`, so nothing the optimizer can do is something a
//! human could not have asked for by hand — and vice versa.

pub mod action;
pub mod adaptive;
pub mod config;
pub mod controller;
pub mod cost;
pub mod decision;
pub mod driver;
pub mod experiment;
pub mod levels;
pub mod memory;
pub mod model;
pub mod optimization;
pub mod registry;
pub mod score;
pub mod search;

pub use action::{Action, ActionSink, ChangePlan};
pub use adaptive::AdaptiveDriver;
pub use config::OptimizationConfig;
pub use controller::OptimizationController;
pub use cost::{AxisEffects, BuildCost, CostEstimate, Ratio};
pub use decision::{Decision, DecisionLog, DecisionRecord, Verdict};
pub use driver::{DriverInput, ManualDriver, OptimizationDriver};
pub use experiment::{Assessment, Experiment, Guardrails, Phase};
pub use levels::{level_preset, MAX_LEVEL};
pub use memory::{Fingerprint, WorkloadMemory};
pub use model::{CostModel, Metrics, Observation};
pub use optimization::{
    permitted_by, Applicability, OptContext, OptMeta, OptScope, Optimization, Reversibility,
    ScopeKind,
};
pub use registry::Registry;
pub use score::{score, Score};
pub use search::{best_combination, Candidate, Combination};
