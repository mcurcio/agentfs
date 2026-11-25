//! agentfs as a library, under one name: tree volumes whose every change is
//! recorded into git history. It is the one crate a consumer depends on, and
//! no crate of this workspace depends on it.
//!
//! Re-exports only, each module a crate whole: [`engine`] is the tree engine
//! (`agentfs-engine`), [`contracts`] the shared kernel of its domains
//! (`agentfs-contracts`), [`volume`], [`clients`] and [`trees`] the Volume,
//! Clients and Trees domains (`agentfs-volume`, `agentfs-clients`,
//! `agentfs-trees`), [`rules`] the Rules domain (`agentfs-rules`),
//! [`referee`] the tree views' landing rules (`agentfs-referee`), and
//! [`service`] one volume composed: its domains wired onto one store
//! (`agentfs-service`).
//!
//! See docs/library/agentfs.md for the consumer API and what is not
//! re-exported.
//!
//! A tree volume served through the service: its composition root mints
//! the launcher's capability, `init` makes the volume, `stage` serves it,
//! one client writes a file through its session and reads it back, and a
//! stop drains and releases it:
//!
//! ```rust
//! use agentfs::contracts::TreeId;
//! use agentfs::contracts::caps::{LaunchCap, LaunchScope, Minter};
//! use agentfs::contracts::clients::{Access, FileOwner};
//! use agentfs::contracts::name::{ClientLabel, LauncherId, VolumeName};
//! use agentfs::contracts::pod::PodContext;
//! use agentfs::contracts::volume::{StartPoint, VolumeSpec};
//! use agentfs::engine::operator_settings::OperatorSettings;
//! use agentfs::engine::tree_actor::{Attrs, Perm};
//! use agentfs::service::{Domains, Launcher, Unstaged};
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! # let dir = tempfile::tempdir()?;
//! # let state = dir.path();
//! // The composition root: the one place capabilities are minted.
//! let minter = Minter::new();
//! let cap = LaunchCap::mint(&minter, LauncherId::parse("docs")?, LaunchScope::Root);
//! let launcher = Launcher::new(minter, cap, PodContext::default());
//! let spec = VolumeSpec {
//!     name: VolumeName::parse("notes")?,
//!     start: StartPoint::Empty,
//!     root_owner: None,
//! };
//! agentfs::service::init(state, spec, &launcher, &[])?;
//! let settings = OperatorSettings::read(state)?;
//! let mut domains = Domains::stage(state, launcher, &settings)?;
//! let at = PodContext::default();
//! let binding = domains.bind(TreeId::Root, ClientLabel::parse("writer")?, Access::ReadWrite, at)?;
//! let session = domains.sessions().session(binding.session).ok_or("a bound session")?;
//! let file = Attrs { perm: Perm::from_mode(0o644), owner: FileOwner { uid: 1000, gid: 1000 } };
//! let handle = {
//!     let mut guard = session.lock()?;
//!     let handle = guard.write_open("hello.txt", Some(file))?;
//!     guard.write(handle, 0, b"hello\n", false)?;
//!     handle
//! };
//! session.close(handle)?;
//! assert_eq!(session.lock()?.read("hello.txt")?, b"hello\n");
//! assert_eq!(domains.unstage(&[binding.client]), Unstaged::Draining);
//! assert!(domains.released());
//! # Ok(())
//! # }
//! ```

pub use agentfs_clients as clients;
pub use agentfs_contracts as contracts;
pub use agentfs_engine as engine;
pub use agentfs_referee as referee;
pub use agentfs_rules as rules;
pub use agentfs_service as service;
pub use agentfs_trees as trees;
pub use agentfs_volume as volume;
