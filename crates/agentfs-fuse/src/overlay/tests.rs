//! The overlay at two isolation levels: [`FakeOps`], which records every
//! call and never touches a disk, proves what is refused before the
//! operations are asked, the existence-blind answers and the owner
//! standing in for the caller; a real [`Ops`] on a temporary directory
//! proves complete mediation and listings over real rules.

use std::collections::HashMap;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::UNIX_EPOCH;

use agentfs_core::events::MemorySink;
use agentfs_core::identity::Context;
use agentfs_core::record::Trigger;
use agentfs_core::rules::{RuleOptions, Rules};
use agentfs_core::telemetry::Telemetry;
use agentfs_core::volume::{self, StateDir};
use fuser::INodeNo;

use super::tmp::TmpLayer;
use super::*;
use crate::backing::Backing;
use crate::ops::{Ops, OpsConfig};
use crate::session::Moved;

/// The worked examples' allowlist, plus a skipped pattern.
const EXCLUDE: &str = "/*\n!/config.yaml\n!/skills/\n*.key\n";

const OWNER: OwnerIds = OwnerIds {
    uid: 10000,
    gid: 10000,
};
const OTHER: u32 = 20000;

/// A request from a sandbox process that is neither root nor the owner.
const REQUEST: Caller = Caller {
    uid: OTHER,
    gid: OTHER,
    pid: 4242,
};

const O_RDONLY: i32 = 0;
const O_WRONLY: i32 = 1;
const O_RDWR: i32 = 2;

fn owner() -> ViewOwner {
    ViewOwner::new(OWNER, &Privileges::default(), OWNER.uid).unwrap()
}

fn attr(id: u64, kind: FileType, perm: u16, uid: u32) -> FileAttr {
    FileAttr {
        ino: INodeNo(id),
        size: 0,
        blocks: 0,
        atime: UNIX_EPOCH,
        mtime: UNIX_EPOCH,
        ctime: UNIX_EPOCH,
        crtime: UNIX_EPOCH,
        kind,
        perm,
        nlink: 1,
        uid,
        gid: uid,
        rdev: 0,
        blksize: 4096,
        flags: 0,
    }
}

/// One call [`FakeOps`] received: the method, the name it named (if any)
/// and the caller it was handed (if any).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Call {
    method: &'static str,
    name: Vec<u8>,
    caller: Option<Caller>,
}

/// The extended attribute names of every [`FakeOps`] node.
const FAKE_XATTRS: &[u8] = b"user.a\0trusted.b\0security.c\0user.d\0";

/// A [`Found`] that owns its path.
struct Owned {
    path: Vec<u8>,
    kind: Option<Kind>,
}

impl Owned {
    fn as_found(&self) -> Found<'_> {
        Found {
            path: &self.path,
            kind: self.kind,
        }
    }
}

/// The inode table's kind of fuser kind `kind`.
fn kind_of(kind: FileType) -> Kind {
    match kind {
        FileType::RegularFile => Kind::File,
        FileType::Directory => Kind::Dir,
        FileType::Symlink => Kind::Symlink,
        _ => Kind::Other,
    }
}

/// An [`Operations`] with scripted nodes that records every call.
#[derive(Default)]
struct FakeOps {
    calls: Mutex<Vec<Call>>,
    attrs: HashMap<u64, FileAttr>,
    paths: HashMap<u64, Vec<u8>>,
    children: HashMap<(u64, Vec<u8>), u64>,
    /// The last node and handle id handed out (fresh ones start at 1000).
    next: AtomicU64,
}

impl FakeOps {
    /// The root, a directory of mode 0755 owned by the owner.
    fn new() -> Self {
        let mut fake = Self::default();
        fake.attrs
            .insert(ROOT, attr(ROOT, FileType::Directory, 0o755, OWNER.uid));
        fake.paths.insert(ROOT, Vec::new());
        fake
    }

    /// Adds node `id`, `name` in `parent`.
    fn node(
        mut self,
        id: u64,
        parent: u64,
        name: &str,
        kind: FileType,
        perm: u16,
        uid: u32,
    ) -> Self {
        let path = join(&self.paths[&parent], name.as_bytes());
        self.attrs.insert(id, attr(id, kind, perm, uid));
        self.paths.insert(id, path);
        self.children.insert((parent, name.as_bytes().to_vec()), id);
        self
    }

    /// The tree of the worked examples: config.yaml, secrets.db, notes/,
    /// skills/, skills/a.md and skills/d/.
    fn examples() -> Self {
        Self::new()
            .node(
                2,
                ROOT,
                "config.yaml",
                FileType::RegularFile,
                0o644,
                OWNER.uid,
            )
            .node(
                3,
                ROOT,
                "secrets.db",
                FileType::RegularFile,
                0o644,
                OWNER.uid,
            )
            .node(4, ROOT, "notes", FileType::Directory, 0o755, OWNER.uid)
            .node(5, ROOT, "skills", FileType::Directory, 0o755, OWNER.uid)
            .node(6, 5, "a.md", FileType::RegularFile, 0o644, OWNER.uid)
            .node(7, 5, "d", FileType::Directory, 0o755, OWNER.uid)
    }

    fn record(&self, method: &'static str, name: &[u8], caller: Option<Caller>) {
        self.calls.lock().unwrap().push(Call {
            method,
            name: name.to_vec(),
            caller,
        });
    }

    fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }

    fn calls_to(&self, method: &str) -> Vec<Call> {
        self.calls()
            .into_iter()
            .filter(|c| c.method == method)
            .collect()
    }

    fn entry(&self, id: u64) -> Entry {
        Entry {
            id,
            attr: self.attrs[&id],
        }
    }

    /// What the fake finds at `name` in `parent`, as the operations hand
    /// it to a check.
    fn found(&self, parent: u64, name: &[u8]) -> Owned {
        Owned {
            path: join(self.paths.get(&parent).map_or(&[][..], Vec::as_slice), name),
            kind: self
                .children
                .get(&(parent, name.to_vec()))
                .map(|id| kind_of(self.attrs[id].kind)),
        }
    }

    /// Asks `actor`'s check about a new entry, as the operations do.
    fn check_create(&self, actor: Actor<'_>, parent: u64, name: &[u8], kind: Kind) -> OpResult<()> {
        match actor.check {
            Some(check) => check.create(self.found(parent, name).as_found(), kind),
            None => Ok(()),
        }
    }

    fn handle(id: u64) -> Arc<Handle> {
        let fd: OwnedFd = std::fs::File::open("/dev/null").unwrap().into();
        Arc::new(Handle::new(id, fd, false, None, 0))
    }
}

impl Operations for FakeOps {
    fn lookup(&self, parent: u64, name: &[u8]) -> OpResult<Entry> {
        self.record("lookup", name, None);
        let id = self
            .children
            .get(&(parent, name.to_vec()))
            .ok_or(Errno::ENOENT)?;
        Ok(self.entry(*id))
    }

    fn forget(&self, _id: u64, _n: u64) {
        self.record("forget", b"", None);
    }

    fn getattr(&self, id: u64, _fh: Option<u64>) -> OpResult<FileAttr> {
        self.record("getattr", b"", None);
        self.attrs.get(&id).copied().ok_or(Errno::ENOENT)
    }

    fn setattr(&self, id: u64, _set: &SetAttr, actor: Actor<'_>) -> OpResult<FileAttr> {
        self.record("setattr", b"", Some(actor.caller));
        self.attrs.get(&id).copied().ok_or(Errno::ENOENT)
    }

    fn readlink(&self, _id: u64) -> OpResult<Vec<u8>> {
        self.record("readlink", b"", None);
        Ok(Vec::new())
    }

    fn access(&self, _id: u64, _mask: i32) -> OpResult<()> {
        self.record("access", b"", None);
        Ok(())
    }

    fn paths(&self, id: u64) -> Vec<Vec<u8>> {
        self.paths.get(&id).cloned().into_iter().collect()
    }

    fn mknod(
        &self,
        parent: u64,
        name: &[u8],
        mode: u32,
        _rdev: u32,
        actor: Actor<'_>,
    ) -> OpResult<Entry> {
        self.record("mknod", name, Some(actor.caller));
        self.check_create(actor, parent, name, Kind::from_mode(mode))?;
        Ok(self.entry(2))
    }

    fn mkdir(&self, parent: u64, name: &[u8], _mode: u32, actor: Actor<'_>) -> OpResult<Entry> {
        self.record("mkdir", name, Some(actor.caller));
        self.check_create(actor, parent, name, Kind::Dir)?;
        Ok(self.entry(5))
    }

    fn unlink(&self, parent: u64, name: &[u8], actor: Actor<'_>) -> OpResult<()> {
        self.record("unlink", name, Some(actor.caller));
        if let Some(check) = actor.check {
            check.remove(self.found(parent, name).as_found())?;
        }
        Ok(())
    }

    fn rmdir(&self, parent: u64, name: &[u8], actor: Actor<'_>) -> OpResult<()> {
        self.record("rmdir", name, Some(actor.caller));
        if let Some(check) = actor.check {
            check.remove(self.found(parent, name).as_found())?;
        }
        Ok(())
    }

    fn symlink(
        &self,
        parent: u64,
        name: &[u8],
        _target: &[u8],
        actor: Actor<'_>,
    ) -> OpResult<Entry> {
        self.record("symlink", name, Some(actor.caller));
        self.check_create(actor, parent, name, Kind::Symlink)?;
        Ok(self.entry(2))
    }

    fn rename(
        &self,
        parent: u64,
        name: &[u8],
        newparent: u64,
        newname: &[u8],
        flags: u32,
        actor: Actor<'_>,
    ) -> OpResult<()> {
        self.record("rename", name, Some(actor.caller));
        if let Some(check) = actor.check {
            check.rename(
                self.found(parent, name).as_found(),
                self.found(newparent, newname).as_found(),
                flags,
            )?;
        }
        Ok(())
    }

    fn link(&self, _id: u64, newparent: u64, newname: &[u8], actor: Actor<'_>) -> OpResult<Entry> {
        self.record("link", newname, Some(actor.caller));
        if let Some(check) = actor.check {
            check.link(self.found(newparent, newname).as_found())?;
        }
        Ok(self.entry(2))
    }

