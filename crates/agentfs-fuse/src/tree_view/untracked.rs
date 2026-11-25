//! A view's untracked half (docs/library/filesystem.md, "Tree views"): the
//! tree's files the rules leave out, served from its untracked area
//! (`agentfs_engine::tree_actor::UntrackedArea`) as ordinary Linux files. No
//! call here is journaled, gated by the lag gate or made under the actor's
//! lock; each change holds the area against a fork's copy while it runs.
//!
//! Inodes: an area entry's is `1 << 62 | n`, `n` from this table keyed by
//! the backing file (`st_dev`, `st_ino`), each with the paths it is known
//! by (more than one after a hard link).

// `stat` field types differ between architectures, so conversions written
// to be portable are identities on some targets.
#![allow(clippy::useless_conversion, clippy::unnecessary_cast)]

use std::collections::{BTreeMap, BTreeSet};
use std::os::fd::OwnedFd;
use std::path::Path;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agentfs_engine::tree_actor::{AreaWrite, Meta, UntrackedArea};
use fuser::{Errno, FileAttr, FileType, INodeNo, TimeOrNow};
use rustix::fs::{AtFlags, Mode, OFlags, RenameFlags, Stat, Timespec, Timestamps};

use super::{AttrChanges, Result};

/// The start of the untracked entries' inodes, `[1 << 62, 1 << 63)`. Tracked
/// inodes are outside it: a file's or link's object and a directory's number,
/// plus 2, are below it, and a directory a take adds has bit 63 set
/// (`taken_dir_number`); the view refuses any that is not (`tracked_inode`).
pub(super) const UNTRACKED: u64 = 1 << 62;

/// Whether `ino` is an untracked entry's.
pub(super) fn is_untracked(ino: INodeNo) -> bool {
    ino.0 >> 62 == 1
}

/// An errno from the area's I/O.
pub(super) fn io(e: impl Into<std::io::Error>) -> Errno {
    Errno::from_i32(e.into().raw_os_error().unwrap_or(libc::EIO))
}

/// What a view knows of its area's entries.
#[derive(Default)]
struct Inodes {
    /// `n` by backing file.
    by_file: BTreeMap<(u64, u64), u64>,
    /// Each `n`'s backing file and known paths.
    entries: BTreeMap<u64, ((u64, u64), BTreeSet<String>)>,
    next: u64,
}

/// One view's untracked area and its inodes.
pub(super) struct Untracked {
    area: UntrackedArea,
    inodes: Mutex<Inodes>,
    /// New entries are given to their caller (`CAP_CHOWN` and `CAP_FOWNER`).
    give: bool,
}

/// The kind `mode` names, as FUSE takes it.
fn kind(mode: u32) -> FileType {
    match mode & libc::S_IFMT {
        libc::S_IFDIR => FileType::Directory,
        libc::S_IFLNK => FileType::Symlink,
        libc::S_IFIFO => FileType::NamedPipe,
        libc::S_IFSOCK => FileType::Socket,
        libc::S_IFCHR => FileType::CharDevice,
        libc::S_IFBLK => FileType::BlockDevice,
        _ => FileType::RegularFile,
    }
}

fn time(sec: i64, nsec: i64) -> SystemTime {
    let nsec = u32::try_from(nsec).unwrap_or(0);
    match u64::try_from(sec) {
        Ok(sec) => UNIX_EPOCH + Duration::new(sec, nsec),
        Err(_) => UNIX_EPOCH,
    }
}

/// A utimensat time: `UTIME_OMIT` when not given.
fn timespec(time: Option<TimeOrNow>) -> Result<Timespec> {
    Ok(match time {
        None => Timespec {
            tv_sec: 0,
            tv_nsec: rustix::fs::UTIME_OMIT,
        },
        Some(TimeOrNow::Now) => Timespec {
            tv_sec: 0,
            tv_nsec: rustix::fs::UTIME_NOW,
        },
        Some(TimeOrNow::SpecificTime(at)) => {
            let since = at.duration_since(UNIX_EPOCH).map_err(|_| Errno::EINVAL)?;
            Timespec {
                tv_sec: i64::try_from(since.as_secs()).map_err(|_| Errno::EINVAL)?,
                tv_nsec: since.subsec_nanos().into(),
            }
        }
    })
}

/// `dir`'s entry `name`, as a tree path.
fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_owned()
    } else {
        format!("{dir}/{name}")
    }
}

