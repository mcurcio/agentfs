//! The sandbox sessions of one daemon (design §2.4; provenance slice 2).
//!
//! A **sandbox session** is one container's mount of the volume: the
//! sandbox overlay ([`crate::overlay::Overlay`]) with the view owner, the
//! visibility gate and a tmp layer of its own, in front of the daemon's one
//! [`crate::ops::Ops`], mounted at `<root>/<id>`. It opens when the
//! container runtime mounts the volume for a container (Docker's
//! `VolumeDriver.Mount`) and closes when it unmounts it
//! (`VolumeDriver.Unmount`); its id is the runtime's mount ID. Every
//! request it serves is stamped `sandbox/<id>`
//! ([`agentfs_core::record::EntryPoint::Sandbox`]).
//!
//! - **Idempotent.** Opening an open id answers its mount point again;
//!   closing an id with no session succeeds.
//! - **One ends alone.** A session that ends, closed or unmounted by
//!   something else, ends only itself: it leaves the registry, the
//!   invalidator and its tmp layer. The main mount ending stops every
//!   session ([`crate::mount::Mount::shutdown`]).
//! - **Bounded.** At most [`SandboxConfig::max_sessions`] are open; an
//!   open past it is refused, and the container start fails visibly.
//! - **Memory only.** The id-to-session map is never persisted: after a
//!   daemon restart a stale session's mount is detached, not adopted.

use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::time::Duration;

use agentfs_core::record::SandboxId;

use crate::overlay::ViewOwner;

/// What every sandbox session of a daemon shares
/// (`--sandbox-root`, `--sandbox-owner`, `--sandbox-max-sessions`).
#[derive(Debug, Clone)]
pub struct SandboxConfig {
    /// Where each session is mounted, as `<root>/<id>`: a directory the
    /// daemon owns, on a mount with shared propagation so the container
    /// runtime sees the sessions mounted there
    /// (`preflight_sandbox_root`).
    pub root: PathBuf,
    /// The one user every operation through a session is checked and done
    /// as ([`ViewOwner`]).
    pub owner: ViewOwner,
    /// Most sessions open at once.
    pub max_sessions: NonZeroUsize,
    /// The parent of the sessions' tmp layers, one directory per id
    /// (`StateDir::sandbox_tmp_layer`), on the backing tree's filesystem.
    pub tmp_layers: PathBuf,
}

/// The open sandbox sessions of one mount, over its one `Ops`. Built by
/// [`crate::mount::Mount::start`]; shared with the container runtime's
/// plugin endpoint. Opens and closes of one id are serialized; different
/// ids open and close concurrently.
pub struct Sandboxes {
    _private: (),
}

impl Sandboxes {
    /// Opens session `id` and returns its mount point, `<root>/<id>`, once
    /// the kernel has finished INIT; or returns that mount point at once
    /// when `id` is open already. The session gets its own tmp layer
    /// (created empty), its own passthrough status, and is attached to the
    /// invalidator before this returns.
    ///
    /// Errors: [`OpenError::Full`] at the session cap, [`OpenError::Stopping`]
    /// once the main mount is stopping, [`OpenError::Mount`] when creating
    /// the mount point, the tmp layer or the FUSE session fails (nothing is
    /// left behind).
    pub fn open(&self, id: &SandboxId) -> Result<PathBuf, OpenError> {
        let _ = id;
        todo!("provenance slice 2")
    }

    /// Closes session `id`: unmounts it (lazily if busy), waits up to the
    /// mount's unmount grace for its FUSE session to end (an event: the
    /// session's own end, never a poll), then detaches it from the
    /// invalidator, wipes its tmp layer and removes its mount point. Every
    /// change it served was journaled before its call returned. An id with
    /// no open session is closed already: `Ok` with
    /// [`Closed::was_open`] false.
    ///
    /// Errors: [`CloseError::Unmount`] (nothing changed), or
    /// [`CloseError::StillServing`] when the grace ran out (the session
    /// stays registered and is closed again at stop).
    pub fn close(&self, id: &SandboxId) -> Result<Closed, CloseError> {
        let _ = id;
        todo!("provenance slice 2")
    }

    /// The ids of the open sessions, sorted.
    pub fn open_ids(&self) -> Vec<SandboxId> {
        todo!("provenance slice 2")
    }
}

/// Why a sandbox session could not be opened.
#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    #[error("{open} sandbox sessions are open, the most --sandbox-max-sessions allows")]
    Full { open: usize },
    #[error("the volume is being unmounted: no new sandbox session")]
    Stopping,
    #[error(transparent)]
    Mount(Box<crate::mount::MountError>),
}

/// Why a sandbox session could not be closed.
#[derive(Debug, thiserror::Error)]
pub enum CloseError {
    #[error("unmounting the sandbox session at {}: {source}", path.display())]
    Unmount {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("the sandbox session at {} was unmounted but still served after {grace:?}", path.display())]
    StillServing { path: PathBuf, grace: Duration },
}

/// What closing a sandbox session did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Closed {
    /// A session with that id was open.
    pub was_open: bool,
    /// It was busy and unmounted lazily.
    pub lazy: bool,
    /// Wiping its tmp layer failed: the message (the next start wipes
    /// every tmp layer).
    pub tmp_wipe_error: Option<String>,
}