    fn open(&self, id: u64, _flags: i32, actor: Actor<'_>) -> OpResult<(u64, Arc<Handle>)> {
        self.record("open", b"", Some(actor.caller));
        Ok((1, Self::handle(id)))
    }

    fn create(
        &self,
        parent: u64,
        name: &[u8],
        _mode: u32,
        _flags: i32,
        actor: Actor<'_>,
    ) -> OpResult<(Entry, u64, Arc<Handle>)> {
        self.record("create", name, Some(actor.caller));
        self.check_create(actor, parent, name, Kind::File)?;
        Ok((self.entry(2), 1, Self::handle(2)))
    }

    fn read(&self, _fh: u64, _offset: u64, _size: u32) -> OpResult<Vec<u8>> {
        self.record("read", b"", None);
        Ok(Vec::new())
    }

    fn write(&self, _fh: u64, _offset: u64, data: &[u8], actor: Actor<'_>) -> OpResult<u32> {
        self.record("write", b"", Some(actor.caller));
        Ok(u32::try_from(data.len()).unwrap())
    }

    fn flush(&self, _fh: u64, actor: Actor<'_>) -> OpResult<()> {
        self.record("flush", b"", Some(actor.caller));
        Ok(())
    }

    fn release(&self, _fh: u64) -> OpResult<()> {
        self.record("release", b"", None);
        Ok(())
    }

    fn fsync(&self, _fh: u64, _datasync: bool) -> OpResult<()> {
        self.record("fsync", b"", None);
        Ok(())
    }

    fn fallocate(
        &self,
        _fh: u64,
        _offset: u64,
        _length: u64,
        _mode: i32,
        actor: Actor<'_>,
    ) -> OpResult<()> {
        self.record("fallocate", b"", Some(actor.caller));
        Ok(())
    }

    fn lseek(&self, _fh: u64, offset: i64, _whence: i32) -> OpResult<i64> {
        self.record("lseek", b"", None);
        Ok(offset)
    }

    fn copy_file_range(
        &self,
        _fh_in: u64,
        _offset_in: u64,
        _fh_out: u64,
        _offset_out: u64,
        len: u64,
        actor: Actor<'_>,
    ) -> OpResult<u32> {
        self.record("copy_file_range", b"", Some(actor.caller));
        Ok(u32::try_from(len).unwrap_or(u32::MAX))
    }

    fn mapping_may_write(&self, _id: u64) {
        self.record("mapping_may_write", b"", None);
    }

    fn opendir(&self, _id: u64) -> OpResult<u64> {
        self.record("opendir", b"", None);
        Ok(1)
    }

    /// Every child of `id`, after `.` and `..`, unfiltered.
    fn readdir(&self, id: u64, _fh: u64, offset: u64) -> OpResult<Vec<(u64, DirEntry)>> {
        self.record("readdir", b"", None);
        let mut entries = vec![
            DirEntry {
                id,
                kind: FileType::Directory,
                name: b".".to_vec(),
            },
            DirEntry {
                id: ROOT,
                kind: FileType::Directory,
                name: b"..".to_vec(),
            },
        ];
        let mut children: Vec<_> = self
            .children
            .iter()
            .filter(|((parent, _), _)| *parent == id)
            .map(|((_, name), child)| DirEntry {
                id: *child,
                kind: self.attrs[child].kind,
                name: name.clone(),
            })
            .collect();
        children.sort_by(|a, b| a.name.cmp(&b.name));
        entries.extend(children);
        Ok(entries
            .into_iter()
            .enumerate()
            .skip(usize::try_from(offset).unwrap())
            .map(|(i, e)| (i as u64 + 1, e))
            .collect())
    }

    fn releasedir(&self, _fh: u64) {
        self.record("releasedir", b"", None);
    }

    fn statfs(&self) -> OpResult<rustix::fs::StatVfs> {
        self.record("statfs", b"", None);
        Err(Errno::ENOSYS)
    }

    fn setxattr(&self, _id: u64, name: &[u8], _value: &[u8], _flags: i32) -> OpResult<()> {
        self.record("setxattr", name, None);
        Ok(())
    }

    fn getxattr(&self, _id: u64, name: &[u8], _size: u32) -> OpResult<Xattr> {
        self.record("getxattr", name, None);
        Ok(Xattr::Size(0))
    }

    /// Every node has `user.a`, `trusted.b`, `security.c` and `user.d`.
    fn listxattr(&self, _id: u64, size: u32) -> OpResult<Xattr> {
        self.record("listxattr", b"", None);
        let list = FAKE_XATTRS.to_vec();
        let len = u32::try_from(list.len()).unwrap();
        match size {
            0 => Ok(Xattr::Size(len)),
            n if n < len => Err(Errno::ERANGE),
            _ => Ok(Xattr::Data(list)),
        }
    }

    fn removexattr(&self, _id: u64, name: &[u8]) -> OpResult<()> {
        self.record("removexattr", name, None);
        Ok(())
    }
}

impl TreeOperations for FakeOps {
    fn new_node_id(&self) -> u64 {
        1000 + self.next.fetch_add(1, Ordering::Relaxed)
    }

    fn new_handle_id(&self) -> u64 {
        1000 + self.next.fetch_add(1, Ordering::Relaxed)
    }

    /// Asks `check` about the scripted entry at the new name, then takes
    /// the file out of the store, as the real move would.
    fn move_in(
        &self,
        from: BorrowedFd<'_>,
        name: &[u8],
        newparent: u64,
        newname: &[u8],
        _flags: u32,
        moved: Moved,
        check: &dyn NameCheck,
        caller: Caller,
    ) -> OpResult<Entry> {
        self.record("move_in", newname, Some(caller));
        let path = join(&self.paths[&newparent], newname);
        let kind = self
            .children
            .get(&(newparent, newname.to_vec()))
            .map(|id| kind_of(self.attrs[id].kind));
        check.move_in(Found { path: &path, kind })?;
        rustix::fs::unlinkat(
            from,
            std::ffi::OsStr::from_bytes(name),
            rustix::fs::AtFlags::empty(),
        )
        .unwrap();
        Ok(Entry {
            id: moved.node,
            attr: attr(moved.node, FileType::RegularFile, 0o644, OWNER.uid),
        })
    }
}

/// This process's own ids as a view owner: the tmp layer's files are made
/// as the daemon is.
fn me() -> ViewOwner {
    let ids = OwnerIds {
        uid: rustix::process::geteuid().as_raw(),
        gid: rustix::process::getegid().as_raw(),
    };
    ViewOwner::new(ids, &Privileges::default(), ids.uid).unwrap()
}

/// Fixture B: the overlay over [`FakeOps`], with the examples' rules over
/// an empty tree (`_tree` keeps it for nested-repository probes) and a tmp
/// layer beside it whose files are this process's.
struct B {
    _tree: tempfile::TempDir,
    fake: Arc<FakeOps>,
    overlay: Overlay<FakeOps>,
    refused: Refusals,
}

/// The refusals a [`RefusalCounter`] counted, in order.
#[derive(Clone, Default)]
struct Refusals(Arc<Mutex<Vec<OverlayRefusal>>>);

impl Refusals {
    fn counter(&self) -> RefusalCounter {
        let seen = self.0.clone();
        Arc::new(move |reason| seen.lock().unwrap().push(reason))
    }

    /// What was counted since the last take.
    fn take(&self) -> Vec<OverlayRefusal> {
        std::mem::take(&mut *self.0.lock().unwrap())
    }
}

fn fixture_b(fake: FakeOps) -> B {
    fixture_b_with(fake, "", &[])
}

/// Fixture B with `attributes` in `info/attributes` and the directories
/// `dirs` in the tree (for nested-repository probes).
fn fixture_b_with(fake: FakeOps, attributes: &str, dirs: &[&str]) -> B {
    let tree = tempfile::tempdir().unwrap();
    for d in dirs {
        std::fs::create_dir_all(tree.path().join(d)).unwrap();
    }
    let rules = Rules::new(
        tree.path(),
        RuleOptions::default(),
        EXCLUDE.as_bytes(),
        attributes.as_bytes(),
    );
    let gate = Gate::new(Arc::new(rules)).unwrap();
    let fake = Arc::new(fake);
    let root = std::fs::File::open(tree.path()).unwrap();
    let tmp = TmpLayer::open(&tree.path().join("tmp-layer"), root.as_fd(), me()).unwrap();
    let refused = Refusals::default();
    B {
        _tree: tree,
        overlay: Overlay::new(
            fake.clone(),
            gate,
            owner(),
            Arc::new(tmp),
            refused.counter(),
        ),
        fake,
        refused,
    }
}

fn names(entries: &[(u64, DirEntry)]) -> Vec<String> {
    entries
        .iter()
        .map(|(_, e)| String::from_utf8(e.name.clone()).unwrap())
        .collect()
}

#[test]
fn a4_listing_shows_only_shown_names() {
    let b = fixture_b(FakeOps::examples());
    let fh = b.overlay.opendir(ROOT).unwrap();
    let listed = b.overlay.readdir(ROOT, fh, 0).unwrap();
    assert_eq!(names(&listed), [".", "..", "config.yaml", "skills"]);
    // Paging resumes where it left off.
    let rest = b.overlay.readdir(ROOT, fh, listed[2].0).unwrap();
    assert_eq!(names(&rest), ["skills"]);
}

#[test]
fn a5_hidden_names_answer_the_same_whether_or_not_they_exist() {
    let b = fixture_b(FakeOps::examples());
    for name in ["secrets.db", "nothing.db"] {
        let n = name.as_bytes();
        assert_eq!(
            b.overlay.lookup(ROOT, n).unwrap_err(),
            Errno::ENOENT,
            "{name}"
        );
        assert_eq!(
            b.overlay
                .unlink(ROOT, n, Actor::of_process(REQUEST))
                .unwrap_err(),
            Errno::ENOENT,
            "{name}"
        );
        assert_eq!(
            b.overlay
                .mkdir(ROOT, n, 0o755, Actor::of_process(REQUEST))
                .unwrap_err(),
            Errno::EACCES,
            "{name}"
        );
        // A file at a skipped name is a tmp file, whether or not the tree
        // holds one there.
        b.overlay
            .create(
                ROOT,
                n,
                0o644,
                O_WRONLY | O_EXCL,
                Actor::of_process(REQUEST),
            )
            .unwrap();
    }
    let asked: Vec<Call> = b
        .fake
        .calls()
        .into_iter()
        .filter(|c| ["lookup", "unlink", "create", "mkdir"].contains(&c.method))
        .collect();
    assert!(
        asked.is_empty(),
        "the operations were asked about a hidden name: {asked:?}"
    );
}

