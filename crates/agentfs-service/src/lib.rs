//! One tree volume, composed: the Volume, Trees and Clients domains wired
//! onto the engine's store as one launcher, the one queue that hands each
//! domain event to its consumers after the call that caused it returns, and
//! the drain at a stop until Volume releases the volume. It is the one place
//! a volume is composed: the `agentfs` binary serves its views on it, and a
//! consumer of the library crate `agentfs` reaches it as `agentfs::service`.
//! It wires domains to adapters and decides nothing.
//!
//! See docs/library/service.md.
