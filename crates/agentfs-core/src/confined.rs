//! Confined access to a directory tree, for restores (design §6
//! "Confinement", §15).
//!
//! A restore reads and writes a tree the workload may change at any
//! moment: the backing tree when no mount runs, or the mount point itself.
//! Every path is resolved from a directory descriptor with `openat2` and
//! `RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS | RESOLVE_NO_MAGICLINKS`, so no
//! path can leave the tree or follow a symlink, and every change is made
//! relative to an opened parent directory, by a single name. A symlink in
//! any ancestor fails with `ELOOP`; a symlink as the last component is an
//! object, never a direction. Needs Linux 5.6 or later.
//!
//! This duplicates the small part of `agentfs_fuse::backing` a restore
//! needs; merging the two is a follow-up.

// `stat` field types differ between architectures, so conversions written
// to be portable are identities on some targets.
#![allow(clippy::useless_conversion, clippy::unnecessary_cast)]

use std::ffi::OsStr;
use std::io::{self, Write as _};
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use rustix::fs::{AtFlags, Mode, OFlags, ResolveFlags, Stat};

use crate::paths::RepoPath;

const RESOLVE: ResolveFlags = ResolveFlags::BENEATH
    .union(ResolveFlags::NO_SYMLINKS)
    .union(ResolveFlags::NO_MAGICLINKS);

const S_IFMT: u32 = 0o170000;
const S_IFREG: u32 = 0o100000;
const S_IFDIR: u32 = 0o040000;
const S_IFLNK: u32 = 0o120000;

/// What is at a path in the tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Current {
    Absent,
    File(FileState),
    Symlink {
        target: Vec<u8>,
    },
    Dir,
    /// A FIFO, socket or device.
    Other,
}

/// A regular file as a restore sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileState {
    /// Its content's git blob id.
    pub oid: git2::Oid,
    /// Any execute bit is set (git's executable bit, design §6).
    pub executable: bool,
    /// Permission bits (`mode & 0o7777`).
    pub mode: u32,
    pub nlink: u64,
    pub uid: u32,
    pub gid: u32,
}

/// The kind of an inode, without reading it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Absent,
    File,
    Symlink,
    Dir,
    Other,
}

/// A directory tree reached only through `openat2` (see the module docs).
#[derive(Debug)]
pub struct Dir {
    root: OwnedFd,
    path: PathBuf,
}

/// `uid` and `gid`.
pub type Owner = (u32, u32);