/// The examples, plus entries the gate hides for their kind at names it
/// places: a FIFO at a recordable name (`skills/pipe`), a file the
/// attributes never record at a name placeable as a directory
/// (`skills/x.sec`, under `/skills/*.sec -agentfs`) and a nested
/// repository at a file-recordable name (`skills/repo`).
fn hidden_kinds() -> B {
    fixture_b_with(
        FakeOps::examples()
            .node(8, 5, "pipe", FileType::NamedPipe, 0o666, OWNER.uid)
            .node(9, 5, "x.sec", FileType::RegularFile, 0o666, OWNER.uid)
            .node(10, 5, "repo", FileType::Directory, 0o777, OWNER.uid),
        "/skills/*.sec -agentfs\n",
        &["skills/repo/.git"],
    )
}

#[test]
fn a5_hidden_entries_at_placeable_names_are_refused_under_the_names_ordering() {
    let b = hidden_kinds();
    let s_ifreg = 0o100000;
    // Each answers as a refused name: never EEXIST, EISDIR or a replace.
    for name in ["pipe", "x.sec", "repo"] {
        let n = name.as_bytes();
        assert_eq!(
            b.overlay
                .mkdir(5, n, 0o755, Actor::of_process(REQUEST))
                .unwrap_err(),
            Errno::EACCES,
            "mkdir {name}"
        );
        assert_eq!(
            b.overlay
                .rename(5, b"a.md", 5, n, 0, Actor::of_process(REQUEST))
                .unwrap_err(),
            Errno::EACCES,
            "rename onto {name}"
        );
    }
    for name in ["pipe", "repo"] {
        let n = name.as_bytes();
        assert_eq!(
            b.overlay
                .create(5, n, 0o644, O_WRONLY, Actor::of_process(REQUEST))
                .unwrap_err(),
            Errno::EACCES,
            "create {name}"
        );
        assert_eq!(
            b.overlay
                .mknod(5, n, s_ifreg | 0o644, 0, Actor::of_process(REQUEST))
                .unwrap_err(),
            Errno::EACCES,
            "mknod {name}"
        );
    }
    // Decided under the names ordering, on what the operations found
    // there: every refusal is the Judge's, counted once as hidden.
    assert_eq!(b.refused.take().len(), 10);
    // The same names with nothing behind them go on.
    let b = fixture_b_with(
        FakeOps::examples(),
        "/skills/*.sec -agentfs\n",
        &["skills/repo/.git"],
    );
    b.overlay
        .mkdir(5, b"y.sec", 0o755, Actor::of_process(REQUEST))
        .unwrap();
    b.overlay
        .rename(5, b"a.md", 5, b"pipe", 0, Actor::of_process(REQUEST))
        .unwrap();
    b.overlay
        .create(5, b"pipe", 0o644, O_WRONLY, Actor::of_process(REQUEST))
        .unwrap();
    b.overlay
        .mknod(5, b"pipe2", s_ifreg | 0o644, 0, Actor::of_process(REQUEST))
        .unwrap();
}

#[test]
fn rename_exchange_needs_both_ends_shown_in_the_tree() {
    let b = hidden_kinds();
    let exchange = RenameFlags::EXCHANGE.bits();
    // A hidden kind at the other end: refused, so it never moves.
    assert_ne!(
        b.overlay
            .rename(5, b"a.md", 5, b"pipe", exchange, Actor::of_process(REQUEST)),
        Ok(())
    );
    // A directory at the other end never moves.
    assert_eq!(
        b.overlay
            .rename(
                ROOT,
                b"config.yaml",
                5,
                b"d",
                exchange,
                Actor::of_process(REQUEST)
            )
            .unwrap_err(),
        Errno::EXDEV
    );
    // Nothing to exchange with.
    assert_eq!(
        b.overlay
            .rename(5, b"a.md", 5, b"b.md", exchange, Actor::of_process(REQUEST))
            .unwrap_err(),
        Errno::ENOENT
    );
    assert!(b.fake.calls_to("rename").is_empty());
    b.overlay
        .rename(
            5,
            b"a.md",
            ROOT,
            b"config.yaml",
            exchange,
            Actor::of_process(REQUEST),
        )
        .unwrap();
    assert_eq!(b.fake.calls_to("rename").len(), 1);
}

#[test]
fn rename_onto_an_entry_follows_the_sticky_rule() {
    // A sticky directory of another user's, holding their file.
    let b = fixture_b(
        FakeOps::examples()
            .node(8, 5, "shared", FileType::Directory, 0o1777, OTHER)
            .node(9, 8, "theirs.md", FileType::RegularFile, 0o666, OTHER),
    );
    assert_eq!(
        b.overlay
            .rename(5, b"a.md", 8, b"theirs.md", 0, Actor::of_process(REQUEST))
            .unwrap_err(),
        Errno::EACCES
    );
    assert_eq!(
        b.overlay
            .unlink(8, b"theirs.md", Actor::of_process(REQUEST))
            .unwrap_err(),
        Errno::EACCES
    );
    assert!(b.fake.calls_to("rename").is_empty());
    assert!(b.fake.calls_to("unlink").is_empty());
    // A new name there is the owner's to add.
    b.overlay
        .rename(5, b"a.md", 8, b"mine.md", 0, Actor::of_process(REQUEST))
        .unwrap();
    assert_eq!(b.fake.calls_to("rename").len(), 1);
}

#[test]
fn create_checks_an_existing_file_only_when_it_opens_it() {
    let b = fixture_b(
        FakeOps::examples()
            .node(8, 5, "theirs.md", FileType::RegularFile, 0o644, OTHER)
            .node(9, 5, "dir", FileType::Directory, 0o755, OTHER),
    );
    // O_EXCL opens nothing: the operations answer for the existing file.
    b.overlay
        .create(
            5,
            b"theirs.md",
            0o644,
            O_WRONLY | O_EXCL,
            Actor::of_process(REQUEST),
        )
        .unwrap();
    // A directory is not opened as a file: the operations answer.
    b.overlay
        .create(5, b"dir", 0o644, O_WRONLY, Actor::of_process(REQUEST))
        .unwrap();
    assert_eq!(b.fake.calls_to("create").len(), 2);
}

#[test]
fn setattr_through_the_overlay() {
    let b =
        fixture_b(FakeOps::examples().node(8, 5, "theirs.md", FileType::RegularFile, 0o644, OTHER));
    let chgrp = SetAttr {
        gid: Some(0),
        ..SetAttr::default()
    };
    assert_eq!(
        b.overlay
            .setattr(2, &chgrp, Actor::of_process(REQUEST))
            .unwrap_err(),
        Errno::EPERM
    );
    // A size without a handle needs the owner's write access.
    let truncate = SetAttr {
        size: Some(0),
        ..SetAttr::default()
    };
    assert_eq!(
        b.overlay
            .setattr(8, &truncate, Actor::of_process(REQUEST))
            .unwrap_err(),
        Errno::EACCES
    );
    assert!(b.fake.calls_to("setattr").is_empty());
    b.overlay
        .setattr(6, &truncate, Actor::of_process(REQUEST))
        .unwrap();
    assert_eq!(b.fake.calls_to("setattr").len(), 1);
    // Through a handle, gated when it was opened: a hidden node too, and
    // no write check (ftruncate on a descriptor opened to write).
    let through = SetAttr {
        size: Some(0),
        fh: Some(1),
        ..SetAttr::default()
    };
    assert_eq!(b.overlay.getattr(3, None).unwrap_err(), Errno::ENOENT);
    b.overlay
        .setattr(3, &through, Actor::of_process(REQUEST))
        .unwrap();
    b.overlay
        .setattr(8, &through, Actor::of_process(REQUEST))
        .unwrap();
    assert_eq!(b.fake.calls_to("setattr").len(), 3);
}

#[test]
fn mknod_of_a_regular_file_is_a_create_as_the_owner() {
    let b = fixture_b(FakeOps::examples());
    let s_ifreg = 0o100000;
    b.overlay
        .mknod(
            5,
            b"new.md",
            s_ifreg | 0o4644,
            0,
            Actor::of_process(REQUEST),
        )
        .unwrap();
    // A type of 0 is a regular file too.
    b.overlay
        .mknod(5, b"new2.md", 0o644, 0, Actor::of_process(REQUEST))
        .unwrap();
    let calls = b.fake.calls_to("mknod");
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].caller, Some(owner().caller(REQUEST.pid)));
    // A skipped name goes to the tmp layer, never the operations.
    let tmp = b
        .overlay
        .mknod(
            ROOT,
            b"notes.txt",
            s_ifreg | 0o4644,
            0,
            Actor::of_process(REQUEST),
        )
        .unwrap();
    assert_eq!(tmp.attr.perm, 0o644);
    assert_eq!(
        b.overlay
            .mknod(
                ROOT,
                b"notes.txt",
                s_ifreg | 0o644,
                0,
                Actor::of_process(REQUEST)
            )
            .unwrap_err(),
        Errno::EEXIST
    );
    assert_eq!(b.fake.calls_to("mknod").len(), 2);
}

#[test]
fn rmdir_through_the_overlay() {
    let b = fixture_b(FakeOps::examples());
    assert_eq!(
        b.overlay
            .rmdir(ROOT, b"notes", Actor::of_process(REQUEST))
            .unwrap_err(),
        Errno::ENOENT
    );
    assert_eq!(
        b.overlay
            .rmdir(ROOT, b"nothing", Actor::of_process(REQUEST))
            .unwrap_err(),
        Errno::ENOENT
    );
    assert!(b.fake.calls_to("rmdir").is_empty());
    b.overlay
        .rmdir(5, b"d", Actor::of_process(REQUEST))
        .unwrap();
    assert_eq!(
        b.fake.calls_to("rmdir")[0].caller,
        Some(owner().caller(REQUEST.pid))
    );
}