impl Untracked {
    pub(super) fn new(area: UntrackedArea, give: bool) -> Self {
        Self {
            area,
            inodes: Mutex::default(),
            give,
        }
    }

    /// Holds the area against a fork's copy for one change.
    pub(super) fn writing(&self) -> AreaWrite<'_> {
        self.area.writing()
    }

    fn inodes(&self) -> MutexGuard<'_, Inodes> {
        self.inodes.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The inode of the entry at `path` with status `st`; its path is
    /// remembered.
    fn ino(&self, path: &str, st: &Stat) -> INodeNo {
        let file = file_of(st);
        let mut inodes = self.inodes();
        let n = match inodes.by_file.get(&file) {
            Some(n) => *n,
            None => {
                inodes.next += 1;
                let n = inodes.next;
                inodes.by_file.insert(file, n);
                n
            }
        };
        inodes
            .entries
            .entry(n)
            .or_insert_with(|| (file, BTreeSet::new()))
            .1
            .insert(path.to_owned());
        INodeNo(UNTRACKED | n)
    }

    /// The attributes of the entry at `path` with status `st`.
    fn attr(&self, path: &str, st: &Stat) -> FileAttr {
        attr_of(self.ino(path, st), st)
    }

    /// The last known path of an untracked inode.
    pub(super) fn path(&self, ino: INodeNo) -> Result<String> {
        self.inodes()
            .entries
            .get(&(ino.0 & !UNTRACKED))
            .and_then(|(_, paths)| paths.first().cloned())
            .ok_or(Errno::ENOENT)
    }

    /// The entry at `path`, if the area has one.
    pub(super) fn lookup(&self, path: &str) -> Result<Option<FileAttr>> {
        Ok(self
            .area
            .stat(path)
            .map_err(io)?
            .map(|st| self.attr(path, &st)))
    }

    /// Whether the area has an entry at `path`.
    pub(super) fn exists(&self, path: &str) -> Result<bool> {
        Ok(self.area.stat(path).map_err(io)?.is_some())
    }

    /// Whether a directory above `path`, other than the root, holds `.git`.
    pub(super) fn inside_repository(&self, path: &str) -> Result<bool> {
        let mut end = 0;
        let parts: Vec<&str> = path.split('/').collect();
        for (i, part) in parts.iter().enumerate().take(parts.len().saturating_sub(1)) {
            end += part.len() + usize::from(i > 0);
            if self.exists(&format!("{}/.git", &path[..end]))? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// The attributes of an untracked inode: through its open descriptor
    /// when one is given (a file no name reaches has one), else by its path.
    pub(super) fn getattr(&self, ino: INodeNo, fd: Option<&OwnedFd>) -> Result<FileAttr> {
        if let Some(fd) = fd {
            return Ok(attr_of(ino, &rustix::fs::fstat(fd).map_err(io)?));
        }
        let path = self.path(ino)?;
        let st = self.area.stat(&path).map_err(io)?.ok_or(Errno::ENOENT)?;
        Ok(self.attr(&path, &st))
    }

    /// The names of the area's directory `dir`, sorted, with their kinds.
    pub(super) fn entries(&self, dir: &str) -> Result<Vec<(String, FileType)>> {
        Ok(self
            .area
            .entries(dir)
            .map_err(io)?
            .into_iter()
            .map(|(name, file_type)| (name, kind(file_type.as_raw_mode())))
            .collect())
    }

    /// The inode readdir gives `dir`'s entry `name`.
    pub(super) fn entry_ino(&self, dir: &str, name: &str) -> Result<Option<INodeNo>> {
        let path = join(dir, name);
        Ok(self
            .area
            .stat(&path)
            .map_err(io)?
            .map(|st| self.ino(&path, &st)))
    }

    /// Gives a new entry to its caller, when the daemon may.
    fn give(&self, fd: &OwnedFd, owner: (u32, u32)) -> Result<()> {
        if self.give {
            rustix::fs::fchown(
                fd,
                Some(rustix::fs::Uid::from_raw(owner.0)),
                Some(rustix::fs::Gid::from_raw(owner.1)),
            )
            .map_err(io)?;
        }
        Ok(())
    }

    /// Opens the file at `path` with the caller's access mode, `O_TRUNC`
    /// and the sync flags, never following a link.
    pub(super) fn open(&self, path: &str, flags: i32) -> Result<OwnedFd> {
        let writing = self.area.writing();
        let (parent, name) = writing.parent(path).map_err(io)?;
        let flags = open_flags(flags);
        rustix::fs::openat(&parent, name.as_str(), flags, Mode::empty()).map_err(io)
    }

    /// Creates or opens the file at `path` (`O_EXCL` honoured here and by
    /// the caller against the tracked half), a new one with exactly
    /// `mode`'s permission bits, given to `owner`.
    pub(super) fn create(
        &self,
        path: &str,
        flags: i32,
        mode: u32,
        owner: (u32, u32),
        meta_of: &mut dyn FnMut(&str) -> Option<Meta>,
    ) -> Result<(OwnedFd, FileAttr)> {
        let writing = self.area.writing();
        writing.containers(path, meta_of).map_err(io)?;
        let (parent, name) = writing.parent(path).map_err(io)?;
        let fd = match rustix::fs::openat(
            &parent,
            name.as_str(),
            open_flags(flags) | OFlags::CREATE | OFlags::EXCL,
            Mode::from_raw_mode(mode & 0o7777),
        ) {
            Ok(fd) => {
                // The daemon's umask does not apply.
                rustix::fs::fchmod(&fd, Mode::from_raw_mode(mode & 0o7777)).map_err(io)?;
                self.give(&fd, owner)?;
                fd
            }
            Err(rustix::io::Errno::EXIST) if flags & libc::O_EXCL == 0 => {
                rustix::fs::openat(&parent, name.as_str(), open_flags(flags), Mode::empty())
                    .map_err(io)?
            }
            Err(e) => return Err(io(e)),
        };
        let st = rustix::fs::fstat(&fd).map_err(io)?;
        Ok((fd, self.attr(path, &st)))
    }

    /// A new directory at `path` with exactly `mode`, given to `owner`.
    pub(super) fn mkdir(
        &self,
        path: &str,
        mode: u32,
        owner: (u32, u32),
        meta_of: &mut dyn FnMut(&str) -> Option<Meta>,
    ) -> Result<FileAttr> {
        let writing = self.area.writing();
        writing.containers(path, meta_of).map_err(io)?;
        let (parent, name) = writing.parent(path).map_err(io)?;
        rustix::fs::mkdirat(&parent, name.as_str(), Mode::from_raw_mode(mode & 0o7777))
            .map_err(io)?;
        let fd = rustix::fs::openat(
            &parent,
            name.as_str(),
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(io)?;
        rustix::fs::fchmod(&fd, Mode::from_raw_mode(mode & 0o7777)).map_err(io)?;
        self.give(&fd, owner)?;
        let st = rustix::fs::fstat(&fd).map_err(io)?;
        Ok(self.attr(path, &st))
    }

    /// A new symbolic link at `path` to `target`, given to `owner`.
    pub(super) fn symlink(
        &self,
        path: &str,
        target: &Path,
        owner: (u32, u32),
        meta_of: &mut dyn FnMut(&str) -> Option<Meta>,
    ) -> Result<FileAttr> {
        let writing = self.area.writing();
        writing.containers(path, meta_of).map_err(io)?;
        let (parent, name) = writing.parent(path).map_err(io)?;
        rustix::fs::symlinkat(target.as_os_str(), &parent, name.as_str()).map_err(io)?;
        if self.give {
            rustix::fs::chownat(
                &parent,
                name.as_str(),
                Some(rustix::fs::Uid::from_raw(owner.0)),
                Some(rustix::fs::Gid::from_raw(owner.1)),
                AtFlags::SYMLINK_NOFOLLOW,
            )
            .map_err(io)?;
        }
        let st =
            rustix::fs::statat(&parent, name.as_str(), AtFlags::SYMLINK_NOFOLLOW).map_err(io)?;
        Ok(self.attr(path, &st))
    }

    /// A link's target.
    pub(super) fn readlink(&self, ino: INodeNo) -> Result<Vec<u8>> {
        self.readlink_at(&self.path(ino)?)
    }

    /// The target of the link at `path`.
    pub(super) fn readlink_at(&self, path: &str) -> Result<Vec<u8>> {
        let writing = self.area.writing();
        let (parent, name) = writing.parent(path).map_err(io)?;
        Ok(rustix::fs::readlinkat(&parent, name.as_str(), Vec::new())
            .map_err(io)?
            .into_bytes())
    }

    /// A setattr of an untracked entry: mode, owner, size (through `fd`
    /// when given), then times.
    pub(super) fn setattr(
        &self,
        ino: INodeNo,
        changes: AttrChanges,
        fd: Option<&OwnedFd>,
    ) -> Result<FileAttr> {
        let path = self.path(ino)?;
        let writing = self.area.writing();
        let (parent, name) = writing.parent(&path).map_err(io)?;
        let name = name.as_str();
        {
            if let Some(mode) = changes.mode {
                rustix::fs::chmodat(
                    &parent,
                    name,
                    Mode::from_raw_mode(mode & 0o7777),
                    AtFlags::empty(),
                )
                .map_err(io)?;
            }
            if changes.uid.is_some() || changes.gid.is_some() {
                rustix::fs::chownat(
                    &parent,
                    name,
                    changes.uid.map(rustix::fs::Uid::from_raw),
                    changes.gid.map(rustix::fs::Gid::from_raw),
                    AtFlags::SYMLINK_NOFOLLOW,
                )
                .map_err(io)?;
            }
            if let Some(size) = changes.size {
                match fd {
                    Some(fd) => rustix::fs::ftruncate(fd, size).map_err(io)?,
                    None => {
                        let fd = rustix::fs::openat(
                            &parent,
                            name,
                            OFlags::WRONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                            Mode::empty(),
                        )
                        .map_err(io)?;
                        rustix::fs::ftruncate(&fd, size).map_err(io)?;
                    }
                }
            }
            if changes.atime.is_some() || changes.mtime.is_some() {
                let times = Timestamps {
                    last_access: timespec(changes.atime)?,
                    last_modification: timespec(changes.mtime)?,
                };
                rustix::fs::utimensat(&parent, name, &times, AtFlags::SYMLINK_NOFOLLOW)
                    .map_err(io)?;
            }
        }
        let st = rustix::fs::statat(&parent, name, AtFlags::SYMLINK_NOFOLLOW).map_err(io)?;
        Ok(self.attr(&path, &st))
    }

    /// Removes the non-directory at `path`.
    pub(super) fn unlink(&self, path: &str) -> Result<()> {
        let writing = self.area.writing();
        let (parent, name) = writing.parent(path).map_err(io)?;
        rustix::fs::unlinkat(&parent, name.as_str(), AtFlags::empty()).map_err(io)?;
        self.forget(path);
        Ok(())
    }

    /// Removes the empty directory at `path`; with `if_any`, nothing there
    /// is fine (a tracked directory's container).
    pub(super) fn rmdir(&self, path: &str, if_any: bool) -> Result<()> {
        let writing = self.area.writing();
        let (parent, name) = match writing.parent(path) {
            Ok(found) => found,
            Err(e) if if_any && agentfs_os::confined::is_gone(&e) => return Ok(()),
            Err(e) => return Err(io(e)),
        };
        match rustix::fs::unlinkat(&parent, name.as_str(), AtFlags::REMOVEDIR) {
            Ok(()) => {}
            Err(rustix::io::Errno::NOENT) if if_any => return Ok(()),
            Err(e) => return Err(io(e)),
        }
        self.forget(path);
        Ok(())
    }

    /// Renames `from` to `to` within the area, made under the containers
    /// `to` needs; `RENAME_NOREPLACE` when `no_replace`.
    pub(super) fn rename(
        &self,
        from: &str,
        to: &str,
        no_replace: bool,
        meta_of: &mut dyn FnMut(&str) -> Option<Meta>,
    ) -> Result<()> {
        let writing = self.area.writing();
        writing.containers(to, meta_of).map_err(io)?;
        let (old_parent, old_name) = writing.parent(from).map_err(io)?;
        let (new_parent, new_name) = writing.parent(to).map_err(io)?;
        let flags = if no_replace {
            RenameFlags::NOREPLACE
        } else {
            RenameFlags::empty()
        };
        rustix::fs::renameat_with(
            &old_parent,
            old_name.as_str(),
            &new_parent,
            new_name.as_str(),
            flags,
        )
        .map_err(io)?;
        if from != to {
            self.forget(to);
        }
        self.moved(from, to);
        Ok(())
    }

    /// After a tracked directory's rename: its container, if it has one,
    /// moves with it.
    pub(super) fn follow_dir(
        &self,
        from: &str,
        to: &str,
        meta_of: &mut dyn FnMut(&str) -> Option<Meta>,
    ) -> Result<()> {
        if self.area.stat(from).map_err(io)?.is_none() {
            return Ok(());
        }
        self.rename(from, to, false, meta_of)
    }

    /// Before a tracked directory is renamed onto `to`: an empty container
    /// left there is removed; a container with entries makes the target's
    /// listing non-empty, so `ENOTEMPTY`; any other entry answers `EXDEV`.
    pub(super) fn clear_for_dir(&self, to: &str) -> Result<()> {
        let Some(st) = self.area.stat(to).map_err(io)? else {
            return Ok(());
        };
        if kind(u32::from(st.st_mode)) != FileType::Directory {
            return Err(Errno::EXDEV);
        }
        if !self.area.entries(to).map_err(io)?.is_empty() {
            return Err(Errno::ENOTEMPTY);
        }
        self.rmdir(to, true)
    }

    /// A hard link at `to` to the area's entry at `from`, made under the
    /// containers `to` needs: its attributes, the inode now known by both.
    pub(super) fn link(
        &self,
        from: &str,
        to: &str,
        meta_of: &mut dyn FnMut(&str) -> Option<Meta>,
    ) -> Result<FileAttr> {
        let writing = self.area.writing();
        writing.containers(to, meta_of).map_err(io)?;
        let (old_parent, old_name) = writing.parent(from).map_err(io)?;
        let (new_parent, new_name) = writing.parent(to).map_err(io)?;
        rustix::fs::linkat(
            &old_parent,
            old_name.as_str(),
            &new_parent,
            new_name.as_str(),
            AtFlags::empty(),
        )
        .map_err(io)?;
        let st = rustix::fs::statat(&new_parent, new_name.as_str(), AtFlags::SYMLINK_NOFOLLOW)
            .map_err(io)?;
        Ok(self.attr(to, &st))
    }

    /// A new FIFO or socket at `path` (`mode` names which) with exactly
    /// `mode`'s permission bits, made by the view host and given to `owner`.
    pub(super) fn mknod(
        &self,
        path: &str,
        mode: u32,
        owner: (u32, u32),
        meta_of: &mut dyn FnMut(&str) -> Option<Meta>,
    ) -> Result<FileAttr> {
        let writing = self.area.writing();
        writing.containers(path, meta_of).map_err(io)?;
        let (parent, name) = writing.parent(path).map_err(io)?;
        let name = name.as_str();
        let perm = Mode::from_raw_mode(mode & 0o7777);
        rustix::fs::mknodat(
            &parent,
            name,
            rustix::fs::FileType::from_raw_mode(mode),
            perm,
            0,
        )
        .map_err(io)?;
        // The daemon's umask does not apply. A FIFO or socket has no
        // descriptor to change it through, and no link is followed: the
        // entry was just made.
        rustix::fs::chmodat(&parent, name, perm, AtFlags::empty()).map_err(io)?;
        if self.give {
            rustix::fs::chownat(
                &parent,
                name,
                Some(rustix::fs::Uid::from_raw(owner.0)),
                Some(rustix::fs::Gid::from_raw(owner.1)),
                AtFlags::SYMLINK_NOFOLLOW,
            )
            .map_err(io)?;
        }
        let st = rustix::fs::statat(&parent, name, AtFlags::SYMLINK_NOFOLLOW).map_err(io)?;
        Ok(self.attr(path, &st))
    }

    /// Puts `len` bytes of `src` at `path` with `meta`, under the area's
    /// hold `writing` ([`AreaWrite::place`]): a tracked file or link moved
    /// out of the tree. The containers above `path` must exist.
    pub(super) fn place(
        &self,
        writing: &AreaWrite<'_>,
        path: &str,
        src: &mut dyn std::io::Read,
        len: u64,
        meta: Meta,
    ) -> Result<()> {
        writing.place(path, src, len, meta).map_err(io)?;
        self.forget(path);
        Ok(())
    }

    /// The regular file at `path` opened read-only, never following a link,
    /// and its status: the source of a move into the tree.
    pub(super) fn open_source(&self, path: &str) -> Result<(std::fs::File, Stat)> {
        let fd = self.open(path, libc::O_RDONLY)?;
        let st = rustix::fs::fstat(&fd).map_err(io)?;
        if u32::from(st.st_mode) & libc::S_IFMT != libc::S_IFREG {
            return Err(Errno::EXDEV);
        }
        Ok((std::fs::File::from(fd), st))
    }

    /// Makes the area's directory at `path`, if it has one, durable.
    pub(super) fn fsync_dir(&self, path: &str) -> Result<()> {
        self.area.sync_dir(path).map_err(io)
    }

    /// Paths at or under `from` are now under `to`.
    fn moved(&self, from: &str, to: &str) {
        let prefix = format!("{from}/");
        for (_, paths) in self.inodes().entries.values_mut() {
            *paths = std::mem::take(paths)
                .into_iter()
                .map(|path| {
                    if path == from {
                        to.to_owned()
                    } else if let Some(rest) = path.strip_prefix(&prefix) {
                        format!("{to}/{rest}")
                    } else {
                        path
                    }
                })
                .collect();
        }
    }

    /// The entry at `path` is gone: no inode has that path, and one left
    /// with none is forgotten.
    fn forget(&self, path: &str) {
        let mut inodes = self.inodes();
        let mut gone = Vec::new();
        for (n, (file, paths)) in &mut inodes.entries {
            if paths.remove(path) && paths.is_empty() {
                gone.push((*n, *file));
            }
        }
        for (n, file) in gone {
            inodes.entries.remove(&n);
            inodes.by_file.remove(&file);
        }
    }
}

/// A backing file's identity: its device and inode numbers.
pub(super) fn file_of(st: &Stat) -> (u64, u64) {
    (st.st_dev as u64, st.st_ino as u64)
}

/// The attributes of inode `ino`, a file with status `st`.
fn attr_of(ino: INodeNo, st: &Stat) -> FileAttr {
    let mode = u32::from(st.st_mode);
    FileAttr {
        ino,
        size: st.st_size as u64,
        blocks: st.st_blocks as u64,
        atime: time(st.st_atime as i64, st.st_atime_nsec as i64),
        mtime: time(st.st_mtime as i64, st.st_mtime_nsec as i64),
        ctime: time(st.st_ctime as i64, st.st_ctime_nsec as i64),
        crtime: time(st.st_ctime as i64, st.st_ctime_nsec as i64),
        kind: kind(mode),
        perm: u16::try_from(mode & 0o7777).unwrap_or(0),
        nlink: u32::try_from(st.st_nlink).unwrap_or(u32::MAX),
        uid: st.st_uid,
        gid: st.st_gid,
        rdev: u32::try_from(st.st_rdev).unwrap_or(0),
        blksize: 4096,
        flags: 0,
    }
}

/// The open flags a view passes to the area: the caller's access mode,
/// `O_TRUNC` and the sync flags, never following a link. `O_APPEND` is the
/// view's own (each append writes at the file's end).
fn open_flags(flags: i32) -> OFlags {
    let keep = libc::O_ACCMODE | libc::O_TRUNC | libc::O_SYNC | libc::O_DSYNC;
    #[allow(clippy::cast_sign_loss, reason = "open flags are bits")]
    let bits = (flags & keep) as u32;
    OFlags::from_bits_retain(bits) | OFlags::NOFOLLOW | OFlags::CLOEXEC
}

/// A read at `offset` of at most `size` bytes.
pub(super) fn read(fd: &OwnedFd, offset: u64, size: u32) -> Result<Vec<u8>> {
    let mut bytes = vec![0; size as usize];
    let read = rustix::io::pread(fd, &mut bytes[..], offset).map_err(io)?;
    bytes.truncate(read);
    Ok(bytes)
}

/// A write at `offset`, or at the file's end for `append`: every byte, or
/// an error. It runs under the area's hold, so no fork's copy sees part of
/// it.
pub(super) fn write(
    _writing: &AreaWrite<'_>,
    fd: &OwnedFd,
    offset: u64,
    data: &[u8],
    append: bool,
) -> Result<()> {
    let mut at = if append {
        rustix::fs::fstat(fd).map_err(io)?.st_size as u64
    } else {
        offset
    };
    let mut rest = data;
    while !rest.is_empty() {
        let written = rustix::io::pwrite(fd, rest, at).map_err(io)?;
        if written == 0 {
            return Err(Errno::EIO);
        }
        rest = &rest[written..];
        at += written as u64;
    }
    Ok(())
}
