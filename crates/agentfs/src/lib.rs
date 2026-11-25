//! agentfs as a library, under one name: tree volumes whose every change is
//! recorded into git history. Re-exports only, one module per crate, each
//! that crate whole: the tree engine ([`engine`]), the shared kernel of its
//! domains ([`contracts`]), the Volume, Clients and Trees domains
//! ([`volume`], [`clients`], [`trees`]) and the Rules domain ([`rules`]).
//! It holds no code of its own, and no crate of the workspace depends on it.
//! docs/target-architecture/future/crate-split.md designs it.

pub use agentfs_clients as clients;
pub use agentfs_contracts as contracts;
pub use agentfs_engine as engine;
pub use agentfs_rules as rules;
pub use agentfs_trees as trees;
pub use agentfs_volume as volume;
