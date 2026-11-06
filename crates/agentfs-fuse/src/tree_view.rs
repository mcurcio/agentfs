//! Phase 1 flat-file view of the target actor (git-model §3/§7/§18).
//! Direct I/O and zero TTLs; no passthrough or writeback-cache negotiation.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agentfs_core::tree_actor::{Error, Saved, Volume};
use fuser::*;

const TTL: Duration = Duration::ZERO;
type Result<T> = std::result::Result<T, Errno>;

/// Views in one process share one locked volume actor.
pub struct Binding {
    pub tree: String,
    pub writer: String,
    pub mountpoint: PathBuf,
}

/// Mount until every view is unmounted. All mountpoints must be empty,
/// outside the private state and separate from one another.
pub fn serve(state: &Path, volume: Volume, bindings: Vec<Binding>) -> io::Result<()> {
    let state = state.canonicalize()?;
    let mut points: Vec<PathBuf> = Vec::new();
    for binding in &bindings {
        let at = binding.mountpoint.canonicalize()?;
        if !at.is_dir()
            || std::fs::read_dir(&at)?.next().is_some()
            || at.starts_with(&state)
            || state.starts_with(&at)
            || points
                .iter()
                .any(|p| at.starts_with(p) || p.starts_with(&at))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "trees mounts must be empty directories, separate from state and each other",
            ));
        }
        volume.head(&binding.tree).map_err(io::Error::other)?;
        if binding.writer.is_empty()
            || binding.writer.chars().any(char::is_control)
            || binding.writer.len() > 256
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid tree writer",
            ));
        }
        points.push(at);
    }
    let actor = Arc::new(Mutex::new(volume));
    let mut sessions = Vec::new();
    for (binding, at) in bindings.into_iter().zip(points) {
        let view = View {
            actor: actor.clone(),
            tree: binding.tree,
            writer: binding.writer,
            handles: Mutex::new(Handles {
                next: 1,
                entries: BTreeMap::new(),
            }),
        };
        let mut config = Config::default();
        config.mount_options = vec![
            MountOption::FSName("agentfs-trees".into()),
            MountOption::DefaultPermissions,
            MountOption::NoDev,
            MountOption::NoSuid,
            MountOption::RW,
        ];
        config.acl = SessionACL::Owner;
        config.n_threads = Some(1);
        sessions.push(fuser::spawn_mount(view, at, &config)?);
    }
    for session in sessions {
        session.join()?;
    }
    Ok(())
}

struct View {
    actor: Arc<Mutex<Volume>>,
    tree: String,
    writer: String,
    handles: Mutex<Handles>,
}
struct Handles {
    next: u64,
    entries: BTreeMap<u64, Open>,
}
enum Open {
    Read(Vec<u8>),
    Write {
        handle: u64,
        append: bool,
        readable: bool,
    },
}

fn errno(error: Error) -> Errno {
    match error {
        Error::NotFound => Errno::ENOENT,
        Error::Exists => Errno::EEXIST,
        Error::Name | Error::Writer => Errno::EINVAL,
        Error::TooLarge => Errno::EFBIG,
        Error::Busy => Errno::EBUSY,
        Error::Io(e) => Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO)),
        _ => Errno::EIO,
    }
}