#[test]
fn lookup_needs_the_owner_to_search_the_parent() {
    let b = fixture_b(
        FakeOps::examples()
            .node(8, 5, "private", FileType::Directory, 0o700, OTHER)
            .node(9, 8, "x.md", FileType::RegularFile, 0o644, OTHER),
    );
    assert_eq!(b.overlay.lookup(8, b"x.md").unwrap_err(), Errno::EACCES);
    assert!(b.fake.calls_to("lookup").is_empty());
}

#[test]
fn names_that_are_not_utf8_are_hidden() {
    let b = fixture_b(FakeOps::examples());
    let bad: &[u8] = b"x\xff.md";
    assert_eq!(b.overlay.lookup(5, bad).unwrap_err(), Errno::ENOENT);
    assert_eq!(
        b.overlay
            .create(5, bad, 0o644, O_WRONLY, Actor::of_process(REQUEST))
            .unwrap_err(),
        Errno::EACCES
    );
    assert_eq!(
        b.overlay
            .rename(5, bad, 5, b"b.md", 0, Actor::of_process(REQUEST))
            .unwrap_err(),
        Errno::ENOENT
    );
    assert_eq!(
        b.overlay
            .rename(5, b"a.md", 5, bad, 0, Actor::of_process(REQUEST))
            .unwrap_err(),
        Errno::EACCES
    );
    assert!(b.fake.calls_to("lookup").is_empty());
    assert!(b.fake.calls_to("create").is_empty());
    assert!(b.fake.calls_to("rename").is_empty());
    // A node whose only path is not UTF-8 is hidden too.
    let b = fixture_b(FakeOps::examples().node(
        8,
        5,
        "y\u{fffd}.md",
        FileType::RegularFile,
        0o644,
        OWNER.uid,
    ));
    let mut fake = FakeOps::examples();
    fake.attrs
        .insert(8, attr(8, FileType::RegularFile, 0o644, OWNER.uid));
    fake.paths.insert(8, b"skills/y\xff.md".to_vec());
    let raw = fixture_b(fake);
    assert_eq!(raw.overlay.getattr(8, None).unwrap_err(), Errno::ENOENT);
    // Its UTF-8 twin is shown.
    b.overlay.getattr(8, None).unwrap();
}

#[test]
fn rules_errors_answer_eio_through_the_overlay() {
    // Longer than any file name the host allows: probing `<long>/.git`
    // for a nested repository fails with ENAMETOOLONG.
    // (`skills` exists in the tree, so the probe reaches the long name.)
    let long = "d".repeat(300);
    let b = fixture_b_with(
        FakeOps::examples()
            .node(8, ROOT, &long, FileType::Directory, 0o755, OWNER.uid)
            .node(9, 8, "x", FileType::RegularFile, 0o644, OWNER.uid)
            .node(10, 5, &long, FileType::Directory, 0o755, OWNER.uid),
        "",
        &["skills"],
    );
    // Every path of the node fails to decide: EIO, not ENOENT.
    assert_eq!(b.overlay.getattr(9, None).unwrap_err(), Errno::EIO);
    assert_eq!(
        b.overlay.lookup(8, b"x").unwrap_err(),
        Errno::EIO,
        "its parent"
    );
    // `skills/<long>` is recordable as a file, but as a directory the
    // rules cannot decide: `may_show` fails closed, so the name is
    // answered without asking the operations.
    assert_eq!(b.overlay.getattr(10, None).unwrap_err(), Errno::EIO);
    assert_eq!(
        b.overlay.lookup(5, long.as_bytes()).unwrap_err(),
        Errno::EIO
    );
    assert!(b.fake.calls_to("lookup").is_empty());
    assert_eq!(
        b.overlay
            .mkdir(
                5,
                format!("{long}x").as_bytes(),
                0o755,
                Actor::of_process(REQUEST)
            )
            .unwrap_err(),
        Errno::EIO
    );
    assert!(b.fake.calls_to("mkdir").is_empty());
}

#[test]
fn a_lookup_that_finds_a_hidden_kind_gives_its_lookup_back() {
    // A FIFO at a recordable name: may_show says yes, its kind says no.
    let b =
        fixture_b(FakeOps::examples().node(8, 5, "fifo", FileType::NamedPipe, 0o644, OWNER.uid));
    assert_eq!(b.overlay.lookup(5, b"fifo").unwrap_err(), Errno::ENOENT);
    assert_eq!(b.fake.calls_to("forget").len(), 1);
    // A shown one keeps it.
    assert_eq!(b.overlay.lookup(5, b"a.md").unwrap().id, 6);
    assert_eq!(b.fake.calls_to("forget").len(), 1);
}

#[test]
fn a7_the_view_owner_acts_for_every_caller() {
    let b = fixture_b(FakeOps::examples());
    b.overlay
        .create(5, b"new.md", 0o644, O_WRONLY, Actor::of_process(REQUEST))
        .unwrap();
    assert_eq!(
        b.fake.calls_to("create")[0].caller,
        Some(Caller {
            uid: OWNER.uid,
            gid: OWNER.gid,
            pid: REQUEST.pid,
        })
    );
    // Handle operations get the owner too.
    b.overlay
        .write(1, 0, b"x", Actor::of_process(REQUEST))
        .unwrap();
    b.overlay.flush(1, Actor::of_process(REQUEST)).unwrap();
    for method in ["write", "flush"] {
        assert_eq!(
            b.fake.calls_to(method)[0].caller,
            Some(owner().caller(REQUEST.pid)),
            "{method}"
        );
    }

    // A file the owner may only read: no write, whoever asks.
    let b =
        fixture_b(FakeOps::examples().node(8, 5, "theirs.md", FileType::RegularFile, 0o644, OTHER));
    let id = b.overlay.lookup(5, b"theirs.md").unwrap().id;
    let root = Caller {
        uid: 0,
        gid: 0,
        pid: 1,
    };
    for caller in [REQUEST, root] {
        assert_eq!(
            b.overlay
                .open(id, O_WRONLY, Actor::of_process(caller))
                .unwrap_err(),
            Errno::EACCES
        );
    }
    assert!(b.fake.calls_to("open").is_empty());
    b.overlay
        .open(id, O_RDONLY, Actor::of_process(REQUEST))
        .unwrap();
    assert_eq!(b.fake.calls_to("open").len(), 1);
    // Nor through access(2). A create that would open it is passed on
    // as the owner with no supplementary groups: the operations check the
    // owner's access to what they find there, under the names ordering
    // (ops::tests::an_existing_file_is_opened_by_create_only_with_access).
    assert_eq!(b.overlay.access(id, 2).unwrap_err(), Errno::EACCES);
    b.overlay
        .create(5, b"theirs.md", 0o644, O_WRONLY, Actor::of_process(REQUEST))
        .unwrap();
    assert_eq!(
        b.fake.calls_to("create")[0].caller,
        Some(owner().caller(REQUEST.pid))
    );
}

#[test]
fn a8_refusals() {
    let b = fixture_b(FakeOps::examples());
    let s_ififo = 0o010000;
    assert_eq!(
        b.overlay
            .symlink(5, b"l", b"config.yaml", Actor::of_process(REQUEST))
            .unwrap_err(),
        Errno::EACCES
    );
    assert_eq!(
        b.overlay
            .link(2, 5, b"c.yaml", Actor::of_process(REQUEST))
            .unwrap_err(),
        Errno::EACCES
    );
    assert_eq!(
        b.overlay
            .mknod(5, b"f", s_ififo | 0o644, 0, Actor::of_process(REQUEST))
            .unwrap_err(),
        Errno::EACCES
    );
    for method in ["symlink", "link", "mknod"] {
        assert!(b.fake.calls_to(method).is_empty(), "{method}");
    }

    let chown = SetAttr {
        uid: Some(0),
        ..SetAttr::default()
    };
    assert_eq!(
        b.overlay
            .setattr(2, &chown, Actor::of_process(REQUEST))
            .unwrap_err(),
        Errno::EPERM
    );
    let setuid = SetAttr {
        mode: Some(0o4755),
        ..SetAttr::default()
    };
    assert_eq!(
        b.overlay
            .setattr(2, &setuid, Actor::of_process(REQUEST))
            .unwrap_err(),
        Errno::EPERM
    );
    assert!(b.fake.calls_to("setattr").is_empty());
    // A plain mode by the object's owner goes on.
    let chmod = SetAttr {
        mode: Some(0o755),
        ..SetAttr::default()
    };
    b.overlay
        .setattr(2, &chmod, Actor::of_process(REQUEST))
        .unwrap();
    assert_eq!(b.fake.calls_to("setattr").len(), 1);

    assert_eq!(
        b.overlay.setxattr(2, b"user.x", b"1", 0).unwrap_err(),
        Errno::ENOTSUP
    );
    assert_eq!(
        b.overlay.removexattr(2, b"user.x").unwrap_err(),
        Errno::ENOTSUP
    );

    assert_eq!(
        b.overlay
            .rename(5, b"d", 5, b"e", 0, Actor::of_process(REQUEST))
            .unwrap_err(),
        Errno::EXDEV
    );
    assert_eq!(
        b.overlay
            .rename(
                ROOT,
                b"config.yaml",
                ROOT,
                b"config.bak",
                0,
                Actor::of_process(REQUEST)
            )
            .unwrap_err(),
        Errno::EACCES
    );
    let whiteout = RenameFlags::WHITEOUT.bits();
    assert_eq!(
        b.overlay
            .rename(5, b"a.md", 5, b"b.md", whiteout, Actor::of_process(REQUEST))
            .unwrap_err(),
        Errno::EINVAL
    );
    assert!(b.fake.calls_to("rename").is_empty());
    // A file moved to a recordable name goes on, as the owner.
    b.overlay
        .rename(
            ROOT,
            b"config.yaml",
            5,
            b"config.yaml",
            0,
            Actor::of_process(REQUEST),
        )
        .unwrap();
    assert_eq!(
        b.fake.calls_to("rename")[0].caller,
        Some(owner().caller(REQUEST.pid))
    );

    // A skipped name goes to the tmp layer, never the operations.
    b.overlay
        .create(
            ROOT,
            b"notes.txt",
            0o644,
            O_WRONLY,
            Actor::of_process(REQUEST),
        )
        .unwrap();
    assert!(b.fake.calls_to("create").is_empty());
}

