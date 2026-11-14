//! Query intermediate representation.
//!
//! Pure: no storage, no execution, no engine. Both the planner and the
//! optimizer depend on it, so it must stay free of either. (Telemetry does
//! not: it carries its own `QueryShape` newtype, which the engine fills from
//! this crate's hash.)

pub mod expr;
pub mod plan;
pub mod shape;
pub mod sql;
pub mod vm;

pub use expr::{CmpOp, Expr, Truth};
pub use plan::{Agg, AggKind, LogicalOp, LogicalPlan, SortKey};
pub use shape::{QueryKey, QueryShape};
pub use sql::{parse_select, SqlError};
pub use vm::Program;
