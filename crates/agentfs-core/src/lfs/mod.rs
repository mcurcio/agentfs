//! Git LFS (design §7): large content stored as LFS objects, with pointer
//! blobs in history that ordinary `git clone` plus `git lfs pull` turns
//! back into content.
//!
//! Neither libgit2 nor gitoxide implements LFS, so agentfs does:
//!
//! - [`pointer`]: the pointer format (git-lfs spec v1, canonical form);
//! - [`cache`]: the local object cache in `<state>/lfs/objects`, in
//!   git-lfs's layout;
//! - [`attributes`]: the block of `filter=lfs` lines agentfs keeps at the
//!   end of the root `.gitattributes` in history, as `git lfs track` would;
//! - [`policy`]: which store a recorded file goes to;
//! - [`endpoint`]: each remote's LFS endpoint and credentials;
//! - [`client`]: the batch API client and the standalone-file transfer;
//! - [`pool`]: the worker pool that moves objects concurrently;
//! - [`referenced`]: which objects commits reference;
//! - [`upload`]: the objects a push must upload first;
//! - [`fetch`]: objects for restores and `agentfs lfs fetch`, from the
//!   cache or a remote;
//! - [`lock`]: the cache lock that keeps a prune from deleting objects in
//!   use.
//!
//! What is stored as LFS is decided when a file is recorded and saved in
//! the journal; the content is hashed and cached when it is committed, and
//! uploaded before the commits that reference it are pushed.

pub mod attributes;
pub mod cache;
pub mod client;
pub mod endpoint;
#[cfg(test)]
pub(crate) mod fake;
pub mod fetch;
pub mod lock;
pub mod pointer;
pub mod policy;
pub mod pool;
pub mod referenced;
pub mod upload;

use std::fmt;

use crate::events::PushFailureKind;

pub use cache::ObjectCache;
pub use client::{Transfer, UploadReport, transfer_for};
pub use endpoint::Direction;
pub use pointer::{LfsOid, Pointer};
pub use policy::{Store, StorePolicy};

/// An LFS problem, classified like a push failure (design §11.3, §13): it
/// fails the push attempt with stage `lfs`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LfsError {
    pub kind: PushFailureKind,
    pub message: String,
    /// The objects an upload needed that are missing from the local cache
    /// (a `local` failure), so the caller can name their paths.
    pub missing: Vec<LfsOid>,
}

impl LfsError {
    pub fn new(kind: PushFailureKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            missing: Vec::new(),
        }
    }

    /// A problem on this side: the cache, the history repository, or a
    /// setting that cannot work.
    pub fn local(message: impl Into<String>) -> Self {
        Self::new(PushFailureKind::Local, message)
    }
}

impl fmt::Display for LfsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for LfsError {}