#[test]
fn a9_owner_ids_parse_and_the_daemon_must_be_able_to_act_as_them() {
    assert_eq!(
        "10000:10000".parse::<OwnerIds>(),
        Ok(OwnerIds {
            uid: 10000,
            gid: 10000
        })
    );
    for bad in ["a:b", "1", "1:2:3", "-1:0", "", "+1:0", ":1", "1:"] {
        assert!(bad.parse::<OwnerIds>().is_err(), "{bad:?}");
    }
    assert_eq!(OWNER.to_string(), "10000:10000");

    let err = ViewOwner::new(OWNER, &Privileges::default(), 1000).unwrap_err();
    assert!(matches!(
        err,
        OwnerError::CannotActAs {
            uid: 10000,
            gid: 10000,
            euid: 1000
        }
    ));
    assert!(
        err.to_string().contains("--sandbox-owner 10000:10000"),
        "{err}"
    );
    let owner = ViewOwner::new(OWNER, &Privileges::default(), OWNER.uid).unwrap();
    assert_eq!((owner.uid(), owner.gid()), (OWNER.uid, OWNER.gid));
    let all = Privileges {
        effective: u64::MAX,
        initial_userns: true,
    };
    ViewOwner::new(OWNER, &all, 0).unwrap();
}

#[test]
fn errnos_for_hidden_names_and_placements() {
    assert_eq!(hidden_errno(Hidden::NotRecordable), Errno::ENOENT);
    assert_eq!(hidden_errno(Hidden::RulesError), Errno::EIO);
    assert_eq!(admit(Placement::Tree), Ok(()));
    assert_eq!(admit(Placement::TmpLayer), Err(Errno::EACCES));
    assert_eq!(admit(Placement::CrossDevice), Err(Errno::EXDEV));
    assert_eq!(
        admit(Placement::Refused(Denial::SourceHidden)),
        Err(Errno::ENOENT)
    );
    assert_eq!(
        admit(Placement::Refused(Denial::RulesError)),
        Err(Errno::EIO)
    );
    for denial in [Denial::Kind, Denial::NotRecordable] {
        assert_eq!(admit(Placement::Refused(denial)), Err(Errno::EACCES));
    }
}

#[test]
fn times_follow_utimensat() {
    let b =
        fixture_b(FakeOps::examples().node(8, 5, "theirs.md", FileType::RegularFile, 0o666, OTHER));
    let now = SetAttr {
        atime: Some(TimeOrNow::Now),
        mtime: Some(TimeOrNow::Now),
        ..SetAttr::default()
    };
    // Not the owner's, but writable: touching is allowed.
    b.overlay
        .setattr(8, &now, Actor::of_process(REQUEST))
        .unwrap();
    let explicit = SetAttr {
        mtime: Some(TimeOrNow::SpecificTime(UNIX_EPOCH)),
        ..SetAttr::default()
    };
    assert_eq!(
        b.overlay
            .setattr(8, &explicit, Actor::of_process(REQUEST))
            .unwrap_err(),
        Errno::EPERM
    );
    // A mode change needs the owner to own the object.
    let chmod = SetAttr {
        mode: Some(0o600),
        ..SetAttr::default()
    };
    assert_eq!(
        b.overlay
            .setattr(8, &chmod, Actor::of_process(REQUEST))
            .unwrap_err(),
        Errno::EPERM
    );
    // On the owner's own file, explicit times are fine.
    b.overlay
        .setattr(6, &explicit, Actor::of_process(REQUEST))
        .unwrap();
}

/// Fixture A: a real [`Ops`] on a temporary directory, with the overlay
/// in front of it, the owner being this process's own ids.
struct A {
    dir: tempfile::TempDir,
    ops: Arc<Ops>,
    overlay: Overlay<Ops>,
    refused: Refusals,
}

fn fixture_a(files: &[&str], dirs: &[&str]) -> A {
    let dir = tempfile::tempdir().unwrap();
    let backing = dir.path().join("backing");
    std::fs::create_dir_all(&backing).unwrap();
    for d in dirs {
        std::fs::create_dir_all(backing.join(d)).unwrap();
    }
    for f in files {
        let path = backing.join(f);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, *f).unwrap();
    }
    let state = StateDir::new(dir.path().join("state"));
    volume::init(&state, "vol", None).unwrap();
    let info = state.git().join("info");
    std::fs::create_dir_all(&info).unwrap();
    std::fs::write(info.join("exclude"), EXCLUDE).unwrap();
    let (recorder, _committer, _) = volume::open(
        &state,
        &backing,
        Context {
            volume: "vol".into(),
            replica: "pod-0".into(),
            pod_uid: "u".into(),
            ..Context::default()
        },
        Arc::new(MemorySink::new()),
        &Telemetry::noop(),
    )
    .unwrap();
    let gate = Gate::new(recorder.rules().clone()).unwrap();
    let ops = Arc::new(
        Ops::new(
            Backing::open(&backing).unwrap(),
            recorder,
            OpsConfig::default(),
        )
        .unwrap(),
    );
    let ids = OwnerIds {
        uid: rustix::process::geteuid().as_raw(),
        gid: rustix::process::getegid().as_raw(),
    };
    let owner = ViewOwner::new(ids, &Privileges::default(), ids.uid).unwrap();
    let tmp =
        Arc::new(TmpLayer::open(&state.tmp_layer(), ops.backing().root().as_fd(), owner).unwrap());
    let refused = Refusals::default();
    A {
        overlay: Overlay::new(ops.clone(), gate, owner, tmp, refused.counter()),
        ops,
        dir,
        refused,
    }
}

#[test]
fn a4_listing_over_a_real_tree() {
    let a = fixture_a(&["config.yaml", "secrets.db", "skills/a.md"], &["notes"]);
    let fh = a.overlay.opendir(ROOT).unwrap();
    let listed = a.overlay.readdir(ROOT, fh, 0).unwrap();
    a.overlay.releasedir(fh);
    let mut got = names(&listed);
    got.sort();
    assert_eq!(got, [".", "..", "config.yaml", "skills"]);
    // The main mount's view is whole.
    let fh = a.ops.opendir(ROOT).unwrap();
    let mut all = names(&a.ops.readdir(fh, 0).unwrap());
    all.sort();
    assert_eq!(
        all,
        [".", "..", "config.yaml", "notes", "secrets.db", "skills"]
    );
    assert!(a.dir.path().join("backing/notes").is_dir());
}

#[test]
fn a6_a_node_moved_out_of_view_is_gone_from_the_overlay() {
    let a = fixture_a(&["config.yaml"], &[]);
    let id = a.overlay.lookup(ROOT, b"config.yaml").unwrap().id;
    a.overlay.getattr(id, None).unwrap();
    // The main mount renames it to a skipped name.
    let main = Caller {
        uid: 0,
        gid: 0,
        pid: 1,
    };
    a.ops
        .rename(
            ROOT,
            b"config.yaml",
            ROOT,
            b"secrets2.db",
            0,
            Actor::of_process(main),
        )
        .unwrap();
    assert_eq!(a.overlay.getattr(id, None).unwrap_err(), Errno::ENOENT);
    assert_eq!(
        a.overlay
            .open(id, O_RDONLY, Actor::of_process(REQUEST))
            .unwrap_err(),
        Errno::ENOENT
    );
    assert_eq!(a.overlay.access(id, 4).unwrap_err(), Errno::ENOENT);
}

#[test]
fn a14_a_close_through_the_overlay_records_before_it_returns() {
    let a = fixture_a(&["config.yaml"], &[]);
    let journal = || {
        agentfs_core::journal::read_records(&StateDir::new(a.dir.path().join("state")).journal())
            .unwrap()
            .0
    };
    let before = journal().len();
    let id = a.overlay.lookup(ROOT, b"config.yaml").unwrap().id;
    let (fh, _handle) = a
        .overlay
        .open(id, O_WRONLY, Actor::of_process(REQUEST))
        .unwrap();
    a.overlay
        .write(fh, 0, b"v2", Actor::of_process(REQUEST))
        .unwrap();
    a.overlay.flush(fh, Actor::of_process(REQUEST)).unwrap();
    // The close's own record, not a later release's or sweep's.
    let records = journal();
    assert_eq!(records.len(), before + 1, "{records:#?}");
    let record = records.last().unwrap();
    assert_eq!(record.draft.trigger, Trigger::Close);
    let owner_uid = rustix::process::geteuid().as_raw();
    assert_eq!(record.draft.caller.uid, owner_uid);
    assert_eq!(record.draft.caller.pid, REQUEST.pid);
    a.overlay.release(fh).unwrap();
}

// ---- the tmp layer (sandbox overlay slice 3) ----

impl A {
    fn journal(&self) -> Vec<agentfs_core::record::Record> {
        agentfs_core::journal::read_records(&StateDir::new(self.dir.path().join("state")).journal())
            .unwrap()
            .0
    }

    fn backing(&self, rel: &str) -> std::path::PathBuf {
        self.dir.path().join("backing").join(rel)
    }

    /// The names in the tmp layer's store.
    fn stored(&self) -> Vec<String> {
        std::fs::read_dir(self.dir.path().join("state/tmp-layer"))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect()
    }

    /// Every path any record names.
    fn recorded_paths(&self) -> Vec<String> {
        self.journal()
            .iter()
            .flat_map(|r| r.draft.changes.iter().flat_map(|c| c.paths()))
            .map(|p| p.as_str().to_owned())
            .collect()
    }

    /// The main mount's names in directory `id`, sorted.
    fn main_listing(&self, id: u64) -> Vec<String> {
        let fh = self.ops.opendir(id).unwrap();
        let mut all = names(&self.ops.readdir(fh, 0).unwrap());
        self.ops.releasedir(fh);
        all.sort();
        all
    }

