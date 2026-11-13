//! agentfs's shared kernel and contracts (docs/target-architecture/system.md):
//! the values every domain shares, the capabilities and their minter, each
//! domain's trait, error and events, and the ports their adapters implement.
//! No I/O: the domain crates (`agentfs-volume`, `agentfs-clients`,
//! `agentfs-trees`) and the adapters in `agentfs-core` depend on it, and it
//! depends on no other agentfs crate.

pub mod audit;
pub mod caps;
pub mod clients;
pub mod name;
pub mod pod;
pub mod ports;
pub mod tree;
pub mod trees;
pub mod views;
pub mod volume;

pub use tree::{ForkId, NotATree, TreeId};
