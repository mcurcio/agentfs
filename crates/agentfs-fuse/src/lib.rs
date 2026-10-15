//! The agentfs FUSE layer (design §2.1, §3.1, §6, §17.1).
//!
//! A passthrough filesystem over a real backing directory that records
//! every change boundary through [`agentfs_core::volume::Recorder`] before
//! the triggering call returns.
//!
//! - [`ops::Ops`] is every operation, independent of the kernel, so its
//!   behaviour is unit-tested against a real directory and state area.
//! - [`fuse`] maps fuser's [`fuser::Filesystem`] onto it and negotiates
//!   kernel passthrough.
//! - [`groups`] holds writes to fsync-class databases while they are
//!   captured and schedules the captures (design §8).
//! - [`sweep`] runs the periodic backstop and retries failed records, and
//!   the fsync-class trailing timer.
//! - [`mount`] checks the layout (nothing of the state area may be
//!   reachable through the mount), mounts, and unmounts.

// `stat` field types differ between architectures (`c_ulong` on x86_64,
// `u32` or `i64` elsewhere), so conversions written to be portable are
// identities on some targets.
#![allow(clippy::useless_conversion)]

pub mod backing;
pub mod boundary;
pub mod creds;
pub mod dirty;
pub mod fuse;
pub mod groups;
pub mod inodes;
pub mod mount;
pub mod names;
pub mod ops;
pub mod privileges;
pub mod sweep;
pub mod views;

pub use fuse::{Passthrough, PassthroughStatus};
pub use mount::{
    Mount, MountConfig, MountError, MountHooks, MountState, MountView, PreflightError, StackOver,
    StartupRepairs, UnmountReport, agentfs_layers, detach, detach_stale_layers, mount_state,
    preflight, preflight_stacked,
};