impl Dir {
    /// Opens the tree at `path`. Fails clearly on a kernel without
    /// `openat2` (before Linux 5.6).
    pub fn open(path: &Path) -> io::Result<Self> {
        let root = rustix::fs::open(
            path,
            OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        let dir = Self {
            root,
            path: path.to_path_buf(),
        };
        match dir.open_at("", OFlags::PATH, Mode::empty()) {
            Err(e) if e.raw_os_error() == Some(rustix::io::Errno::NOSYS.raw_os_error()) => {
                Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "the kernel has no openat2 (agentfs needs Linux 5.6 or later)",
                ))
            }
            Err(e) => Err(e),
            Ok(_) => Ok(dir),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Opens `rel` (empty for the root) with `flags`, never following a
    /// symlink. A final symlink is only opened by `O_PATH`.
    pub fn open_at(&self, rel: &str, flags: OFlags, mode: Mode) -> io::Result<OwnedFd> {
        let rel = if rel.is_empty() { "." } else { rel };
        let flags = flags | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        loop {
            match rustix::fs::openat2(&self.root, rel, flags, mode, RESOLVE) {
                // The kernel asks for a retry when a rename raced the walk.
                Err(rustix::io::Errno::AGAIN) => continue,
                other => return other.map_err(Into::into),
            }
        }
    }

    /// The `O_PATH` descriptor and status of `rel`, or `None` if nothing
    /// is there. An ancestor that is a file is `ENOTDIR`; one that is a
    /// symlink is `ELOOP`.
    pub fn stat_at(&self, rel: &str) -> io::Result<Option<(OwnedFd, Stat)>> {
        match self.open_at(rel, OFlags::PATH, Mode::empty()) {
            Ok(fd) => {
                let st = rustix::fs::fstat(&fd)?;
                Ok(Some((fd, st)))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// The kind of inode at `rel`.
    pub fn kind_at(&self, rel: &str) -> io::Result<Kind> {
        Ok(match self.stat_at(rel)? {
            None => Kind::Absent,
            Some((_, st)) => kind_of(&st),
        })
    }

    /// What is at `path`, with a regular file's content hashed as a git
    /// blob.
    pub fn current_entry(&self, path: &RepoPath) -> io::Result<Current> {
        let Some((fd, st)) = self.stat_at(path.as_str())? else {
            return Ok(Current::Absent);
        };
        Ok(match kind_of(&st) {
            Kind::Absent => Current::Absent,
            Kind::Dir => Current::Dir,
            Kind::Other => Current::Other,
            Kind::Symlink => Current::Symlink {
                target: read_link_fd(&fd)?,
            },
            Kind::File => {
                let mode = u32::from(st.st_mode);
                let oid = git2::Oid::hash_file(git2::ObjectType::Blob, proc_path(&fd))
                    .map_err(io::Error::other)?;
                Current::File(FileState {
                    oid,
                    executable: mode & 0o111 != 0,
                    mode: mode & 0o7777,
                    nlink: st.st_nlink as u64,
                    uid: st.st_uid,
                    gid: st.st_gid,
                })
            }
        })
    }

    /// Every regular file, symlink and other non-directory under the
    /// directory `rel`, as paths relative to the root.
    pub fn list_tree(&self, rel: &str) -> io::Result<Vec<String>> {
        let mut out = Vec::new();
        let mut stack = vec![rel.to_owned()];
        while let Some(dir) = stack.pop() {
            let fd = self.open_at(&dir, OFlags::RDONLY | OFlags::DIRECTORY, Mode::empty())?;
            for entry in rustix::fs::Dir::read_from(&fd)? {
                let entry = entry?;
                let name = entry.file_name().to_bytes();
                if name == b"." || name == b".." {
                    continue;
                }
                let name = String::from_utf8_lossy(name);
                let child = if dir.is_empty() {
                    name.into_owned()
                } else {
                    format!("{dir}/{name}")
                };
                if self.kind_at(&child)? == Kind::Dir {
                    stack.push(child);
                } else {
                    out.push(child);
                }
            }
        }
        out.sort();
        Ok(out)
    }

    /// The parent directory of `rel` (opened `O_PATH`) and its last name.
    fn parent(&self, rel: &str) -> io::Result<(OwnedFd, String)> {
        let (parent, name) = match rel.rsplit_once('/') {
            Some((parent, name)) => (parent, name),
            None => ("", rel),
        };
        let fd = self.open_at(parent, OFlags::PATH | OFlags::DIRECTORY, Mode::empty())?;
        Ok((fd, name.to_owned()))
    }

    /// The owner of `rel`.
    pub fn owner_of(&self, rel: &str) -> io::Result<Owner> {
        let fd = self.open_at(rel, OFlags::PATH, Mode::empty())?;
        let st = rustix::fs::fstat(&fd)?;
        Ok((st.st_uid, st.st_gid))
    }

    /// Replaces the content of the regular file `rel` in place (keeping its
    /// inode, owner, mode and extended attributes).
    pub fn write_in_place(&self, rel: &str, content: &[u8]) -> io::Result<()> {
        let fd = self.open_at(rel, OFlags::WRONLY | OFlags::TRUNC, Mode::empty())?;
        let st = rustix::fs::fstat(&fd)?;
        if kind_of(&st) != Kind::File {
            return Err(io::Error::other(format!("{rel} is not a regular file")));
        }
        let mut file = std::fs::File::from(fd);
        file.write_all(content)?;
        // Closing is the change boundary a mount records.
        drop(file);
        Ok(())
    }

    /// Creates the regular file `rel`, which must not exist, with `content`
    /// and exactly `mode`, given to `owner` if set.
    pub fn create_file(
        &self,
        rel: &str,
        content: &[u8],
        mode: u32,
        owner: Option<Owner>,
    ) -> io::Result<()> {
        let (dir, name) = self.parent(rel)?;
        let fd = rustix::fs::openat(
            &dir,
            name.as_str(),
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(mode),
        )?;
        // The process umask does not apply; neither does a default ACL.
        rustix::fs::fchmod(&fd, Mode::from_raw_mode(mode))?;
        if let Some((uid, gid)) = owner {
            chown_fd(&fd, uid, gid)?;
        }
        let mut file = std::fs::File::from(fd);
        file.write_all(content)?;
        drop(file);
        Ok(())
    }

    /// Creates the symlink `rel` to `target`, given to `owner` if set.
    pub fn create_symlink(&self, rel: &str, target: &[u8], owner: Option<Owner>) -> io::Result<()> {
        let (dir, name) = self.parent(rel)?;
        rustix::fs::symlinkat(OsStr::from_bytes(target), &dir, name.as_str())?;
        if let Some((uid, gid)) = owner {
            rustix::fs::chownat(
                &dir,
                name.as_str(),
                Some(rustix::fs::Uid::from_raw(uid)),
                Some(rustix::fs::Gid::from_raw(gid)),
                AtFlags::SYMLINK_NOFOLLOW,
            )?;
        }
        Ok(())
    }

    /// Creates the directory `rel` with mode `0755`, given to `owner` if
    /// set. An existing directory is left alone.
    pub fn create_dir(&self, rel: &str, owner: Option<Owner>) -> io::Result<()> {
        let (dir, name) = self.parent(rel)?;
        match rustix::fs::mkdirat(&dir, name.as_str(), Mode::from_raw_mode(0o755)) {
            Ok(()) => {}
            Err(rustix::io::Errno::EXIST) if self.kind_at(rel)? == Kind::Dir => return Ok(()),
            Err(e) => return Err(e.into()),
        }
        let fd = self.open_at(rel, OFlags::PATH | OFlags::DIRECTORY, Mode::empty())?;
        chmod_fd(&fd, 0o755)?;
        if let Some((uid, gid)) = owner {
            chown_fd(&fd, uid, gid)?;
        }
        Ok(())
    }

    /// Sets the permission bits of `rel`, which must be a regular file.
    pub fn chmod(&self, rel: &str, mode: u32) -> io::Result<()> {
        let fd = self.open_at(rel, OFlags::PATH, Mode::empty())?;
        if kind_of(&rustix::fs::fstat(&fd)?) != Kind::File {
            return Err(io::Error::other(format!("{rel} is not a regular file")));
        }
        chmod_fd(&fd, mode)
    }

    /// Removes the non-directory `rel`.
    pub fn unlink(&self, rel: &str) -> io::Result<()> {
        let (dir, name) = self.parent(rel)?;
        rustix::fs::unlinkat(&dir, name.as_str(), AtFlags::empty())?;
        Ok(())
    }

    /// Removes the empty directory `rel`. `Ok(false)` if it is not empty.
    pub fn rmdir(&self, rel: &str) -> io::Result<bool> {
        let (dir, name) = self.parent(rel)?;
        match rustix::fs::unlinkat(&dir, name.as_str(), AtFlags::REMOVEDIR) {
            Ok(()) => Ok(true),
            Err(rustix::io::Errno::NOTEMPTY | rustix::io::Errno::EXIST) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }
}

/// The kind of inode `st` describes.
pub fn kind_of(st: &Stat) -> Kind {
    match u32::from(st.st_mode) & S_IFMT {
        S_IFREG => Kind::File,
        S_IFDIR => Kind::Dir,
        S_IFLNK => Kind::Symlink,
        _ => Kind::Other,
    }
}

/// The `/proc/self/fd` path of `fd`, which opens the very inode `fd`
/// refers to.
pub fn proc_path(fd: &impl AsFd) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", fd.as_fd().as_raw_fd()))
}

/// The target of the symlink `fd` (an `O_PATH` descriptor of it).
pub fn read_link_fd(fd: &OwnedFd) -> io::Result<Vec<u8>> {
    Ok(rustix::fs::readlinkat(fd, "", Vec::new())?.into_bytes())
}

/// `chmod` through an `O_PATH` descriptor, by its `/proc/self/fd` link
/// (`fchmod` refuses `O_PATH` descriptors).
fn chmod_fd(fd: &OwnedFd, mode: u32) -> io::Result<()> {
    rustix::fs::chmod(proc_path(fd), Mode::from_raw_mode(mode))?;
    Ok(())
}

/// `chown` of exactly the inode `fd` refers to (`AT_EMPTY_PATH`).
fn chown_fd(fd: &OwnedFd, uid: u32, gid: u32) -> io::Result<()> {
    rustix::fs::chownat(
        fd,
        "",
        Some(rustix::fs::Uid::from_raw(uid)),
        Some(rustix::fs::Gid::from_raw(gid)),
        AtFlags::EMPTY_PATH,
    )?;
    Ok(())
}

/// Whether this process may give files to other users (`CAP_CHOWN` in its
/// effective set).
pub fn can_chown() -> bool {
    const CAP_CHOWN: u64 = 1 << 0;
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("CapEff:"))
                .and_then(|v| u64::from_str_radix(v.trim(), 16).ok())
        })
        .is_some_and(|caps| caps & CAP_CHOWN != 0)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};

