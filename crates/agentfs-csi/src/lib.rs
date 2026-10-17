//! The agentfs CSI driver (design §14.2, §17.2).
//!
//! One binary, `agentfs-csi`, runs in the node plugin DaemonSet on every
//! node, beside csi-provisioner in `--node-deployment` mode (F8 R8), and
//! serves all three CSI services on a Unix socket. Every worktree (a
//! volume's files and its branch) lives on one node's own disk:
//!
//! - the controller ([`controller`]) makes worktrees on this node under the
//!   base path, empty, started from another worktree's branch
//!   (`startFrom`), or forked from a source on this node, and answers this
//!   node's topology; DeleteVolume asks the one release decider
//!   ([`agentfs_core::release::decide`]) and never pushes;
//! - the node plugin ([`node`], [`supervisor`]) starts one
//!   `agentfs serve-history` per staged volume and one
//!   `agentfs mount --history-service` front per pod, bind-mounted onto
//!   the kubelet's target, and recovers them when they crash;
//! - the release scan ([`release_scan`]) pushes every worktree no pod
//!   stages until a remote holds its head, and finishes and publishes the
//!   forks whose deletion asked for it (design §11.5).
//!
//! The history services, the release scan's children and a start class's
//! CreateVolume (one fetch) do the network work; DeleteVolume lists each
//! remote once. Nothing else talks to a remote.

/// The CSI driver name (CSIDriver object, StorageClass provisioner).
pub const DRIVER_NAME: &str = "agentfs.mcurcio.github.io";

pub mod csi;
pub mod error;
pub mod layout;
pub mod params;
pub mod records;

pub use error::Error;
pub mod controller;
pub mod identity;
pub mod launcher;
pub mod mounter;
pub mod node;
pub mod release_scan;
pub mod server;
pub mod supervisor;
pub mod volume;
