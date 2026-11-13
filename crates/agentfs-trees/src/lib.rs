//! The Trees domain (docs/target-architecture/domains/trees.md): each fork's
//! lifecycle, from create to drop. Pure, over the ports in
//! `agentfs_contracts::ports`; branches and commits stay the git model's.

mod service;

pub use service::TreesService;