    /// Creates `name` in `parent` through the overlay with `content`, and
    /// closes it.
    fn put(&self, parent: u64, name: &str, content: &str) -> u64 {
        let (entry, fh, _) = self
            .overlay
            .create(
                parent,
                name.as_bytes(),
                0o644,
                O_WRONLY | O_EXCL,
                Actor::of_process(REQUEST),
            )
            .unwrap();
        self.overlay
            .write(fh, 0, content.as_bytes(), Actor::of_process(REQUEST))
            .unwrap();
        self.overlay.flush(fh, Actor::of_process(REQUEST)).unwrap();
        self.overlay.release(fh).unwrap();
        entry.id
    }

    fn read(&self, id: u64) -> String {
        let (fh, _) = self
            .overlay
            .open(id, O_RDONLY, Actor::of_process(REQUEST))
            .unwrap();
        let bytes = self.overlay.read(fh, 0, 100).unwrap();
        self.overlay.release(fh).unwrap();
        String::from_utf8(bytes).unwrap()
    }
}

#[test]
fn tmp_files_are_private_and_shadow_hidden_names() {
    let a = fixture_a(&["config.yaml", "secrets.db"], &["skills"]);
    let records = a.journal().len();
    // A hidden tree name: the tmp file is created over it, exclusively.
    let (entry, fh, _) = a
        .overlay
        .create(
            ROOT,
            b"secrets.db",
            0o644,
            O_RDWR | O_EXCL,
            Actor::of_process(REQUEST),
        )
        .unwrap();
    a.overlay
        .write(fh, 0, b"tmp", Actor::of_process(REQUEST))
        .unwrap();
    a.overlay.fsync(fh, false).unwrap();
    a.overlay.flush(fh, Actor::of_process(REQUEST)).unwrap();
    a.overlay.release(fh).unwrap();
    a.put(ROOT, "new.txt", "n");
    let skill_dir = a.overlay.lookup(ROOT, b"skills").unwrap().id;
    a.put(skill_dir, "x.key", "k");

    // The overlay sees its tmp file: the upper layer wins.
    let found = a.overlay.lookup(ROOT, b"secrets.db").unwrap();
    assert_eq!(found.id, entry.id);
    assert_eq!(a.overlay.getattr(entry.id, None).unwrap().size, 3);
    assert_eq!(a.read(entry.id), "tmp");
    assert_eq!(a.overlay.access(entry.id, 6), Ok(()));
    assert!(a.overlay.paths(entry.id).is_empty());
    assert_eq!(a.overlay.readlink(entry.id).unwrap_err(), Errno::EINVAL);
    assert_eq!(
        a.overlay.getxattr(entry.id, b"user.x", 0).unwrap_err(),
        Errno::ENODATA
    );
    assert_eq!(a.overlay.listxattr(entry.id, 0), Ok(Xattr::Size(0)));
    // Nothing reached the tree, the main mount or history.
    assert_eq!(
        std::fs::read_to_string(a.backing("secrets.db")).unwrap(),
        "secrets.db"
    );
    assert!(!a.backing("new.txt").exists());
    assert!(!a.backing("skills/x.key").exists());
    assert_eq!(
        a.main_listing(ROOT),
        [".", "..", "config.yaml", "secrets.db", "skills"]
    );
    assert_eq!(a.journal().len(), records, "nothing recorded");
    assert_eq!(a.stored().len(), 3);

    // Unlinking removes only the tmp file: the tree's is hidden again.
    a.overlay
        .unlink(ROOT, b"secrets.db", Actor::of_process(REQUEST))
        .unwrap();
    assert_eq!(
        a.overlay.lookup(ROOT, b"secrets.db").unwrap_err(),
        Errno::ENOENT
    );
    assert_eq!(
        std::fs::read_to_string(a.backing("secrets.db")).unwrap(),
        "secrets.db"
    );
    assert_eq!(a.stored().len(), 2);
    assert_eq!(a.journal().len(), records);
}

#[test]
fn a_tmp_file_renamed_onto_a_recordable_name_is_recorded_there() {
    let a = fixture_a(&["config.yaml"], &[]);
    let tmp = a.put(ROOT, "config.yaml.tmp", "new");
    let before = a.journal().len();
    a.overlay
        .rename(
            ROOT,
            b"config.yaml.tmp",
            ROOT,
            b"config.yaml",
            0,
            Actor::of_process(REQUEST),
        )
        .unwrap();
    let records = a.journal();
    assert_eq!(records.len(), before + 1, "{records:#?}");
    let record = records.last().unwrap();
    assert_eq!(record.draft.trigger, Trigger::Rename);
    assert_eq!(record.draft.caller.pid, REQUEST.pid);
    assert_eq!(record.draft.caller.uid, rustix::process::geteuid().as_raw());
    let paths: Vec<&str> = record
        .draft
        .changes
        .iter()
        .flat_map(|c| c.paths())
        .map(|p| p.as_str())
        .collect();
    assert_eq!(paths, ["config.yaml"]);
    assert!(
        a.recorded_paths().iter().all(|p| !p.contains("tmp")),
        "{:?}",
        a.recorded_paths()
    );
    // The layer gave it up; the tree has it, under the same node id.
    assert!(a.stored().is_empty());
    assert_eq!(
        a.overlay.lookup(ROOT, b"config.yaml.tmp").unwrap_err(),
        Errno::ENOENT
    );
    assert_eq!(a.overlay.lookup(ROOT, b"config.yaml").unwrap().id, tmp);
    assert_eq!(a.ops.lookup(ROOT, b"config.yaml").unwrap().id, tmp);
    assert_eq!(
        std::fs::read_to_string(a.backing("config.yaml")).unwrap(),
        "new"
    );
    assert_eq!(a.overlay.paths(tmp), [b"config.yaml".to_vec()]);
}

#[test]
fn a_handle_open_across_the_move_writes_into_the_tree() {
    let a = fixture_a(&["config.yaml"], &[]);
    let (entry, fh, _) = a
        .overlay
        .create(
            ROOT,
            b"config.yaml.tmp",
            0o644,
            O_WRONLY | O_EXCL,
            Actor::of_process(REQUEST),
        )
        .unwrap();
    a.overlay
        .write(fh, 0, b"v1", Actor::of_process(REQUEST))
        .unwrap();
    a.overlay
        .rename(
            ROOT,
            b"config.yaml.tmp",
            ROOT,
            b"config.yaml",
            0,
            Actor::of_process(REQUEST),
        )
        .unwrap();
    a.overlay
        .write(fh, 2, b"+v2", Actor::of_process(REQUEST))
        .unwrap();
    let before = a.journal().len();
    a.overlay.flush(fh, Actor::of_process(REQUEST)).unwrap();
    let records = a.journal();
    assert_eq!(records.len(), before + 1, "the close records");
    assert_eq!(records.last().unwrap().draft.trigger, Trigger::Close);
    a.overlay.release(fh).unwrap();
    assert_eq!(a.ops.open_handles(), 0);
    assert_eq!(a.read(entry.id), "v1+v2");
}

#[test]
fn recorded_files_never_move_out() {
    let a = fixture_a(&["config.yaml", "skills/a.md"], &[]);
    let skills = a.overlay.lookup(ROOT, b"skills").unwrap().id;
    // A tree file to a skipped name: refused, a directory: cross-device.
    assert_eq!(
        a.overlay
            .rename(
                ROOT,
                b"config.yaml",
                ROOT,
                b"config.bak",
                0,
                Actor::of_process(REQUEST)
            )
            .unwrap_err(),
        Errno::EACCES
    );
    assert_eq!(
        a.overlay
            .rename(
                ROOT,
                b"skills",
                ROOT,
                b"skills2",
                0,
                Actor::of_process(REQUEST)
            )
            .unwrap_err(),
        Errno::EXDEV
    );
    // The judge answers the same under the names ordering.
    let judge = Judge::new(a.overlay.gate.clone(), Refusals::default().counter());
    let file = |path: &'static [u8]| Found {
        path,
        kind: Some(Kind::File),
    };
    let free = |path: &'static [u8]| Found { path, kind: None };
    assert_eq!(
        judge.rename(file(b"config.yaml"), free(b"config.bak"), 0),
        Err(Errno::EACCES)
    );
    assert!(a.backing("config.yaml").is_file());
    assert!(!a.backing("config.bak").exists());

    // A tmp file to another skipped name stays in the layer.
    let tmp = a.put(ROOT, "a.tmp", "t");
    a.overlay
        .rename(
            ROOT,
            b"a.tmp",
            skills,
            b"b.key",
            0,
            Actor::of_process(REQUEST),
        )
        .unwrap();
    assert_eq!(a.overlay.lookup(skills, b"b.key").unwrap().id, tmp);
    assert!(!a.backing("skills/b.key").exists());
    assert_eq!(a.stored().len(), 1);
    // An exchange with a tmp end is refused, either way round.
    let exchange = RenameFlags::EXCHANGE.bits();
    assert_eq!(
        a.overlay
            .rename(
                skills,
                b"b.key",
                ROOT,
                b"config.yaml",
                exchange,
                Actor::of_process(REQUEST)
            )
            .unwrap_err(),
        Errno::EACCES
    );
    assert_eq!(
        a.overlay
            .rename(
                ROOT,
                b"config.yaml",
                skills,
                b"b.key",
                exchange,
                Actor::of_process(REQUEST)
            )
            .unwrap_err(),
        Errno::EACCES
    );
    // No-replace holds in both layers.
    let noreplace = RenameFlags::NOREPLACE.bits();
    a.put(ROOT, "c.tmp", "c");
    assert_eq!(
        a.overlay
            .rename(
                ROOT,
                b"c.tmp",
                skills,
                b"b.key",
                noreplace,
                Actor::of_process(REQUEST)
            )
            .unwrap_err(),
        Errno::EEXIST
    );
    assert_eq!(
        a.overlay
            .rename(
                ROOT,
                b"c.tmp",
                ROOT,
                b"config.yaml",
                noreplace,
                Actor::of_process(REQUEST)
            )
            .unwrap_err(),
        Errno::EEXIST
    );
    assert_eq!(a.stored().len(), 2, "nothing moved");
    assert_eq!(
        std::fs::read_to_string(a.backing("config.yaml")).unwrap(),
        "config.yaml"
    );
    // Without it, a tmp file replaces another.
    a.overlay
        .rename(
            ROOT,
            b"c.tmp",
            skills,
            b"b.key",
            0,
            Actor::of_process(REQUEST),
        )
        .unwrap();
    assert_eq!(a.stored().len(), 1);
}

