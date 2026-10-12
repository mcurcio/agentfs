//! Access to the backing tree (design §2.1).
//!
//! The tree is reached only through a directory file descriptor opened when
//! the mount starts. Every path is resolved with `openat2` and
//! `RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS | RESOLVE_NO_MAGICLINKS`, so no
//! path, whatever the workload did to the tree, can leave it or follow a
//! symlink: symlinks are objects in the tree, never directions. Mutations
//! are made relative to an opened parent directory, by a single name. This
//! needs Linux 5.6 or later.
//!
//! Content is handed to the recorder as `/proc/self/fd/<n>` of a
//! descriptor opened here, so a freeze reads exactly that inode, never a
//! path that may have been renamed or replaced meanwhile.

use std::ffi::OsStr;
use std::io;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fuser::{FileAttr, FileType, INodeNo};
use rustix::fs::{Mode, OFlags, ResolveFlags, Stat};

/// How every path is resolved.
const RESOLVE: ResolveFlags = ResolveFlags::BENEATH
    .union(ResolveFlags::NO_SYMLINKS)
    .union(ResolveFlags::NO_MAGICLINKS);

/// The backing tree's root.
#[derive(Debug)]
pub struct Backing {
    root: OwnedFd,
    path: PathBuf,
}

impl Backing {
    /// Opens the backing tree at `path`. Fails with a clear message on a
    /// kernel without `openat2` (before Linux 5.6).
    pub fn open(path: &Path) -> io::Result<Self> {
        let root = rustix::fs::open(
            path,
            OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        let backing = Self {
            root,
            path: path.to_path_buf(),
        };
        match backing.open_path(b"") {
            Err(e) if e.raw_os_error() == Some(rustix::io::Errno::NOSYS.raw_os_error()) => {
                Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "the kernel has no openat2 (agentfs mount needs Linux 5.6 or later)",
                ))
            }
            Err(e) => Err(e),
            Ok(_) => Ok(backing),
        }
    }

    /// Where the backing tree is.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The root directory's descriptor (`O_PATH`).
    pub fn root(&self) -> &OwnedFd {
        &self.root
    }

    /// Opens `rel` (relative to the root; empty for the root itself) with
    /// `flags`, never following a symlink in any component. A final
    /// symlink is only opened by `O_PATH`.
    pub fn open_at(&self, rel: &[u8], flags: OFlags, mode: Mode) -> io::Result<OwnedFd> {
        let rel: &[u8] = if rel.is_empty() { b"." } else { rel };
        let flags = flags | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        loop {
            match rustix::fs::openat2(&self.root, OsStr::from_bytes(rel), flags, mode, RESOLVE) {
                // The kernel asks for a retry when a rename raced the walk.
                Err(rustix::io::Errno::AGAIN) => continue,
                other => return other.map_err(Into::into),
            }
        }
    }

    /// Opens `rel` as an `O_PATH` descriptor (any kind of inode).
    pub fn open_path(&self, rel: &[u8]) -> io::Result<OwnedFd> {
        self.open_at(rel, OFlags::PATH, Mode::empty())
    }

    /// Opens the directory `rel` as an `O_PATH` descriptor.
    pub fn open_dir(&self, rel: &[u8]) -> io::Result<OwnedFd> {
        self.open_at(rel, OFlags::PATH | OFlags::DIRECTORY, Mode::empty())
    }
}

/// The `/proc/self/fd` path of `fd`, which opens the very inode `fd`
/// refers to.
pub fn proc_path(fd: &impl AsFd) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", fd.as_fd().as_raw_fd()))
}

/// `parent/name`, or `name` under the root.
pub fn join(parent: &[u8], name: &[u8]) -> Vec<u8> {
    let mut path = parent.to_vec();
    if !path.is_empty() {
        path.push(b'/');
    }
    path.extend_from_slice(name);
    path
}

/// The node's kind as fuser names it.
pub fn file_type(mode: u32) -> FileType {
    match mode & 0o170000 {
        0o040000 => FileType::Directory,
        0o120000 => FileType::Symlink,
        0o010000 => FileType::NamedPipe,
        0o020000 => FileType::CharDevice,
        0o060000 => FileType::BlockDevice,
        0o140000 => FileType::Socket,
        _ => FileType::RegularFile,
    }
}

/// `d_type` from a directory entry's kind.
pub fn file_type_of(kind: rustix::fs::FileType) -> Option<FileType> {
    use rustix::fs::FileType as T;
    Some(match kind {
        T::RegularFile => FileType::RegularFile,
        T::Directory => FileType::Directory,
        T::Symlink => FileType::Symlink,
        T::Fifo => FileType::NamedPipe,
        T::Socket => FileType::Socket,
        T::CharacterDevice => FileType::CharDevice,
        T::BlockDevice => FileType::BlockDevice,
        T::Unknown => return None,
    })
}