    use super::*;

    fn p(s: &str) -> RepoPath {
        RepoPath::new(s).unwrap()
    }

    #[test]
    fn current_entries() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir(root.join("d")).unwrap();
        fs::write(root.join("d/f"), "hello").unwrap();
        fs::set_permissions(root.join("d/f"), fs::Permissions::from_mode(0o750)).unwrap();
        symlink("target", root.join("l")).unwrap();
        let t = Dir::open(root).unwrap();
        match t.current_entry(&p("d/f")).unwrap() {
            Current::File(f) => {
                assert_eq!(
                    f.oid,
                    git2::Oid::hash_object(git2::ObjectType::Blob, b"hello").unwrap()
                );
                assert!(f.executable);
                assert_eq!(f.mode, 0o750);
                assert_eq!(f.nlink, 1);
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            t.current_entry(&p("l")).unwrap(),
            Current::Symlink {
                target: b"target".to_vec()
            }
        );
        assert_eq!(t.current_entry(&p("d")).unwrap(), Current::Dir);
        assert_eq!(t.current_entry(&p("missing")).unwrap(), Current::Absent);
        assert_eq!(t.current_entry(&p("d/missing/x")).unwrap(), Current::Absent);
        // Through a file: ENOTDIR. Through a symlink: refused (ELOOP).
        assert!(t.current_entry(&p("d/f/x")).is_err());
        symlink("d", root.join("ld")).unwrap();
        let e = t.current_entry(&p("ld/f")).unwrap_err();
        assert_eq!(
            e.raw_os_error(),
            Some(rustix::io::Errno::LOOP.raw_os_error())
        );
        assert_eq!(t.list_tree("").unwrap(), ["d/f", "l", "ld"]);
        assert_eq!(t.list_tree("d").unwrap(), ["d/f"]);
    }

