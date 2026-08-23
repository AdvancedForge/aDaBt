//! Workload observation.
//!
//! Two properties are load-bearing and must survive every later change:
//! telemetry costs nothing when compiled out, and bounded, predictable effort
//! when compiled in. A probe that perturbs the workload cannot be used to
//! decide how to optimize that workload.

pub mod collector;
pub mod event;
pub mod histogram;
pub mod probe;

pub use collector::{CollectingProbe, OpStats, Snapshot};
pub use event::{Event, OpKind, QueryShape};
pub use histogram::Histogram;
pub use probe::{NoopProbe, Probe};
