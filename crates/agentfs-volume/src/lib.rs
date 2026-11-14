//! The Volume domain (docs/target-architecture/domains/volume.md): a volume's
//! identity check at every start (`ensure`), its lifecycle and the release
//! verdict. Pure: the state area, rows and facts are reached through the ports
//! in `agentfs_contracts::ports`, and `decide_ensure` and `decide` take facts
//! and answer decisions.

pub mod ensure;
pub mod release;
mod service;

pub use service::VolumeService;
