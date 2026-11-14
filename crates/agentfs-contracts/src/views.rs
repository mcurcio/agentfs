//! The Views domain's contract (docs/target-architecture/domains/views.md), M1's
//! surface (slice S4): open a view for a binding, close it.

use std::path::PathBuf;

use crate::clients::Binding;

/// Where a view is mounted: an existing empty directory outside the state area
/// and every other view.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MountPoint(pub PathBuf);

/// How the kernel mount is made. The owner every file is presented as is the
/// binding's grant's `acts_as`, never an option.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MountOptions {
    /// FUSE `allow_other`: other users reach the mount (needs
    /// `user_allow_other` in `/etc/fuse.conf` when not root), as the recording
    /// mount. On by default; `--no-allow-other` turns it off.
    pub allow_other: bool,
}

/// A mounted view, unique per view host run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ViewId(pub u64);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ViewError {
    /// The mount point is missing, not empty, inside the state area or another
    /// view; or the kernel refused the mount (errno).
    #[error("the view could not be mounted (errno {0})")]
    MountFailed(i32),
    /// The binding's session is unknown to the history service.
    #[error("no such session")]
    UnknownSession,
}

/// The Views domain (views.md §5, M1's surface).
pub trait Views {
    /// # Errors
    ///
    /// `MountFailed`, `UnknownSession`.
    fn open(
        &mut self,
        at: MountPoint,
        binding: Binding,
        options: MountOptions,
    ) -> Result<ViewId, ViewError>;

    /// Unmounts; its open files close as the kernel releases them. Idempotent.
    fn close(&mut self, view: ViewId);
}
