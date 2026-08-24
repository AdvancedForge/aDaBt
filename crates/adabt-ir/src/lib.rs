//! Query intermediate representation.
//!
//! Pure: no storage, no execution, no engine. Both the planner and the
//! telemetry layer depend on it, so it must stay free of either.

pub mod expr;
pub mod plan;
pub mod shape;
pub mod vm;

pub use expr::{CmpOp, Expr, Truth};
pub use plan::{Agg, AggKind, LogicalOp, LogicalPlan, SortKey};
pub use shape::{QueryKey, QueryShape};
pub use vm::Program;
