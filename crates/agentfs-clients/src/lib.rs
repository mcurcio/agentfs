//! The Clients domain (docs/target-architecture/domains/clients.md): who is
//! attached to which tree, with what grant, and each fork's one writer. Pure,
//! over the ports in `agentfs_contracts::ports`.

mod service;

pub use service::ClientsService;
