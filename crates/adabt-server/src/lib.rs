//! Network server.
//!
//! Length-prefixed binary frames over TCP, a thread per connection, and one
//! engine behind one lock. The engine remains an embedded library first — that
//! is what keeps the network out of the measurement loop — and this is the seam
//! where the Level 9 roadmap items land: zero-copy paths, `io_uring`, per-core
//! accept. None of those exist yet, and [`server`] says plainly what does.
//!
//! The client is here too, deliberately. A protocol with one implementation is
//! a protocol nobody has checked, and writing the reader against the writer is
//! how a field that is encoded but never decoded gets noticed.

pub mod client;
pub mod protocol;
pub mod server;
pub mod wire;

pub use client::Client;
pub use protocol::{Frame, RequestKind, StatusCode, HEADER_LEN, MAX_FRAME, PROTOCOL_VERSION};
pub use server::{Server, Stopper};
