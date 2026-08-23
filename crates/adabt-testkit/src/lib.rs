//! Correctness infrastructure.
//!
//! A database that rewrites its own physical layer needs testing that most
//! projects skip. This crate is built before the engine, not after, because
//! every later milestone is validated against it: the reference model defines
//! what the logical layer must do, and the differential runner proves that no
//! optimization changed it.

pub mod differential;
pub mod generator;
pub mod ops;
pub mod reference;
pub mod rng;

pub use differential::{compare, run, seeds, shrink_prefix, Divergence};
pub use generator::{GenConfig, Generator, OpWeights};
pub use ops::{apply, ErrKind, Op, OpOutcome};
pub use reference::ReferenceStore;
pub use rng::Rng;
