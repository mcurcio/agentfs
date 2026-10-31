//! Kernel cache invalidations across a daemon's FUSE sessions (design
//! §17.1; sandbox overlay slice 4).
//!
//! The main mount and the sandbox overlay are two FUSE sessions over one
//! [`crate::ops::Ops`], and node ids are the same in both. Each kernel
//! caches names (1 s) per session, and the pages of files open through it,
//! so a change made through one session is an event every other session's
//! kernel must hear: [`Coherent`], a decorator in front of each session's
//! [`Operations`], reports each change the session served, and the
//! [`Invalidator`] sends it to the other sessions' kernels
//! ([`KernelCache`]).
//!
//! - **Events, not timers.** Every change is sent once, as it happens.
//! - **Never from a request thread.** The kernel may wait on a request of
//!   the session it invalidates to finish an invalidation, and two
//!   sessions' requests invalidating each other would wait on each other,
//!   so a FUSE request thread only queues ([`Invalidator::changed`] never
//!   blocks); one worker thread sends.
//! - **Not to the session the change came through**: its kernel already
//!   knows.
//! - A session attached late ([`Invalidator::attach`]) has nothing cached
//!   from before, so changes queued before it attached are not sent to it.
//!
//! [`Invalidator::invalidate`] sends at once to every session, from a
//! thread that is not a FUSE request's: a restore made outside every
//! session ([`crate::mount::MountHooks`]).

use std::io;
use std::sync::Arc;

use crate::mount::MountKind;
use crate::ops::Cached;
use crate::session::Operations;

/// One session's kernel cache, as the invalidator drops entries from it.
/// Implemented by [`fuser::Notifier`]; tests use a recording fake.
pub trait KernelCache: Send + Sync {
    /// Drops the kernel's cached entry `name` in directory node `parent`.
    /// An entry the kernel does not hold is not an error.
    fn inval_entry(&self, parent: u64, name: &[u8]) -> io::Result<()>;
    /// Drops node `id`'s cached attributes and pages. A node the kernel
    /// does not hold is not an error.
    fn inval_inode(&self, id: u64) -> io::Result<()>;
}

impl KernelCache for fuser::Notifier {
    fn inval_entry(&self, parent: u64, name: &[u8]) -> io::Result<()> {
        let _ = (parent, name);
        todo!("sandbox overlay slice 4")
    }

    fn inval_inode(&self, id: u64) -> io::Result<()> {
        let _ = id;
        todo!("sandbox overlay slice 4")
    }
}

/// Sends kernel cache invalidations to a daemon's FUSE sessions.
/// `Send + Sync`.
#[expect(
    dead_code,
    reason = "sandbox overlay slice 4 implements the invalidator"
)]
pub struct Invalidator {
    stub: (),
}

impl Invalidator {
    /// An invalidator with no session attached yet, and its worker thread.
    pub fn start() -> io::Result<Arc<Self>> {
        todo!("sandbox overlay slice 4")
    }

    /// Session `kind`'s kernel cache hears every later change made through
    /// another session. A second attach of the same kind replaces the
    /// first.
    pub fn attach(&self, kind: MountKind, cache: Box<dyn KernelCache>) {
        let _ = (kind, cache);
        todo!("sandbox overlay slice 4")
    }

    /// A change served through session `from`: every other attached
    /// session drops `cached`. Queues and returns; never blocks.
    pub fn changed(&self, from: MountKind, cached: Vec<Cached>) {
        let _ = (from, cached);
        todo!("sandbox overlay slice 4")
    }

    /// Every attached session drops `cached` now, sent from the calling
    /// thread (never a FUSE request's); the first error, if any.
    pub fn invalidate(&self, cached: &[Cached]) -> Result<(), String> {
        let _ = cached;
        todo!("sandbox overlay slice 4")
    }

    /// Sends what is queued, then stops the worker. Later calls are no-ops.
    pub fn stop(&self) {
        todo!("sandbox overlay slice 4")
    }
}

/// Where [`Coherent`] reports the changes its session served: the mount
/// wires it to [`Invalidator::changed`] with the session's kind.
pub type ChangeSink = Arc<dyn Fn(Vec<Cached>) + Send + Sync>;

/// A session's [`Operations`] that reports every change it served to a
/// [`ChangeSink`], after the operation succeeded:
///
/// | Operation | Reported |
/// |---|---|
/// | `mknod`, `mkdir`, `symlink`, `link`, `create` | `Entry(parent, name)` |
/// | `unlink`, `rmdir` | `Entry(parent, name)` |
/// | `rename` | `Entry(parent, name)`, `Entry(newparent, newname)` |
/// | `setattr` | `Node(id)` |
/// | `flush` and `release` of a handle that can write, `fallocate`, `copy_file_range` (its destination) | `Node(node)` |
/// | every other operation, and any that failed | nothing |
///
/// Everything else is passed on unchanged. It learns a handle's node and
/// whether it can write from the `open` and `create` results it passes
/// back, and forgets it at `release`.
#[expect(dead_code, reason = "sandbox overlay slice 4 implements the decorator")]
pub struct Coherent<O: Operations> {
    inner: Arc<O>,
    sink: ChangeSink,
}

impl<O: Operations> Coherent<O> {
    pub fn new(inner: Arc<O>, sink: ChangeSink) -> Self {
        Self { inner, sink }
    }
}