impl View {
    fn actor(&self) -> Result<MutexGuard<'_, Volume>> {
        let v = self.actor.lock().map_err(|_| Errno::EIO)?;
        if v.is_stopped() {
            return Err(Errno::EIO);
        }
        Ok(v)
    }
    fn handles(&self) -> Result<MutexGuard<'_, Handles>> {
        self.handles.lock().map_err(|_| Errno::EIO)
    }
    fn path(&self, v: &Volume, ino: INodeNo) -> Result<String> {
        v.files(&self.tree)
            .map_err(errno)?
            .into_iter()
            .find(|(_, o, _)| *o + 2 == ino.0)
            .map(|(p, _, _)| p)
            .ok_or(Errno::ENOENT)
    }
    fn attr(&self, v: &Volume, ino: INodeNo, fh: Option<FileHandle>) -> Result<FileAttr> {
        let size = if ino.0 == 1 {
            0
        } else if let Some(fh) = fh {
            let handles = self.handles()?;
            match handles.entries.get(&fh.0).ok_or(Errno::EBADF)? {
                Open::Read(bytes) => bytes.len(),
                Open::Write { handle, .. } => v.read_open(*handle).map_err(errno)?.len(),
            }
        } else {
            v.read(&self.tree, &self.path(v, ino)?)
                .map_err(errno)?
                .len()
        };
        let dir = ino.0 == 1;
        Ok(FileAttr {
            ino,
            size: size as u64,
            blocks: (size as u64).div_ceil(512),
            atime: UNIX_EPOCH,
            mtime: UNIX_EPOCH,
            ctime: UNIX_EPOCH,
            crtime: UNIX_EPOCH,
            kind: if dir {
                FileType::Directory
            } else {
                FileType::RegularFile
            },
            perm: if dir { 0o700 } else { 0o600 },
            nlink: if dir { 2 } else { 1 },
            uid: rustix::process::geteuid().as_raw(),
            gid: rustix::process::getegid().as_raw(),
            rdev: 0,
            blksize: 4096,
            flags: 0,
        })
    }
    fn open_file(&self, v: &mut Volume, path: &str, flags: i32, create: bool) -> Result<u64> {
        let mut handles = self.handles()?;
        let write = flags & libc::O_ACCMODE != libc::O_RDONLY;
        if flags & libc::O_TRUNC != 0 && !write {
            return Err(Errno::EACCES);
        }
        let entry = if write {
            let handle = v
                .write_open(&self.tree, path, &self.writer, create)
                .map_err(errno)?;
            if flags & libc::O_TRUNC != 0 {
                v.truncate(handle, 0).map_err(errno)?;
            }
            Open::Write {
                handle,
                append: flags & libc::O_APPEND != 0,
                readable: flags & libc::O_ACCMODE == libc::O_RDWR,
            }
        } else {
            if create {
                let h = v
                    .write_open(&self.tree, path, &self.writer, true)
                    .map_err(errno)?;
                v.close(h).map_err(errno)?;
            }
            Open::Read(v.read(&self.tree, path).map_err(errno)?.to_vec())
        };
        let fh = handles.next;
        handles.next += 1;
        handles.entries.insert(fh, entry);
        Ok(fh)
    }
    fn finish(&self, fh: FileHandle) -> Result<()> {
        let mut v = self.actor()?;
        let entry = self.handles()?.entries.remove(&fh.0).ok_or(Errno::EBADF)?;
        if let Open::Write { handle, .. } = entry
            && v.close(handle).map_err(errno)? == Saved::OrphanDropped
        {
            eprintln!("tree OrphanDropped: tree={} open={handle}", self.tree);
        }
        Ok(())
    }
}

