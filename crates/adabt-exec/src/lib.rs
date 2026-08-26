//! Query planning and execution.

pub mod batch;
pub mod cost;
pub mod exec;
pub mod physical;
pub mod planner;

pub use batch::{RecordBatch, BATCH_SIZE};
pub use exec::{execute, ExecStats, Source};
pub use physical::{PhysicalOp, PhysicalPlan};
pub use planner::{build_from, decide, plan, AccessDecision, PlanContext, PlanDecision};
