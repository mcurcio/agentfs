//! Each tree's untracked area (docs/trees.md, "Rules and the untracked
//! area"): the tree's files the rules leave out of history, kept as plain
//! files under `<state>/untracked/<tree>/` and reached only beneath it.
//!
//! - A view serves them as ordinary Linux files: no journal, no history.
//! - A new fork's area starts as a copy of its upstream's, taken while the
//!   upstream's untracked writes are held, so the copy is the upstream's
//!   files at one instant ([`copy_area`]). The copy is published before the
//!   fork's record; an area no tree owns, and every copy a crash cut short,
//!   is removed when the volume opens ([`sweep`]).
//! - A dropped fork's area is removed when its drop is recorded.

// `stat` field types differ between architectures, so conversions written
// to be portable are identities on some targets.
#![allow(clippy::useless_conversion, clippy::unnecessary_cast)]

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::io::{self, Read};
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

use rustix::fs::{AtFlags, FileType, Mode, OFlags, Stat, Timestamps};

use super::TreeId;
use super::meta::{Kind, Meta};
use crate::confined;

/// The state area's directory of untracked areas.
pub(in crate::tree_actor) const UNTRACKED: &str = "untracked";
/// Where files are written before they are renamed into an area, and where
/// a fork's copy is made before it is published.
const STAGING: &str = ".staging";

/// A tree's untracked area, `<state>/untracked/<tree>/` (mode 0700, made on
/// first use): the tree's files the rules leave out, reached only beneath it
/// (`openat2` with `RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS |
/// RESOLVE_NO_MAGICLINKS`, never following a link).
pub struct UntrackedArea {
    dir: confined::Dir,
    tree: TreeId,
    /// `<state>/untracked/.staging/`, for [`UntrackedArea::place`].
    staging: PathBuf,
    /// Held shared by every change to the area, and exclusively while a fork
    /// copies it.
    hold: Arc<RwLock<()>>,
}

impl std::fmt::Debug for UntrackedArea {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UntrackedArea")
            .field("tree", &self.tree)
            .field("dir", &self.dir.path())
            .finish_non_exhaustive()
    }
}

/// A change to an area in progress: while any is held, no fork copies it.
pub struct AreaWrite<'a> {
    _held: RwLockReadGuard<'a, ()>,
}

impl UntrackedArea {
    pub fn tree(&self) -> TreeId {
        self.tree
    }

    pub fn dir(&self) -> &confined::Dir {
        &self.dir
    }