fn system_time(secs: i64, nsecs: u64) -> SystemTime {
    let nsecs = u32::try_from(nsecs.min(999_999_999)).unwrap_or(0);
    if secs >= 0 {
        UNIX_EPOCH + Duration::new(secs.unsigned_abs(), nsecs)
    } else {
        UNIX_EPOCH - Duration::new(secs.unsigned_abs(), 0) + Duration::new(0, nsecs)
    }
}

/// The attributes the kernel sees for node `id`: the backing inode's,
/// with the node id as its inode number.
pub fn attr(id: u64, st: &Stat) -> FileAttr {
    let mode = u32::from(st.st_mode);
    FileAttr {
        ino: INodeNo(id),
        size: u64::try_from(i64::from(st.st_size)).unwrap_or(0),
        blocks: u64::try_from(i64::from(st.st_blocks)).unwrap_or(0),
        atime: system_time(i64::from(st.st_atime), u64::from(st.st_atime_nsec)),
        mtime: system_time(i64::from(st.st_mtime), u64::from(st.st_mtime_nsec)),
        ctime: system_time(i64::from(st.st_ctime), u64::from(st.st_ctime_nsec)),
        crtime: UNIX_EPOCH,
        kind: file_type(mode),
        perm: (mode & 0o7777) as u16,
        nlink: u32::try_from(u64::from(st.st_nlink)).unwrap_or(u32::MAX),
        uid: st.st_uid,
        gid: st.st_gid,
        // FUSE carries the kernel's 32-bit device encoding, which is the
        // low half of userspace's dev_t for ordinary major numbers.
        rdev: (u64::from(st.st_rdev) & 0xffff_ffff) as u32,
        blksize: u32::try_from(i64::from(st.st_blksize)).unwrap_or(4096),
        flags: 0,
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn opens_beneath_the_root_only() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        fs::create_dir_all(root.join("d")).unwrap();
        fs::write(root.join("d/f"), "x").unwrap();
        fs::write(dir.path().join("outside"), "secret").unwrap();
        std::os::unix::fs::symlink("../outside", root.join("escape")).unwrap();
        std::os::unix::fs::symlink(dir.path(), root.join("dirlink")).unwrap();
        let b = Backing::open(&root).unwrap();

        b.open_at(b"d/f", OFlags::RDONLY, Mode::empty()).unwrap();
        b.open_path(b"").unwrap();
        assert!(
            b.open_at(b"../outside", OFlags::RDONLY, Mode::empty())
                .is_err()
        );
        assert!(b.open_at(b"escape", OFlags::RDONLY, Mode::empty()).is_err());
        assert!(
            b.open_at(b"dirlink/outside", OFlags::RDONLY, Mode::empty())
                .is_err()
        );
        // The symlink itself can be opened by O_PATH, never followed.
        let link = b.open_path(b"escape").unwrap();
        let st = rustix::fs::fstat(&link).unwrap();
        assert_eq!(u32::from(st.st_mode) & 0o170000, 0o120000);
    }

    #[test]
    fn proc_paths_reopen_the_inode() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("f"), "content").unwrap();
        let b = Backing::open(dir.path()).unwrap();
        let fd = b.open_path(b"f").unwrap();
        fs::rename(dir.path().join("f"), dir.path().join("g")).unwrap();
        assert_eq!(fs::read_to_string(proc_path(&fd)).unwrap(), "content");
    }

    #[test]
    fn helpers() {
        assert_eq!(join(b"", b"a"), b"a");
        assert_eq!(join(b"a", b"b"), b"a/b");
        assert_eq!(file_type(0o100644), FileType::RegularFile);
        assert_eq!(file_type(0o040755), FileType::Directory);
        assert_eq!(file_type(0o120777), FileType::Symlink);
        assert_eq!(file_type(0o010644), FileType::NamedPipe);
        assert_eq!(system_time(1, 5), UNIX_EPOCH + Duration::new(1, 5));
        assert!(system_time(-10, 0) < UNIX_EPOCH);
    }

    #[test]
    fn attributes_carry_the_node_id() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("f"), "abc").unwrap();
        let st = rustix::fs::stat(dir.path().join("f")).unwrap();
        let a = attr(42, &st);
        assert_eq!(a.ino, INodeNo(42));
        assert_eq!(a.size, 3);
        assert_eq!(a.kind, FileType::RegularFile);
        assert_eq!(u32::from(a.perm), u32::from(st.st_mode) & 0o7777);
    }
}
