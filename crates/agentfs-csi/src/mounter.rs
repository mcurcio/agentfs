//! The mount operations of the node plugin (design §14.2): what is
//! mounted at a path, bind mounts of a front's own mount onto the
//! kubelet's target, and lazy detaches. A trait so tests can keep an
//! in-memory mount table.

use std::io;
use std::path::Path;

use agentfs_fuse::MountState;
use rustix::mount::{MountFlags, MountPropagationFlags, UnmountFlags};

/// Most layers [`Mounter::detach_all`] removes: each crash of a front
/// leaves one stale layer under the bind stacked over it.
pub const MAX_LAYERS: usize = 64;

pub trait Mounter: Send + Sync + 'static {
    /// The topmost mount at `path`.
    fn state(&self, path: &Path) -> io::Result<MountState>;
    /// Bind-mounts `source` onto `target`, on top of whatever is there
    /// (read-only when asked).
    fn bind(&self, source: &Path, target: &Path, readonly: bool) -> io::Result<()>;
    /// Lazily detaches the topmost mount at `path`.
    fn detach(&self, path: &Path) -> io::Result<()>;
    /// Detaches every mount at `path`; returns how many.
    fn detach_all(&self, path: &Path) -> io::Result<usize> {
        let mut n = 0;
        while n < MAX_LAYERS {
            if self.state(path)? == MountState::NotMounted {
                return Ok(n);
            }
            self.detach(path)?;
            n += 1;
        }
        Err(io::Error::other(format!(
            "{} still has mounts after {MAX_LAYERS} detaches",
            path.display()
        )))
    }
}

/// The kernel's mount table.
#[derive(Debug, Clone, Copy, Default)]
pub struct KernelMounter;

impl Mounter for KernelMounter {
    fn state(&self, path: &Path) -> io::Result<MountState> {
        agentfs_fuse::mount_state(path)
    }

    fn bind(&self, source: &Path, target: &Path, readonly: bool) -> io::Result<()> {
        rustix::mount::mount_bind(source, target)?;
        if readonly {
            let flags =
                MountFlags::BIND | MountFlags::RDONLY | MountFlags::NOSUID | MountFlags::NODEV;
            if let Err(e) = rustix::mount::mount_remount(target, flags, "") {
                let _ = self.detach(target);
                return Err(e.into());
            }
        }
        // The target is in the kubelet's shared peer group; make sure the
        // new layer propagates to the pod's HostToContainer mounts.
        let _ = rustix::mount::mount_change(target, MountPropagationFlags::SHARED);
        Ok(())
    }

    fn detach(&self, path: &Path) -> io::Result<()> {
        rustix::mount::unmount(path, UnmountFlags::DETACH).map_err(io::Error::from)
    }
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
        fn bind(&self, _: &Path, _: &Path, _: bool) -> io::Result<()> {
            unimplemented!()
        }
        fn detach(&self, path: &Path) -> io::Result<()> {
            self.0.lock().unwrap().get_mut(path).and_then(Vec::pop);
            Ok(())
        }
    }

    #[test]
    fn detach_all_pops_every_layer_and_gives_up_at_the_cap() {
        let t = Table::default();
        let stale = MountState::Stale {
            source: "agentfs:v".into(),
        };
        t.0.lock()
            .unwrap()
            .insert("/t".into(), vec![stale.clone(), stale.clone(), stale]);
        assert_eq!(t.detach_all(Path::new("/t")).unwrap(), 3);
        assert_eq!(t.detach_all(Path::new("/t")).unwrap(), 0);

        struct Stuck;
        impl Mounter for Stuck {
            fn state(&self, _: &Path) -> io::Result<MountState> {
                Ok(MountState::Foreign { fstype: "x".into() })
            }
            fn bind(&self, _: &Path, _: &Path, _: bool) -> io::Result<()> {
                Ok(())
            }
            fn detach(&self, _: &Path) -> io::Result<()> {
                Ok(())
            }
        }
        assert!(Stuck.detach_all(Path::new("/t")).is_err());
    }
}