    /// Hold the area against a fork's copy for one change: every call that
    /// changes a file, a name or metadata in the area holds this while it
    /// runs. Never take the actor's lock while holding it.
    pub fn writing(&self) -> AreaWrite<'_> {
        AreaWrite {
            _held: self.hold.read().unwrap_or_else(PoisonError::into_inner),
        }
    }

    /// `path`'s parent directory opened `O_PATH | O_DIRECTORY` beneath the
    /// area, and its last name: where a passthrough `*at` call acts.
    ///
    /// # Errors
    ///
    /// `ENOENT` for a missing parent, `ELOOP` for a link on the way,
    /// `ENOTDIR` for a file on the way.
    pub fn parent(&self, path: &str) -> io::Result<(OwnedFd, String)> {
        let (parent, name) = split(path);
        let fd = self
            .dir
            .open_at(parent, OFlags::PATH | OFlags::DIRECTORY, Mode::empty())?;
        Ok((fd, name.to_owned()))
    }

    /// The `lstat` of `path` (`""` is the area itself); `None` when nothing
    /// is there as a plain path (missing, or under a file or a link).
    pub fn stat(&self, path: &str) -> io::Result<Option<Stat>> {
        match self.dir.stat_at(path) {
            Ok(found) => Ok(found.map(|(_, st)| st)),
            Err(e) if confined::is_gone(&e) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Makes each missing directory above `path` as a container standing for
    /// the tracked directory there: the mode `meta_of` gives it (0755 when it
    /// gives none), and its owner when this process may give files away.
    ///
    /// # Errors
    ///
    /// A failed `mkdirat`, or an entry on the way that is no directory.
    pub fn containers(
        &self,
        path: &str,
        meta_of: &mut dyn FnMut(&str) -> Option<Meta>,
    ) -> io::Result<()> {
        let chown = confined::can_chown();
        let mut end = 0;
        let parts: Vec<&str> = path.split('/').collect();
        for (i, part) in parts.iter().enumerate().take(parts.len().saturating_sub(1)) {
            end += part.len() + usize::from(i > 0);
            let dir = &path[..end];
            if self.stat(dir)?.is_some() {
                continue;
            }
            let meta = meta_of(dir);
            let mode = meta.map_or(0o755, |m| m.mode.bits());
            let (parent, name) = self.parent(dir)?;
            match rustix::fs::mkdirat(&parent, name.as_str(), Mode::from_raw_mode(mode)) {
                Ok(()) | Err(rustix::io::Errno::EXIST) => {}
                Err(e) => return Err(e.into()),
            }
            let fd = self
                .dir
                .open_at(dir, OFlags::RDONLY | OFlags::DIRECTORY, Mode::empty())?;
            // The umask does not apply.
            rustix::fs::fchmod(&fd, Mode::from_raw_mode(mode))?;
            if let (true, Some(meta)) = (chown, meta) {
                rustix::fs::fchown(
                    &fd,
                    Some(rustix::fs::Uid::from_raw(meta.uid)),
                    Some(rustix::fs::Gid::from_raw(meta.gid)),
                )?;
            }
        }
        Ok(())
    }

    /// `dir`'s entries sorted by name, without `.` and `..`; names that are
    /// not UTF-8 are left out. A directory the area does not have has none.
    ///
    /// # Errors
    ///
    /// A failed open or read of the directory.
    pub fn entries(&self, dir: &str) -> io::Result<Vec<(String, FileType)>> {
        let fd = match self
            .dir
            .open_at(dir, OFlags::RDONLY | OFlags::DIRECTORY, Mode::empty())
        {
            Ok(fd) => fd,
            Err(e) if confined::is_gone(&e) => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        let mut out = Vec::new();
        for entry in rustix::fs::Dir::read_from(&fd)? {
            let entry = entry?;
            let name = entry.file_name().to_bytes();
            if name == b"." || name == b".." {
                continue;
            }
            let Ok(name) = std::str::from_utf8(name) else {
                continue;
            };
            let kind = match entry.file_type() {
                FileType::Unknown => {
                    let st = rustix::fs::statat(&fd, name, AtFlags::SYMLINK_NOFOLLOW)?;
                    FileType::from_raw_mode(st.st_mode)
                }
                kind => kind,
            };
            out.push((name.to_owned(), kind));
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }

    /// Puts `len` bytes of `src` at `path` with `meta`'s mode and owner (a
    /// link when `meta.kind` is [`Kind::Symlink`], `src` holding its
    /// target), replacing any non-directory there: written under
    /// `<state>/untracked/.staging/`, fsynced, then renamed into place, so a
    /// crash leaves the old entry or the new one. The containers above `path`
    /// must exist.
    ///
    /// # Errors
    ///
    /// `EISDIR` for a directory, any failed write, and a source shorter than
    /// `len`.
    pub fn place(&self, path: &str, src: &mut dyn Read, len: u64, meta: Meta) -> io::Result<()> {
        if meta.kind == Kind::Dir {
            return Err(io::Error::from_raw_os_error(libc::EISDIR));
        }
        let _writing = self.writing();
        let (parent, name) = self.parent(path)?;
        fs_create_dir(&self.staging)?;
        let staging = rustix::fs::open(
            &self.staging,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        let temporary = format!("place-{}-{}", std::process::id(), unique());
        let chown = confined::can_chown();
        let written = (|| -> io::Result<()> {
            if meta.kind == Kind::Symlink {
                let mut target = Vec::new();
                src.take(len).read_to_end(&mut target)?;
                rustix::fs::symlinkat(OsStr::from_bytes(&target), &staging, temporary.as_str())?;
                if chown {
                    rustix::fs::chownat(
                        &staging,
                        temporary.as_str(),
                        Some(rustix::fs::Uid::from_raw(meta.uid)),
                        Some(rustix::fs::Gid::from_raw(meta.gid)),
                        AtFlags::SYMLINK_NOFOLLOW,
                    )?;
                }
                return Ok(());
            }
            let fd = rustix::fs::openat(
                &staging,
                temporary.as_str(),
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::from_raw_mode(0o600),
            )?;
            let mut file = std::fs::File::from(fd);
            let copied = io::copy(&mut src.take(len), &mut file)?;
            if copied != len {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    format!("the content ended after {copied} of {len} bytes"),
                ));
            }
            rustix::fs::fchmod(&file, Mode::from_raw_mode(meta.mode.bits()))?;
            if chown {
                rustix::fs::fchown(
                    &file,
                    Some(rustix::fs::Uid::from_raw(meta.uid)),
                    Some(rustix::fs::Gid::from_raw(meta.gid)),
                )?;
            }
            file.sync_all()
        })();
        if let Err(e) = written {
            let _ = rustix::fs::unlinkat(&staging, temporary.as_str(), AtFlags::empty());
            return Err(e);
        }
        if let Err(e) = rustix::fs::renameat(&staging, temporary.as_str(), &parent, name.as_str()) {
            let _ = rustix::fs::unlinkat(&staging, temporary.as_str(), AtFlags::empty());
            return Err(e.into());
        }
        let parent = self.dir.open_at(
            split(path).0,
            OFlags::RDONLY | OFlags::DIRECTORY,
            Mode::empty(),
        )?;
        rustix::fs::fsync(&parent)?;
        Ok(())
    }
}

/// `path`'s parent (`""` the area) and last name.
fn split(path: &str) -> (&str, &str) {
    path.rsplit_once('/').unwrap_or(("", path))
}

/// A number no other temporary of this process has had.
fn unique() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// Makes `dir` and its parents, each new one mode 0700.
fn fs_create_dir(dir: &Path) -> io::Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

/// Every tree's area of one volume, and each tree's hold: what the actor
/// hands out and what a fork's copy holds.
#[derive(Debug)]
pub(in crate::tree_actor) struct Areas {
    /// `<state>/untracked`.
    root: PathBuf,
    holds: Mutex<BTreeMap<TreeId, Arc<RwLock<()>>>>,
}

impl Areas {
    pub(in crate::tree_actor) fn new(state: &Path) -> Self {
        Self {
            root: state.join(UNTRACKED),
            holds: Mutex::default(),
        }
    }

    fn hold(&self, tree: TreeId) -> Arc<RwLock<()>> {
        let mut holds = self.holds.lock().unwrap_or_else(PoisonError::into_inner);
        Arc::clone(holds.entry(tree).or_default())
    }

    fn path(&self, tree: TreeId) -> PathBuf {
        self.root.join(tree.to_string())
    }

    /// `tree`'s area, made (mode 0700) if it has none.
    pub(in crate::tree_actor) fn open(&self, tree: TreeId) -> io::Result<UntrackedArea> {
        let path = self.path(tree);
        create_dir_durably(&path)?;
        Ok(UntrackedArea {
            dir: confined::Dir::open(&path)?,
            tree,
            staging: self.root.join(STAGING),
            hold: self.hold(tree),
        })
    }

    /// Copies `from`'s area as `to`'s, published under its name before this
    /// returns, durably: a copy made under `.staging/` while `from`'s changes
    /// are held, each file fsynced, then renamed into place and the rename
    /// fsynced. Nothing is published when `from` has no area.
    ///
    /// # Errors
    ///
    /// Any failed copy, with nothing published.
    pub(in crate::tree_actor) fn copy_area(&self, from: TreeId, to: TreeId) -> io::Result<()> {
        let source = self.path(from);
        let hold = self.hold(from);
        let _held: RwLockWriteGuard<'_, ()> = hold.write().unwrap_or_else(PoisonError::into_inner);
        let source = match confined::Dir::open(&source) {
            Ok(dir) => dir,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        };
        let staging = self.root.join(STAGING);
        fs_create_dir(&staging)?;
        let temporary = staging.join(format!("copy-{}", to.to_string().replace('/', "-")));
        remove_all(&temporary)?;
        let target = self.path(to);
        if let Err(e) = copy_tree(&source, &temporary) {
            let _ = remove_all(&temporary);
            return Err(e);
        }
        let published = self.publish(&temporary, &target);
        if published.is_err() {
            // Past the rename the copy is under the fork's name: removed, so
            // a fork made again in this process finds no copy in its way.
            let _ = remove_all(&temporary);
            let _ = remove_all(&target);
        }
        published
    }

    /// Renames the finished copy `temporary` to `target`, durably: every
    /// directory made on the way to `target`, and the rename itself, are
    /// fsynced in their parents.
    fn publish(&self, temporary: &Path, target: &Path) -> io::Result<()> {
        let parent = target.parent().unwrap_or(&self.root);
        create_dir_durably(parent)?;
        std::fs::rename(temporary, target)?;
        sync_dir(parent)?;
        sync_dir(&self.root.join(STAGING))?;
        crate::failpoints::hit("tree.fork_after_copy")
    }

    /// Removes `tree`'s area, if it has one.
    pub(in crate::tree_actor) fn remove(&self, tree: TreeId) -> io::Result<()> {
        remove_all(&self.path(tree))
    }

    /// At open: removes every copy a crash cut short, and every fork's area
    /// whose fork is not among `live`.
    pub(in crate::tree_actor) fn sweep(&self, live: &[TreeId]) -> io::Result<()> {
        remove_all(&self.root.join(STAGING))?;
        sweep_forks(&self.root, live)
    }
}

/// Removes each `<untracked>/fork/<n>` whose fork is not among `live`.
pub(in crate::tree_actor) fn sweep_forks(untracked: &Path, live: &[TreeId]) -> io::Result<()> {
    let forks = untracked.join("fork");
    let entries = match std::fs::read_dir(&forks) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    for entry in entries {
        let entry = entry?;
        let tree = format!("fork/{}", entry.file_name().to_string_lossy()).parse::<TreeId>();
        match tree {
            Ok(tree) if live.contains(&tree) => {}
            Ok(_) => remove_all(&entry.path())?,
            // Nothing a tree's area is named: never agentfs's to delete.
            Err(_) => {}
        }
    }
    Ok(())
}

/// Removes `path` and everything under it; nothing there is fine.
pub(in crate::tree_actor) fn remove_all(path: &Path) -> io::Result<()> {
    match std::fs::remove_dir_all(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

fn sync_dir(dir: &Path) -> io::Result<()> {
    std::fs::File::open(dir)?.sync_all()
}

/// Makes `dir` and its missing parents (each mode 0700), and fsyncs the
/// parent of each one made, so their names survive a power cut.
fn create_dir_durably(dir: &Path) -> io::Result<()> {
    let mut missing = Vec::new();
    let mut at = dir;
    while !at.exists() {
        missing.push(at);
        match at.parent() {
            Some(parent) => at = parent,
            None => break,
        }
    }
    fs_create_dir(dir)?;
    for made in missing.iter().rev() {
        if let Some(parent) = made.parent() {
            sync_dir(parent)?;
        }
    }
    Ok(())
}

/// Copies the tree under `source` to the new directory `target`: each
/// directory, file and link with its mode, owner (when this process may give
/// files away) and times; each file reflinked where the filesystem can,
/// else copied; every file and directory fsynced. A file with several names
/// is copied once and its other names linked to the copy. FIFOs, sockets,
/// devices and names that are not UTF-8 are left out.
fn copy_tree(source: &confined::Dir, target: &Path) -> io::Result<()> {
    let chown = confined::can_chown();
    std::fs::DirBuilder::new().mode(0o700).create(target)?;
    let target = confined::Dir::open(target)?;
    let top = target.open_at("", OFlags::RDONLY | OFlags::DIRECTORY, Mode::empty())?;
    // Each multiply-linked file's first copy, by the source's device and inode.
    let mut linked: BTreeMap<(u64, u64), String> = BTreeMap::new();
    let mut stack = vec![String::new()];
    let mut dirs = Vec::new();
    while let Some(dir) = stack.pop() {
        let from = source.open_at(&dir, OFlags::RDONLY | OFlags::DIRECTORY, Mode::empty())?;
        let to = target.open_at(&dir, OFlags::RDONLY | OFlags::DIRECTORY, Mode::empty())?;
        for entry in rustix::fs::Dir::read_from(&from)? {
            let entry = entry?;
            let name = entry.file_name();
            if matches!(name.to_bytes(), b"." | b"..") {
                continue;
            }
            // A view makes no such name; one written into the state area
            // directly is no file of the tree's, as `entries` leaves it out.
            let Ok(utf8) = std::str::from_utf8(name.to_bytes()) else {
                continue;
            };
            let st = rustix::fs::statat(&from, name, AtFlags::SYMLINK_NOFOLLOW)?;
            let rel = if dir.is_empty() {
                utf8.to_owned()
            } else {
                format!("{dir}/{utf8}")
            };
            match FileType::from_raw_mode(st.st_mode) {
                FileType::Directory => {
                    rustix::fs::mkdirat(&to, name, Mode::from_raw_mode(0o700))?;
                    stack.push(rel.clone());
                    dirs.push((rel, st));
                }
                FileType::RegularFile
                    if st.st_nlink > 1
                        && linked.contains_key(&(st.st_dev as u64, st.st_ino as u64)) =>
                {
                    // Another name of a file already copied: linked to its copy.
                    let first = &linked[&(st.st_dev as u64, st.st_ino as u64)];
                    rustix::fs::linkat(&top, first.as_str(), &to, name, AtFlags::empty())?;
                }
                FileType::RegularFile => {
                    if st.st_nlink > 1 {
                        linked.insert((st.st_dev as u64, st.st_ino as u64), rel.clone());
                    }
                    let src = rustix::fs::openat(
                        &from,
                        name,
                        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                        Mode::empty(),
                    )?;
                    let dst = rustix::fs::openat(
                        &to,
                        name,
                        OFlags::WRONLY
                            | OFlags::CREATE
                            | OFlags::EXCL
                            | OFlags::NOFOLLOW
                            | OFlags::CLOEXEC,
                        Mode::from_raw_mode(0o600),
                    )?;
                    if rustix::fs::ioctl_ficlone(&dst, &src).is_err() {
                        let mut dst = std::fs::File::from(dst.try_clone()?);
                        io::copy(&mut std::fs::File::from(src), &mut dst)?;
                    }
                    set_meta(&dst, &st, chown)?;
                    rustix::fs::fsync(&dst)?;
                    crate::failpoints::hit("tree.fork_copy_file")?;
                }
                FileType::Symlink => {
                    let link = rustix::fs::readlinkat(&from, name, Vec::new())?;
                    rustix::fs::symlinkat(link.as_c_str(), &to, name)?;
                    let times = times_of(&st);
                    rustix::fs::utimensat(&to, name, &times, AtFlags::SYMLINK_NOFOLLOW)?;
                    if chown {
                        rustix::fs::chownat(
                            &to,
                            name,
                            Some(rustix::fs::Uid::from_raw(st.st_uid)),
                            Some(rustix::fs::Gid::from_raw(st.st_gid)),
                            AtFlags::SYMLINK_NOFOLLOW,
                        )?;
                    }
                }
                _ => {}
            }
        }
    }
    // Deepest first: a directory's times after its entries are made.
    for (rel, st) in dirs.iter().rev() {
        let fd = target.open_at(rel, OFlags::RDONLY | OFlags::DIRECTORY, Mode::empty())?;
        set_meta(&fd, st, chown)?;
        rustix::fs::fsync(&fd)?;
    }
    rustix::fs::fsync(&top)?;
    Ok(())
}

fn times_of(st: &Stat) -> Timestamps {
    Timestamps {
        last_access: rustix::fs::Timespec {
            tv_sec: st.st_atime as i64,
            tv_nsec: st.st_atime_nsec as _,
        },
        last_modification: rustix::fs::Timespec {
            tv_sec: st.st_mtime as i64,
            tv_nsec: st.st_mtime_nsec as _,
        },
    }
}

/// `st`'s owner (when `chown`), mode and times on the open `fd`.
fn set_meta(fd: &impl AsFd, st: &Stat, chown: bool) -> io::Result<()> {
    if chown {
        rustix::fs::fchown(
            fd,
            Some(rustix::fs::Uid::from_raw(st.st_uid)),
            Some(rustix::fs::Gid::from_raw(st.st_gid)),
        )?;
    }
    rustix::fs::fchmod(fd, Mode::from_raw_mode(u32::from(st.st_mode) & 0o7777))?;
    rustix::fs::futimens(fd, &times_of(st))?;
    Ok(())
}
