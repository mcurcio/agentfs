//! The Clients domain (docs/trees.md, "Clients"; docs/library/domains.md):
//! who is attached to which tree, with what grant, and each fork's one
//! writer. Pure, over the ports in `agentfs_contracts::ports`.
#![cfg_attr(not(test), no_std)]

extern crate alloc;

mod service;

pub use service::ClientsService;
