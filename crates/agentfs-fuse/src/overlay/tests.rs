//! The overlay at two isolation levels: [`FakeOps`], which records every
//! call and never touches a disk, proves what is refused before the
//! operations are asked, the existence-blind answers and the owner
//! standing in for the caller; a real [`Ops`] on a temporary directory
//! proves complete mediation and listings over real rules.

use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::sync::Mutex;
use std::time::UNIX_EPOCH;

use agentfs_core::events::MemorySink;
use agentfs_core::identity::Context;
use agentfs_core::rules::{RuleOptions, Rules};
use agentfs_core::telemetry::Telemetry;
use agentfs_core::volume::{self, StateDir};
use fuser::INodeNo;

use super::*;
use crate::backing::Backing;
use crate::ops::{Ops, OpsConfig};

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

/// An [`Operations`] with scripted nodes that records every call.
#[derive(Default)]
struct FakeOps {
    calls: Mutex<Vec<Call>>,
    attrs: HashMap<u64, FileAttr>,
    paths: HashMap<u64, Vec<u8>>,
    children: HashMap<(u64, Vec<u8>), u64>,
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

    fn setattr(&self, id: u64, _set: &SetAttr, caller: Caller) -> OpResult<FileAttr> {
        self.record("setattr", b"", Some(caller));
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
        _parent: u64,
        name: &[u8],
        _mode: u32,
        _rdev: u32,
        caller: Caller,
    ) -> OpResult<Entry> {
        self.record("mknod", name, Some(caller));
        Ok(self.entry(2))
    }

    fn mkdir(&self, _parent: u64, name: &[u8], _mode: u32, caller: Caller) -> OpResult<Entry> {
        self.record("mkdir", name, Some(caller));
        Ok(self.entry(5))
    }

    fn unlink(&self, _parent: u64, name: &[u8], caller: Caller) -> OpResult<()> {
        self.record("unlink", name, Some(caller));
        Ok(())
    }

    fn rmdir(&self, _parent: u64, name: &[u8], caller: Caller) -> OpResult<()> {
        self.record("rmdir", name, Some(caller));
        Ok(())
    }

    fn symlink(
        &self,
        _parent: u64,
        name: &[u8],
        _target: &[u8],
        caller: Caller,
    ) -> OpResult<Entry> {
        self.record("symlink", name, Some(caller));
        Ok(self.entry(2))
    }

    fn rename(
        &self,
        _parent: u64,
        name: &[u8],
        _newparent: u64,
        _newname: &[u8],
        _flags: u32,
        caller: Caller,
    ) -> OpResult<()> {
        self.record("rename", name, Some(caller));
        Ok(())
    }

    fn link(&self, _id: u64, _newparent: u64, newname: &[u8], caller: Caller) -> OpResult<Entry> {
        self.record("link", newname, Some(caller));
        Ok(self.entry(2))
    }

    fn open(&self, id: u64, _flags: i32, caller: Caller) -> OpResult<(u64, Arc<Handle>)> {
        self.record("open", b"", Some(caller));
        Ok((1, Self::handle(id)))
    }

    fn create(
        &self,
        _parent: u64,
        name: &[u8],
        _mode: u32,
        _flags: i32,
        caller: Caller,
    ) -> OpResult<(Entry, u64, Arc<Handle>)> {
        self.record("create", name, Some(caller));
        Ok((self.entry(2), 1, Self::handle(2)))
    }

    fn read(&self, _fh: u64, _offset: u64, _size: u32) -> OpResult<Vec<u8>> {
        self.record("read", b"", None);
        Ok(Vec::new())
    }

    fn write(&self, _fh: u64, _offset: u64, data: &[u8], caller: Caller) -> OpResult<u32> {
        self.record("write", b"", Some(caller));
        Ok(u32::try_from(data.len()).unwrap())
    }

    fn flush(&self, _fh: u64, caller: Caller) -> OpResult<()> {
        self.record("flush", b"", Some(caller));
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
        caller: Caller,
    ) -> OpResult<()> {
        self.record("fallocate", b"", Some(caller));
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
        caller: Caller,
    ) -> OpResult<u32> {
        self.record("copy_file_range", b"", Some(caller));
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

    fn listxattr(&self, _id: u64, _size: u32) -> OpResult<Xattr> {
        self.record("listxattr", b"", None);
        Ok(Xattr::Size(0))
    }

    fn removexattr(&self, _id: u64, name: &[u8]) -> OpResult<()> {
        self.record("removexattr", name, None);
        Ok(())
    }
}

/// Fixture B: the overlay over [`FakeOps`], with the examples' rules over
/// an empty tree (`_tree` keeps it for nested-repository probes).
struct B {
    _tree: tempfile::TempDir,
    fake: Arc<FakeOps>,
    overlay: Overlay<FakeOps>,
}

fn fixture_b(fake: FakeOps) -> B {
    let tree = tempfile::tempdir().unwrap();
    let rules = Rules::new(tree.path(), RuleOptions::default(), EXCLUDE.as_bytes(), b"");
    let gate = Gate::new(Arc::new(rules)).unwrap();
    let fake = Arc::new(fake);
    B {
        _tree: tree,
        overlay: Overlay::new(fake.clone(), gate, owner()),
        fake,
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
    // Offsets are the operations' own, so paging resumes where it left off.
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
            b.overlay.unlink(ROOT, n, REQUEST).unwrap_err(),
            Errno::ENOENT,
            "{name}"
        );
        assert_eq!(
            b.overlay
                .create(ROOT, n, 0o644, O_WRONLY, REQUEST)
                .unwrap_err(),
            Errno::EACCES,
            "{name}"
        );
        assert_eq!(
            b.overlay.mkdir(ROOT, n, 0o755, REQUEST).unwrap_err(),
            Errno::EACCES,
            "{name}"
        );
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
        .create(5, b"new.md", 0o644, O_WRONLY, REQUEST)
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
    b.overlay.write(1, 0, b"x", REQUEST).unwrap();
    b.overlay.flush(1, REQUEST).unwrap();
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
            b.overlay.open(id, O_WRONLY, caller).unwrap_err(),
            Errno::EACCES
        );
    }
    assert!(b.fake.calls_to("open").is_empty());
    b.overlay.open(id, O_RDONLY, REQUEST).unwrap();
    assert_eq!(b.fake.calls_to("open").len(), 1);
    // Nor through access(2), nor a create that would open it to write.
    assert_eq!(b.overlay.access(id, 2).unwrap_err(), Errno::EACCES);
    assert_eq!(
        b.overlay
            .create(5, b"theirs.md", 0o644, O_WRONLY, REQUEST)
            .unwrap_err(),
        Errno::EACCES
    );
    assert!(b.fake.calls_to("create").is_empty());
}

