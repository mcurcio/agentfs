//! One tree volume, composed: the Volume, Trees and Clients domains wired
//! onto the engine's store as one launcher, the one queue that hands each
//! domain event to its consumers after the call that caused it returns, and
//! the drain at a stop until Volume releases the volume. It is the one place
//! a volume is composed: the `agentfs` binary serves its views on it, and a
//! consumer of the library crate `agentfs` reaches it as `agentfs::service`.
//! It mints nothing: its caller's composition root hands it the
//! capabilities. It wires domains to adapters; the domains decide.
//!
//! See docs/library/service.md.

mod domains;
mod error;
mod events;
mod launch;
mod shared;

pub use self::domains::{Domains, Unstaged};
pub use self::error::ServiceError;
pub use self::launch::{Launcher, end_fork, init};