#[test]
fn judge_answers() {
    let tree = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tree.path().join("skills/repo/.git")).unwrap();
    let rules = Rules::new(
        tree.path(),
        RuleOptions::default(),
        EXCLUDE.as_bytes(),
        b"/skills/*.sec -agentfs\n",
    );
    let judge = Judge::new(
        Gate::new(Arc::new(rules)).unwrap(),
        Refusals::default().counter(),
    );
    let at = |path: &'static str, kind: Option<Kind>| Found {
        path: path.as_bytes(),
        kind,
    };
    let (file, dir) = (Some(Kind::File), Some(Kind::Dir));
    let other = Some(Kind::Other);
    let bad: &[u8] = b"skills/x\xff.md";
    let not_utf8 = Found {
        path: bad,
        kind: None,
    };

    // create
    assert_eq!(judge.create(at("skills/n.md", None), Kind::File), Ok(()));
    assert_eq!(judge.create(at("skills/n.md", file), Kind::File), Ok(()));
    assert_eq!(judge.create(at("skills/e", None), Kind::Dir), Ok(()));
    assert_eq!(
        judge.create(at("notes.txt", None), Kind::File),
        Err(Errno::EACCES),
        "a skipped file is the tmp layer's, never the tree's"
    );
    assert_eq!(
        judge.create(at("notes", None), Kind::Dir),
        Err(Errno::EACCES)
    );
    for kind in [Kind::Symlink, Kind::Other] {
        assert_eq!(judge.create(at("skills/l", None), kind), Err(Errno::EACCES));
    }
    assert_eq!(judge.create(not_utf8, Kind::File), Err(Errno::EACCES));
    // An entry hidden for its kind is never created over.
    assert_eq!(
        judge.create(at("skills/pipe", other), Kind::File),
        Err(Errno::EACCES)
    );
    assert_eq!(
        judge.create(at("skills/x.sec", file), Kind::Dir),
        Err(Errno::EACCES)
    );
    assert_eq!(
        judge.create(at("skills/repo", dir), Kind::File),
        Err(Errno::EACCES)
    );

    // link
    assert_eq!(judge.link(at("skills/n.md", None)), Err(Errno::EACCES));

    // remove
    assert_eq!(judge.remove(at("skills/a.md", file)), Ok(()));
    assert_eq!(judge.remove(at("skills/d", dir)), Ok(()));
    assert_eq!(judge.remove(at("skills/a.md", None)), Err(Errno::ENOENT));
    assert_eq!(judge.remove(at("secrets.db", file)), Err(Errno::ENOENT));
    assert_eq!(judge.remove(at("skills/pipe", other)), Err(Errno::ENOENT));
    assert_eq!(judge.remove(not_utf8), Err(Errno::ENOENT));

    // rename
    let exchange = RenameFlags::EXCHANGE.bits();
    let whiteout = RenameFlags::WHITEOUT.bits();
    assert_eq!(
        judge.rename(at("skills/a.md", file), at("config.yaml", None), 0),
        Ok(())
    );
    assert_eq!(
        judge.rename(at("skills/a.md", file), at("config.yaml", file), 0),
        Ok(())
    );
    assert_eq!(
        judge.rename(at("skills/a.md", file), at("config.yaml", None), whiteout),
        Err(Errno::EINVAL)
    );
    assert_eq!(
        judge.rename(at("skills/a.md", file), at("notes.txt", None), 0),
        Err(Errno::EACCES)
    );
    assert_eq!(
        judge.rename(at("skills/d", dir), at("skills/e", None), 0),
        Err(Errno::EXDEV)
    );
    assert_eq!(
        judge.rename(at("secrets.db", file), at("config.yaml", None), 0),
        Err(Errno::ENOENT)
    );
    assert_eq!(
        judge.rename(at("skills/a.md", None), at("config.yaml", None), 0),
        Err(Errno::ENOENT)
    );
    assert_eq!(
        judge.rename(at("skills/a.md", file), at("skills/pipe", other), 0),
        Err(Errno::EACCES)
    );
    assert_eq!(
        judge.rename(at("skills/a.md", file), at("config.yaml", file), exchange),
        Ok(())
    );
    assert_eq!(
        judge.rename(at("skills/a.md", file), at("config.yaml", None), exchange),
        Err(Errno::ENOENT)
    );
    assert_eq!(
        judge.rename(at("skills/a.md", file), at("skills/d", dir), exchange),
        Err(Errno::EXDEV)
    );

    // move_in
    assert_eq!(judge.move_in(at("config.yaml", None)), Ok(()));
    assert_eq!(judge.move_in(at("config.yaml", file)), Ok(()));
    assert_eq!(judge.move_in(at("notes.txt", None)), Err(Errno::EACCES));
    assert_eq!(judge.move_in(at("skills/pipe", other)), Err(Errno::EACCES));
    assert_eq!(judge.move_in(at("skills/repo", dir)), Err(Errno::EACCES));
    assert_eq!(judge.move_in(not_utf8), Err(Errno::EACCES));

    // A rules error is EIO.
    let long = format!("skills/{}", "d".repeat(300));
    let long: &'static str = Box::leak(long.into_boxed_str());
    assert_eq!(judge.move_in(at(long, dir)), Err(Errno::EIO));
    assert_eq!(judge.remove(at(long, dir)), Err(Errno::EIO));
}

#[test]
fn rmdir_of_a_directory_holding_tmp_files_is_not_empty() {
    let a = fixture_a(&[], &["skills/d"]);
    let skills = a.overlay.lookup(ROOT, b"skills").unwrap().id;
    let d = a.overlay.lookup(skills, b"d").unwrap().id;
    a.put(d, "x.key", "k");
    assert_eq!(
        a.overlay
            .rmdir(skills, b"d", Actor::of_process(REQUEST))
            .unwrap_err(),
        Errno::ENOTEMPTY
    );
    assert!(a.backing("skills/d").is_dir());
    assert_eq!(a.stored().len(), 1);
    a.overlay
        .unlink(d, b"x.key", Actor::of_process(REQUEST))
        .unwrap();
    a.overlay
        .rmdir(skills, b"d", Actor::of_process(REQUEST))
        .unwrap();
    assert!(!a.backing("skills/d").exists());
}

#[test]
fn a_skipped_file_name_over_a_shown_directory() {
    // `skills` is skipped as a file, shown as a directory.
    let b = fixture_b(FakeOps::examples());
    assert_eq!(
        b.overlay
            .create(
                ROOT,
                b"skills",
                0o644,
                O_WRONLY | O_EXCL,
                Actor::of_process(REQUEST)
            )
            .unwrap_err(),
        Errno::EEXIST
    );
    assert_eq!(
        b.overlay
            .create(ROOT, b"skills", 0o644, O_WRONLY, Actor::of_process(REQUEST))
            .unwrap_err(),
        Errno::EISDIR
    );
    assert_eq!(
        b.overlay
            .mknod(ROOT, b"skills", 0o100644, 0, Actor::of_process(REQUEST))
            .unwrap_err(),
        Errno::EEXIST
    );
    // Nothing there: a tmp file.
    b.overlay
        .create(
            ROOT,
            b"notes.txt",
            0o644,
            O_WRONLY,
            Actor::of_process(REQUEST),
        )
        .unwrap();
    // A directory over a tmp file's name.
    assert_eq!(
        b.overlay
            .mkdir(ROOT, b"notes.txt", 0o755, Actor::of_process(REQUEST))
            .unwrap_err(),
        Errno::EEXIST
    );
    // Without O_EXCL the tmp file is opened again; with it, it exists.
    // (Here the layer's files are this process's, not the owner's, who
    // may only read them.)
    b.overlay
        .create(
            ROOT,
            b"notes.txt",
            0o644,
            O_RDONLY,
            Actor::of_process(REQUEST),
        )
        .unwrap();
    assert_eq!(
        b.overlay
            .create(
                ROOT,
                b"notes.txt",
                0o644,
                O_WRONLY | O_EXCL,
                Actor::of_process(REQUEST)
            )
            .unwrap_err(),
        Errno::EEXIST
    );
    for method in ["create", "mknod", "mkdir"] {
        assert!(b.fake.calls_to(method).is_empty(), "{method}");
    }
}

#[test]
fn a_tmp_file_moves_into_the_tree_only_as_the_judge_allows() {
    let b = fixture_b(FakeOps::examples());
    b.overlay
        .create(
            ROOT,
            b"c.tmp",
            0o644,
            O_WRONLY | O_EXCL,
            Actor::of_process(REQUEST),
        )
        .unwrap();
    // Onto a recordable name: the operations move it, as the owner,
    // asking the judge.
    b.overlay
        .rename(
            ROOT,
            b"c.tmp",
            ROOT,
            b"config.yaml",
            0,
            Actor::of_process(REQUEST),
        )
        .unwrap();
    let calls = b.fake.calls_to("move_in");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].name, b"config.yaml");
    assert_eq!(calls[0].caller, Some(owner().caller(REQUEST.pid)));
    assert!(!b.overlay.tmp.holds(ROOT, b"c.tmp"));
    assert!(b.fake.calls_to("rename").is_empty());
}

#[test]
fn tmp_files_count_in_listings_with_dense_offsets() {
    let a = fixture_a(
        &["config.yaml", "secrets.db", "a.key", "skills/a.md"],
        &["notes"],
    );
    a.put(ROOT, "secrets.db", "tmp");
    a.put(ROOT, "new.txt", "n");
    let fh = a.overlay.opendir(ROOT).unwrap();
    let listed = a.overlay.readdir(ROOT, fh, 0).unwrap();
    let offsets: Vec<u64> = listed.iter().map(|(o, _)| *o).collect();
    assert_eq!(offsets, (1..=listed.len() as u64).collect::<Vec<_>>());
    assert_eq!(&names(&listed)[..2], [".", ".."]);
    let mut got = names(&listed);
    got.sort();
    assert_eq!(
        got,
        [".", "..", "config.yaml", "new.txt", "secrets.db", "skills"]
    );
    // Paging from any offset resumes at the next entry.
    for (i, (offset, _)) in listed.iter().enumerate() {
        let rest = a.overlay.readdir(ROOT, fh, *offset).unwrap();
        assert_eq!(names(&rest), names(&listed[i + 1..]));
    }
    a.overlay.releasedir(fh);
    assert_eq!(a.overlay.readdir(ROOT, fh, 0).unwrap_err(), Errno::EBADF);
}