#[test]
fn a8_refusals() {
    let b = fixture_b(FakeOps::examples());
    let s_ififo = 0o010000;
    assert_eq!(
        b.overlay
            .symlink(5, b"l", b"config.yaml", REQUEST)
            .unwrap_err(),
        Errno::EACCES
    );
    assert_eq!(
        b.overlay.link(2, 5, b"c.yaml", REQUEST).unwrap_err(),
        Errno::EACCES
    );
    assert_eq!(
        b.overlay
            .mknod(5, b"f", s_ififo | 0o644, 0, REQUEST)
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
        b.overlay.setattr(2, &chown, REQUEST).unwrap_err(),
        Errno::EPERM
    );
    let setuid = SetAttr {
        mode: Some(0o4755),
        ..SetAttr::default()
    };
    assert_eq!(
        b.overlay.setattr(2, &setuid, REQUEST).unwrap_err(),
        Errno::EPERM
    );
    assert!(b.fake.calls_to("setattr").is_empty());
    // A plain mode by the object's owner goes on.
    let chmod = SetAttr {
        mode: Some(0o755),
        ..SetAttr::default()
    };
    b.overlay.setattr(2, &chmod, REQUEST).unwrap();
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
        b.overlay.rename(5, b"d", 5, b"e", 0, REQUEST).unwrap_err(),
        Errno::EXDEV
    );
    assert_eq!(
        b.overlay
            .rename(ROOT, b"config.yaml", ROOT, b"config.bak", 0, REQUEST)
            .unwrap_err(),
        Errno::EACCES
    );
    let whiteout = RenameFlags::WHITEOUT.bits();
    assert_eq!(
        b.overlay
            .rename(5, b"a.md", 5, b"b.md", whiteout, REQUEST)
            .unwrap_err(),
        Errno::EINVAL
    );
    assert!(b.fake.calls_to("rename").is_empty());
    // A file moved to a recordable name goes on, as the owner.
    b.overlay
        .rename(ROOT, b"config.yaml", 5, b"config.yaml", 0, REQUEST)
        .unwrap();
    assert_eq!(
        b.fake.calls_to("rename")[0].caller,
        Some(owner().caller(REQUEST.pid))
    );

    // A skipped name would go to the tmp layer, which is not built yet.
    assert_eq!(
        b.overlay
            .create(ROOT, b"notes.txt", 0o644, O_WRONLY, REQUEST)
            .unwrap_err(),
        Errno::EACCES
    );
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
    b.overlay.setattr(8, &now, REQUEST).unwrap();
    let explicit = SetAttr {
        mtime: Some(TimeOrNow::SpecificTime(UNIX_EPOCH)),
        ..SetAttr::default()
    };
    assert_eq!(
        b.overlay.setattr(8, &explicit, REQUEST).unwrap_err(),
        Errno::EPERM
    );
    // A mode change needs the owner to own the object.
    let chmod = SetAttr {
        mode: Some(0o600),
        ..SetAttr::default()
    };
    assert_eq!(
        b.overlay.setattr(8, &chmod, REQUEST).unwrap_err(),
        Errno::EPERM
    );
    // On the owner's own file, explicit times are fine.
    b.overlay.setattr(6, &explicit, REQUEST).unwrap();
}

/// Fixture A: a real [`Ops`] on a temporary directory, with the overlay
/// in front of it, the owner being this process's own ids.
struct A {
    dir: tempfile::TempDir,
    ops: Arc<Ops>,
    overlay: Overlay<Ops>,
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
    A {
        overlay: Overlay::new(ops.clone(), gate, owner),
        ops,
        dir,
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
        .rename(ROOT, b"config.yaml", ROOT, b"secrets2.db", 0, main)
        .unwrap();
    assert_eq!(a.overlay.getattr(id, None).unwrap_err(), Errno::ENOENT);
    assert_eq!(
        a.overlay.open(id, O_RDONLY, REQUEST).unwrap_err(),
        Errno::ENOENT
    );
    assert_eq!(a.overlay.access(id, 4).unwrap_err(), Errno::ENOENT);
}
