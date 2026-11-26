//! The Trees domain (docs/trees.md, "Forks: keys, end and the drop"): each fork's
//! lifecycle, from create to drop. Pure, over the ports in
//! `agentfs_contracts::ports`; branches and commits stay the git model's.
#![cfg_attr(not(test), no_std)]

extern crate alloc;

mod service;

pub use service::TreesService;
