//! The agentfs CSI driver (design §14.2, §17.2).
//!
//! One binary, `agentfs-csi --mode controller|node|all`, serves the CSI
//! gRPC services on a Unix socket:
//!
//! - the controller ([`controller`]) turns a StorageClass's parameters
//!   into a volume directory under a driver-wide base path and creates its
//!   state area in-process; ControllerPublish only records the node, so
//!   Kubernetes' attach machinery enforces one node per volume;
//! - the node plugin ([`node`], [`supervisor`]) starts one
//!   `agentfs serve-history` per staged volume and one
//!   `agentfs mount --history-service` front per pod, bind-mounted onto
//!   the kubelet's target, and recovers them when they crash.
//!
//! No CSI call clones, fetches or pushes: the history service does all
//! network work in the background (design §14).

/// The CSI driver name (CSIDriver object, StorageClass provisioner).
pub const DRIVER_NAME: &str = "agentfs.mcurcio.github.io";

pub mod csi;
pub mod error;
pub mod layout;
pub mod params;
pub mod records;

pub use error::Error;
