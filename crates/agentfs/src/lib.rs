//! agentfs as a library, under one name: tree volumes whose every change is
//! recorded into git history. It is the one crate a consumer depends on, and
//! no crate of this workspace depends on it.
//!
//! Re-exports only, each module a crate whole: [`engine`] is the tree engine
//! (`agentfs-engine`), [`contracts`] the shared kernel of its domains
//! (`agentfs-contracts`), [`volume`], [`clients`] and [`trees`] the Volume,
//! Clients and Trees domains (`agentfs-volume`, `agentfs-clients`,
//! `agentfs-trees`), and [`rules`] the Rules domain (`agentfs-rules`).
//!
//! See docs/library/agentfs.md for the consumer API and what is not
//! re-exported.
//!
//! A tree volume, made and opened through the engine's state area, which
//! gives it its id; one file written, closed and read back:
//!
//! ```rust
//! use agentfs::contracts::TreeId;
//! use agentfs::contracts::clients::FileOwner;
//! use agentfs::contracts::name::{VolumeName, WriterLabel};
//! use agentfs::contracts::pod::{PodContext, WriterAt};
//! use agentfs::contracts::ports::StateArea;
//! use agentfs::engine::state_area::TreeStateArea;
//! use agentfs::engine::tree_actor::{Attrs, Perm};
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! # let dir = tempfile::tempdir()?;
//! # let root = dir.path();
//! let at = PodContext::default();
//! let mut area = TreeStateArea::at(root);
//! // The engine gives the new volume its id.
//! let (identity, _created) = area.create(&VolumeName::parse("notes")?, None, &at)?;
//! assert_eq!(area.open(&at)?, identity);
//! let volume = area.recording().ok_or("open serves the volume")?;
//! let by = WriterAt { writer: WriterLabel::parse("docs")?, at };
//! let file = Attrs { perm: Perm::from_mode(0o644), owner: FileOwner { uid: 1000, gid: 1000 } };
//! let handle = volume.actor()?.write_open(TreeId::Root, "hello.txt", &by, Some(file))?;
//! volume.actor()?.write(handle, 0, b"hello\n", false)?;
//! volume.close_with(handle, |_| Ok(()))?;
//! assert_eq!(volume.actor()?.read(TreeId::Root, "hello.txt")?, b"hello\n");
//! # Ok(())
//! # }
//! ```

pub use agentfs_clients as clients;
pub use agentfs_contracts as contracts;
pub use agentfs_engine as engine;
pub use agentfs_rules as rules;
pub use agentfs_trees as trees;
pub use agentfs_volume as volume;