    #[test]
    fn writes_stay_beneath_the_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let outside = dir.path().join("outside");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&outside).unwrap();
        symlink(&outside, root.join("escape")).unwrap();
        let t = Dir::open(&root).unwrap();
        assert!(t.create_file("escape/x", b"no", 0o644, None).is_err());
        assert!(t.create_dir("escape/d", None).is_err());
        assert!(t.write_in_place("escape", b"no").is_err());
        assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);

        t.create_dir("a", None).unwrap();
        t.create_dir("a", None).unwrap();
        t.create_file("a/f", b"one", 0o755, None).unwrap();
        let meta = fs::metadata(root.join("a/f")).unwrap();
        assert_eq!(meta.mode() & 0o7777, 0o755, "exact mode, no umask");
        assert!(t.create_file("a/f", b"two", 0o644, None).is_err(), "O_EXCL");
        let ino = meta.ino();
        t.write_in_place("a/f", b"two").unwrap();
        assert_eq!(fs::read(root.join("a/f")).unwrap(), b"two");
        assert_eq!(fs::metadata(root.join("a/f")).unwrap().ino(), ino);
        t.chmod("a/f", 0o600).unwrap();
        assert_eq!(
            fs::metadata(root.join("a/f")).unwrap().mode() & 0o7777,
            0o600
        );
        t.create_symlink("a/l", b"../x", None).unwrap();
        assert_eq!(fs::read_link(root.join("a/l")).unwrap(), Path::new("../x"));
        assert!(t.chmod("a/l", 0o600).is_err(), "never through a symlink");
        assert!(!t.rmdir("a").unwrap(), "not empty");
        t.unlink("a/f").unwrap();
        t.unlink("a/l").unwrap();
        assert!(t.rmdir("a").unwrap());
        assert_eq!(t.kind_at("a").unwrap(), Kind::Absent);
    }

    #[test]
    fn owners_are_set_when_allowed() {
        if !can_chown() {
            eprintln!("skipped: needs CAP_CHOWN");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let t = Dir::open(dir.path()).unwrap();
        t.create_dir("d", Some((10000, 10001))).unwrap();
        t.create_file("d/f", b"x", 0o644, Some((10000, 10001)))
            .unwrap();
        t.create_symlink("d/l", b"f", Some((10000, 10001))).unwrap();
        for rel in ["d", "d/f", "d/l"] {
            assert_eq!(t.owner_of(rel).unwrap(), (10000, 10001), "{rel}");
        }
    }
}
