//! The agentfs CSI driver (docs/csi.md).
//!
//! One binary, `agentfs-csi`, runs in the node plugin DaemonSet on every
//! node, beside csi-provisioner in `--node-deployment` mode, and serves all
//! three CSI services on a Unix socket. Every volume is a tree volume on
//! one node's own disk:
//!
//! - the controller ([`controller`]) makes a claim's tree volume on this
//!   node with `agentfs init` and answers this node's topology; DeleteVolume
//!   drains the volume through its host and keeps its directory;
//! - the node plugin ([`node`], [`supervisor`]) runs one `agentfs host` per
//!   volume ([`launcher`]), drives it over its standard input
//!   ([`host_client`]), and opens each pod's view at the kubelet's target.
//!
//! Nothing here talks to a remote: push is not built on tree volumes.

/// The CSI driver name (CSIDriver object, StorageClass provisioner).
pub const DRIVER_NAME: &str = "agentfs.mcurcio.github.io";

pub mod csi;
pub mod error;
pub mod layout;
pub mod params;
pub mod records;

pub use error::Error;
pub mod controller;
pub mod host_client;
pub mod identity;
pub mod launcher;
pub mod mounter;
pub mod node;
pub mod server;
pub mod supervisor;
pub mod volume;