impl Filesystem for View {
    fn init(&mut self, _req: &Request, config: &mut KernelConfig) -> io::Result<()> {
        if config
            .capabilities()
            .contains(InitFlags::FUSE_ATOMIC_O_TRUNC)
        {
            let _ = config.add_capabilities(InitFlags::FUSE_ATOMIC_O_TRUNC);
        }
        Ok(())
    }
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let result = (|| {
            if parent.0 != 1 {
                return Err(Errno::ENOTDIR);
            }
            let name = name.to_str().ok_or(Errno::EINVAL)?;
            let v = self.actor()?;
            let (_, object, _) = v
                .files(&self.tree)
                .map_err(errno)?
                .into_iter()
                .find(|(p, _, _)| p == name)
                .ok_or(Errno::ENOENT)?;
            self.attr(&v, INodeNo(object + 2), None)
        })();
        match result {
            Ok(attr) => reply.entry(&TTL, &attr, Generation(0)),
            Err(e) => reply.error(e),
        }
    }
    fn getattr(&self, _req: &Request, ino: INodeNo, fh: Option<FileHandle>, reply: ReplyAttr) {
        match self.actor().and_then(|v| self.attr(&v, ino, fh)) {
            Ok(attr) => reply.attr(&TTL, &attr),
            Err(e) => reply.error(e),
        }
    }
    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let result = (|| {
            let mut v = self.actor()?;
            let path = self.path(&v, ino)?;
            self.open_file(&mut v, &path, flags.0, false)
        })();
        match result {
            Ok(fh) => reply.opened(FileHandle(fh), FopenFlags::FOPEN_DIRECT_IO),
            Err(e) => reply.error(e),
        }
    }
    fn create(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        flags: i32,
        reply: ReplyCreate,
    ) {
        let result = (|| {
            if parent.0 != 1 {
                return Err(Errno::ENOTDIR);
            }
            let path = name.to_str().ok_or(Errno::EINVAL)?;
            let mut v = self.actor()?;
            if flags & libc::O_EXCL != 0 && v.read(&self.tree, path).is_ok() {
                return Err(Errno::EEXIST);
            }
            let fh = self.open_file(&mut v, path, flags, true)?;
            let object = v
                .files(&self.tree)
                .map_err(errno)?
                .into_iter()
                .find(|(p, _, _)| p == path)
                .ok_or(Errno::ENOENT)?
                .1;
            Ok((
                fh,
                self.attr(&v, INodeNo(object + 2), Some(FileHandle(fh)))?,
            ))
        })();
        match result {
            Ok((fh, attr)) => reply.created(
                &TTL,
                &attr,
                Generation(0),
                FileHandle(fh),
                FopenFlags::FOPEN_DIRECT_IO,
            ),
            Err(e) => reply.error(e),
        }
    }
    fn read(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        let result = (|| {
            let v = self.actor()?;
            let handles = self.handles()?;
            let bytes = match handles.entries.get(&fh.0).ok_or(Errno::EBADF)? {
                Open::Read(bytes) => bytes.as_slice(),
                Open::Write {
                    handle,
                    readable: true,
                    ..
                } => v.read_open(*handle).map_err(errno)?,
                _ => return Err(Errno::EBADF),
            };
            let start = usize::try_from(offset)
                .unwrap_or(usize::MAX)
                .min(bytes.len());
            Ok(bytes[start..start.saturating_add(size as usize).min(bytes.len())].to_vec())
        })();
        match result {
            Ok(bytes) => reply.data(&bytes),
            Err(e) => reply.error(e),
        }
    }
    fn write(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        let result = (|| {
            let mut v = self.actor()?;
            let handles = self.handles()?;
            match handles.entries.get(&fh.0).ok_or(Errno::EBADF)? {
                Open::Write { handle, append, .. } => {
                    v.write(*handle, offset, data, *append).map_err(errno)
                }
                _ => Err(Errno::EBADF),
            }
        })();
        match result {
            Ok(()) => reply.written(data.len() as u32),
            Err(e) => reply.error(e),
        }
    }
    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        let result = (|| {
            if mode.is_some()
                || uid.is_some()
                || gid.is_some()
                || atime.is_some()
                || mtime.is_some()
            {
                return Err(Errno::EOPNOTSUPP);
            }
            let mut v = self.actor()?;
            if let Some(size) = size {
                if let Some(fh) = fh {
                    let handles = self.handles()?;
                    match handles.entries.get(&fh.0).ok_or(Errno::EBADF)? {
                        Open::Write { handle, .. } => v.truncate(*handle, size).map_err(errno)?,
                        _ => return Err(Errno::EBADF),
                    }
                } else {
                    let path = self.path(&v, ino)?;
                    let h = v
                        .write_open(&self.tree, &path, &self.writer, false)
                        .map_err(errno)?;
                    let result = v.truncate(h, size).map_err(errno);
                    v.close(h).map_err(errno)?;
                    result?;
                }
            }
            self.attr(&v, ino, fh)
        })();
        match result {
            Ok(attr) => reply.attr(&TTL, &attr),
            Err(e) => reply.error(e),
        }
    }
    fn flush(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _owner: LockOwner,
        reply: ReplyEmpty,
    ) {
        match self.actor().and_then(|v| v.sync().map_err(errno)) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }
    fn fsync(
        &self,
        req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        self.flush(req, ino, fh, LockOwner(0), reply);
    }
    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        match self.finish(fh) {
            Ok(()) => reply.ok(),
            Err(e) => {
                eprintln!(
                    "tree SaveFailed: tree={} errno={e:?}; preserve state and restart",
                    self.tree
                );
                reply.error(e);
            }
        }
    }
    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let result = (|| {
            if parent.0 != 1 {
                return Err(Errno::ENOTDIR);
            }
            self.actor()?
                .unlink(
                    &self.tree,
                    name.to_str().ok_or(Errno::EINVAL)?,
                    &self.writer,
                )
                .map_err(errno)
        })();
        match result {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }
    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        let result = (|| {
            if parent.0 != 1 || newparent.0 != 1 {
                return Err(Errno::ENOTDIR);
            }
            if !flags.is_empty() {
                return Err(Errno::EOPNOTSUPP);
            }
            self.actor()?
                .rename(
                    &self.tree,
                    name.to_str().ok_or(Errno::EINVAL)?,
                    newname.to_str().ok_or(Errno::EINVAL)?,
                    &self.writer,
                )
                .map_err(errno)
        })();
        match result {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }
    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let result = (|| {
            if ino.0 != 1 {
                return Err(Errno::ENOTDIR);
            }
            let v = self.actor()?;
            let mut entries = vec![
                (INodeNo(1), FileType::Directory, ".".to_owned()),
                (INodeNo(1), FileType::Directory, "..".to_owned()),
            ];
            entries.extend(
                v.files(&self.tree)
                    .map_err(errno)?
                    .into_iter()
                    .map(|(p, o, _)| (INodeNo(o + 2), FileType::RegularFile, p)),
            );
            Ok(entries)
        })();
        match result {
            Ok(entries) => {
                for (i, (ino, kind, name)) in entries.into_iter().enumerate().skip(offset as usize)
                {
                    if reply.add(ino, (i + 1) as u64, kind, name) {
                        break;
                    }
                }
                reply.ok();
            }
            Err(e) => reply.error(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_bindings_are_checked_before_any_mount_can_expose_state() {
        for invalid in ["state", "state/inside", "."] {
            let tmp = tempfile::tempdir().unwrap();
            let state = tmp.path().join("state");
            let volume = Volume::open(&state).unwrap();
            let outside = tmp.path().join("valid");
            std::fs::create_dir(&outside).unwrap();
            let invalid = tmp.path().join(invalid);
            std::fs::create_dir_all(&invalid).unwrap();
            let bindings = vec![
                Binding {
                    tree: "main".into(),
                    writer: "main".into(),
                    mountpoint: outside,
                },
                Binding {
                    tree: "main".into(),
                    writer: "other".into(),
                    mountpoint: invalid,
                },
            ];
            assert_eq!(
                serve(&state, volume, bindings).unwrap_err().kind(),
                io::ErrorKind::InvalidInput
            );
        }
    }

    #[test]
    fn a_readers_snapshot_survives_a_later_save_and_readonly_truncation_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let view = View {
            actor: Arc::new(Mutex::new(Volume::open(tmp.path()).unwrap())),
            tree: "main".into(),
            writer: "client".into(),
            handles: Mutex::new(Handles {
                next: 1,
                entries: BTreeMap::new(),
            }),
        };
        let mut volume = view.actor().unwrap();
        let h = volume.write_open("main", "a", "seed", true).unwrap();
        volume.write(h, 0, b"old", false).unwrap();
        volume.close(h).unwrap();
        let reader = view
            .open_file(&mut volume, "a", libc::O_RDONLY, false)
            .unwrap();
        let h = volume.write_open("main", "a", "next", false).unwrap();
        volume.truncate(h, 0).unwrap();
        volume.write(h, 0, b"new", false).unwrap();
        volume.close(h).unwrap();
        assert_eq!(volume.read("main", "a").unwrap(), b"new");
        match &view.handles().unwrap().entries[&reader] {
            Open::Read(bytes) => assert_eq!(bytes, b"old"),
            _ => panic!("read-only open acquired a writer"),
        }
        assert_eq!(
            view.open_file(&mut volume, "a", libc::O_RDONLY | libc::O_TRUNC, false),
            Err(Errno::EACCES)
        );
        assert_eq!(volume.read("main", "a").unwrap(), b"new");
    }
}
