//! The mount operations of the node plugin: what is mounted at a path, and
//! lazy detaches of a view a host no longer serves. A trait so tests can
//! keep an in-memory mount table.

use std::io;
use std::path::Path;

use agentfs_fuse::MountState;
use rustix::mount::UnmountFlags;

/// Most layers [`detach_agentfs`] removes: each crash of a host leaves one
/// stale layer under the view the restarted host stacks over it.
pub const MAX_LAYERS: usize = 64;

pub trait Mounter: Send + Sync + 'static {
    /// Lazily detaches the topmost mount at `path` (`MNT_DETACH`); `Ok`
    /// when nothing is mounted there.
    fn detach(&self, path: &Path) -> io::Result<()>;
    /// The topmost mount at `path`.
    fn state(&self, path: &Path) -> io::Result<MountState>;
}

/// The kernel's mount table.
#[derive(Debug, Clone, Copy, Default)]
pub struct KernelMounter;

impl Mounter for KernelMounter {
    fn detach(&self, path: &Path) -> io::Result<()> {
        match rustix::mount::unmount(path, UnmountFlags::DETACH) {
            // Not a mount point.
            Err(rustix::io::Errno::INVAL) => Ok(()),
            other => other.map_err(io::Error::from),
        }
    }

    fn state(&self, path: &Path) -> io::Result<MountState> {
        agentfs_fuse::mount_state(path)
    }
}

/// Detaches every agentfs mount stacked at `path`, live or stale; returns
/// how many. Anything else mounted there is left alone.
///
/// # Errors
///
/// A failed detach or mount table read, or more than [`MAX_LAYERS`] layers.
pub fn detach_agentfs(mounter: &dyn Mounter, path: &Path) -> io::Result<usize> {
    for n in 0..MAX_LAYERS {
        match mounter.state(path)? {
            MountState::Live { .. } | MountState::Stale { .. } => mounter.detach(path)?,
            MountState::NotMounted | MountState::Foreign { .. } => return Ok(n),
        }
    }
    Err(io::Error::other(format!(
        "{} still has agentfs mounts after {MAX_LAYERS} detaches",
        path.display()
    )))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::Mutex;

    use super::*;

    /// A stack of mounts per path.
    #[derive(Default)]
    struct Table(Mutex<HashMap<PathBuf, Vec<MountState>>>);

    impl Mounter for Table {
        fn state(&self, path: &Path) -> io::Result<MountState> {
            Ok(self
                .0
                .lock()
                .unwrap()
                .get(path)
                .and_then(|s| s.last().cloned())
                .unwrap_or(MountState::NotMounted))
        }
        fn detach(&self, path: &Path) -> io::Result<()> {
            self.0.lock().unwrap().get_mut(path).and_then(Vec::pop);
            Ok(())
        }
    }

    #[test]
    fn every_agentfs_layer_goes_and_nothing_else() {
        let t = Table::default();
        let stale = MountState::Stale {
            source: "agentfs:v".into(),
        };
        let foreign = MountState::Foreign {
            fstype: "tmpfs".into(),
        };
        t.0.lock().unwrap().insert(
            "/t".into(),
            vec![foreign.clone(), stale.clone(), stale.clone(), stale],
        );
        assert_eq!(detach_agentfs(&t, Path::new("/t")).unwrap(), 3);
        assert_eq!(t.state(Path::new("/t")).unwrap(), foreign);
        assert_eq!(detach_agentfs(&t, Path::new("/none")).unwrap(), 0);

        struct Stuck;
        impl Mounter for Stuck {
            fn state(&self, _: &Path) -> io::Result<MountState> {
                Ok(MountState::Stale {
                    source: "agentfs:v".into(),
                })
            }
            fn detach(&self, _: &Path) -> io::Result<()> {
                Ok(())
            }
        }
        assert!(detach_agentfs(&Stuck, Path::new("/t")).is_err());
    }
}