#[test]
fn copy_file_range_across_layers_is_cross_device() {
    let a = fixture_a(&["config.yaml"], &[]);
    let (_, tmp_in, _) = a
        .overlay
        .create(
            ROOT,
            b"in.tmp",
            0o644,
            O_RDWR | O_EXCL,
            Actor::of_process(REQUEST),
        )
        .unwrap();
    a.overlay
        .write(tmp_in, 0, b"copied", Actor::of_process(REQUEST))
        .unwrap();
    let (_, tmp_out, _) = a
        .overlay
        .create(
            ROOT,
            b"out.tmp",
            0o644,
            O_RDWR | O_EXCL,
            Actor::of_process(REQUEST),
        )
        .unwrap();
    let id = a.overlay.lookup(ROOT, b"config.yaml").unwrap().id;
    let (tree, _) = a
        .overlay
        .open(id, O_RDWR, Actor::of_process(REQUEST))
        .unwrap();
    for (from, to) in [(tmp_in, tree), (tree, tmp_out)] {
        assert_eq!(
            a.overlay
                .copy_file_range(from, 0, to, 0, 6, Actor::of_process(REQUEST))
                .unwrap_err(),
            Errno::EXDEV
        );
    }
    assert_eq!(
        a.overlay
            .copy_file_range(tmp_in, 0, tmp_out, 0, 6, Actor::of_process(REQUEST))
            .unwrap(),
        6
    );
    assert_eq!(a.overlay.read(tmp_out, 0, 10).unwrap(), b"copied");
    assert_eq!(
        std::fs::read_to_string(a.backing("config.yaml")).unwrap(),
        "config.yaml"
    );
    for fh in [tmp_in, tmp_out, tree] {
        a.overlay.release(fh).unwrap();
    }
}

// ---- sandbox overlay slice 4: decisions under the names ordering, the
// view owner's access, refusal counts ----

/// A FIFO at a recordable name is hidden for its kind: an overlay rename
/// never replaces it and an overlay create never opens it, decided by the
/// operations' check under the names ordering on what is there then.
#[test]
fn b6_a_hidden_kind_at_a_recordable_name_is_never_replaced_nor_opened() {
    let a = fixture_a(&["skills/a.md"], &[]);
    let pipe = a.dir.path().join("backing/skills/p");
    rustix::fs::mknodat(
        rustix::fs::CWD,
        &pipe,
        rustix::fs::FileType::Fifo,
        rustix::fs::Mode::from_raw_mode(0o666),
        0,
    )
    .unwrap();
    let skills = a.overlay.lookup(ROOT, b"skills").unwrap().id;
    let actor = Actor::of_process(REQUEST);
    assert_eq!(
        a.overlay
            .rename(skills, b"a.md", skills, b"p", 0, actor)
            .unwrap_err(),
        Errno::EACCES
    );
    assert_eq!(
        a.overlay
            .create(skills, b"p", 0o644, O_WRONLY, actor)
            .unwrap_err(),
        Errno::EACCES
    );
    let st = rustix::fs::lstat(&pipe).unwrap();
    assert_eq!(
        rustix::fs::FileType::from_raw_mode(st.st_mode),
        rustix::fs::FileType::Fifo
    );
    assert!(a.dir.path().join("backing/skills/a.md").is_file());
    assert_eq!(
        a.refused.take(),
        [OverlayRefusal::Hidden, OverlayRefusal::Hidden]
    );
}

/// Only `user.*` attributes exist through the overlay, and reading one
/// needs the owner's read access.
#[test]
fn b8_extended_attributes_are_read_as_the_owner() {
    let b = fixture_b(
        FakeOps::examples()
            .node(8, 5, "theirs.md", FileType::RegularFile, 0o600, OTHER)
            .node(9, 5, "open.md", FileType::RegularFile, 0o644, OTHER),
    );
    assert_eq!(
        b.overlay.getxattr(8, b"user.a", 0).unwrap_err(),
        Errno::EACCES
    );
    assert_eq!(
        b.overlay.getxattr(9, b"trusted.b", 0).unwrap_err(),
        Errno::ENODATA
    );
    assert_eq!(
        b.overlay.getxattr(9, b"security.c", 64).unwrap_err(),
        Errno::ENODATA
    );
    assert!(b.fake.calls_to("getxattr").is_empty());
    b.overlay.getxattr(9, b"user.a", 0).unwrap();
    assert_eq!(b.fake.calls_to("getxattr").len(), 1);
    // Listed: the user.* names only, sized for that list.
    let listed = b"user.a\0user.d\0".to_vec();
    assert_eq!(
        b.overlay.listxattr(9, 0),
        Ok(Xattr::Size(u32::try_from(listed.len()).unwrap()))
    );
    assert_eq!(b.overlay.listxattr(9, 64), Ok(Xattr::Data(listed)));
    assert_eq!(b.overlay.listxattr(9, 5).unwrap_err(), Errno::ERANGE);
    // A hidden node has none.
    assert_eq!(b.overlay.listxattr(3, 0).unwrap_err(), Errno::ENOENT);
    assert!(b.refused.take().is_empty(), "reads are not counted");
}

/// An open to execute wants the owner's execute access.
#[test]
fn b9_an_exec_open_needs_the_owners_execute_bit() {
    let b = fixture_b(
        FakeOps::examples()
            .node(8, 5, "owner-only.sh", FileType::RegularFile, 0o744, OTHER)
            .node(9, 5, "anyone.sh", FileType::RegularFile, 0o755, OTHER),
    );
    let actor = Actor::of_process(REQUEST);
    let exec = O_RDONLY | 0o40;
    assert_eq!(b.overlay.open(8, exec, actor).unwrap_err(), Errno::EACCES);
    assert!(b.fake.calls_to("open").is_empty());
    // Read, not execute: allowed.
    b.overlay.open(8, O_RDONLY, actor).unwrap();
    b.overlay.open(9, exec, actor).unwrap();
    assert_eq!(b.fake.calls_to("open").len(), 2);
}

/// Each refused change is counted once, with its reason: the overlay's
/// own refusals before it calls the operations, the Judge's under the
/// names ordering. Reads of hidden names are not counted.
#[test]
fn b10_each_refused_change_is_counted_once() {
    let long = "d".repeat(300);
    let b = fixture_b_with(
        FakeOps::examples().node(9, 5, "p", FileType::NamedPipe, 0o644, OWNER.uid),
        "",
        &["skills"],
    );
    let actor = Actor::of_process(REQUEST);
    let s_ififo = 0o010000;
    // Reads of hidden names.
    let _ = b.overlay.lookup(ROOT, b"secrets.db");
    let _ = b.overlay.getattr(3, None);
    let _ = b.overlay.open(3, O_RDONLY, actor);
    assert!(b.refused.take().is_empty());

    let cases: Vec<(&str, Errno, OverlayRefusal, OpResult<()>)> = vec![
        (
            "symlink",
            Errno::EACCES,
            OverlayRefusal::Kind,
            b.overlay.symlink(5, b"s", b"a.md", actor).map(drop),
        ),
        (
            "link",
            Errno::EACCES,
            OverlayRefusal::Kind,
            b.overlay.link(6, 5, b"l", actor).map(drop),
        ),
        (
            "mknod fifo",
            Errno::EACCES,
            OverlayRefusal::Kind,
            b.overlay
                .mknod(5, b"f", s_ififo | 0o644, 0, actor)
                .map(drop),
        ),
        (
            "mkdir of a skipped directory",
            Errno::EACCES,
            OverlayRefusal::NotRecordable,
            b.overlay.mkdir(ROOT, b"notes2", 0o755, actor).map(drop),
        ),
        (
            "a directory move",
            Errno::EXDEV,
            OverlayRefusal::CrossDevice,
            b.overlay.rename(5, b"d", 5, b"e", 0, actor),
        ),
        (
            "a recorded file moved to a skipped name",
            Errno::EACCES,
            OverlayRefusal::NotRecordable,
            b.overlay
                .rename(ROOT, b"config.yaml", ROOT, b"other.txt", 0, actor),
        ),
        (
            "unlink of a hidden name",
            Errno::ENOENT,
            OverlayRefusal::Hidden,
            b.overlay.unlink(ROOT, b"secrets.db", actor),
        ),
        (
            "a rename onto a FIFO (the Judge)",
            Errno::EACCES,
            OverlayRefusal::Hidden,
            b.overlay.rename(5, b"a.md", 5, b"p", 0, actor),
        ),
        (
            "chown",
            Errno::EPERM,
            OverlayRefusal::Ownership,
            b.overlay
                .setattr(
                    2,
                    &SetAttr {
                        uid: Some(0),
                        ..SetAttr::default()
                    },
                    actor,
                )
                .map(drop),
        ),
        (
            "setuid",
            Errno::EPERM,
            OverlayRefusal::Ownership,
            b.overlay
                .setattr(
                    2,
                    &SetAttr {
                        mode: Some(0o4755),
                        ..SetAttr::default()
                    },
                    actor,
                )
                .map(drop),
        ),
        (
            "setxattr",
            Errno::ENOTSUP,
            OverlayRefusal::Xattr,
            b.overlay.setxattr(2, b"user.x", b"1", 0),
        ),
        (
            "removexattr",
            Errno::ENOTSUP,
            OverlayRefusal::Xattr,
            b.overlay.removexattr(2, b"user.x"),
        ),
        (
            "a rules error",
            Errno::EIO,
            OverlayRefusal::RulesError,
            b.overlay
                .mkdir(5, format!("{long}x").as_bytes(), 0o755, actor)
                .map(drop),
        ),
    ];
    let counted = b.refused.take();
    let expected: Vec<OverlayRefusal> = cases.iter().map(|c| c.2).collect();
    for (what, errno, _, result) in &cases {
        assert_eq!(result.as_ref().unwrap_err(), errno, "{what}");
    }
    assert_eq!(counted, expected);
}
