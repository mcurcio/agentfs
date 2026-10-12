//! `Ops` against a real backing directory and a real state area, without
//! the kernel: every boundary of design §3.1 and the backstops.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::PathBuf;
use std::sync::Arc;

use agentfs_core::events::{Event, MemorySink};
use agentfs_core::identity::Context;
use agentfs_core::telemetry::Telemetry;
use agentfs_core::volume::{self, Committer, StateDir};

use super::*;

const C: Caller = Caller {
    uid: 1000,
    gid: 1000,
    pid: 42,
};

const O_RDONLY: i32 = 0;
const O_WRONLY: i32 = 1;
const O_RDWR: i32 = 2;
const O_TRUNC: i32 = 0o1000;
const O_APPEND: i32 = 0o2000;

struct Fx {
    dir: tempfile::TempDir,
    ops: Ops,
    committer: Committer,
    sink: Arc<MemorySink>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Item {
    File(String),
    Exec(String),
    Link(String),
}

fn file(s: &str) -> Item {
    Item::File(s.into())
}

impl Fx {
    fn new() -> Self {
        Self::with(OpsConfig {
            granularity_ns: 0,
            chunk: RENAME_CHUNK,
        })
    }

    fn with(config: OpsConfig) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let backing = dir.path().join("backing");
        fs::create_dir_all(&backing).unwrap();
        let state = StateDir::new(dir.path().join("state"));
        volume::init(&state, "vol", None).unwrap();
        Self::open_on(dir, config)
    }

    fn open_on(dir: tempfile::TempDir, config: OpsConfig) -> Self {
        let backing = dir.path().join("backing");
        let state = StateDir::new(dir.path().join("state"));
        let sink = Arc::new(MemorySink::new());
        let context = Context {
            volume: "vol".into(),
            replica: "pod-0".into(),
            pod_uid: "u".into(),
            ..Context::default()
        };
        let (recorder, committer, _) =
            volume::open(&state, &backing, context, sink.clone(), &Telemetry::noop()).unwrap();
        let ops = Ops::new(Backing::open(&backing).unwrap(), recorder, config).unwrap();
        Self {
            dir,
            ops,
            committer,
            sink,
        }
    }

    fn backing(&self) -> PathBuf {
        self.dir.path().join("backing")
    }

    fn state(&self) -> StateDir {
        StateDir::new(self.dir.path().join("state"))
    }

    fn backlog(&self) -> usize {
        self.committer.backlog()
    }

    /// Commits everything and returns the branch tree.
    fn history(&self) -> BTreeMap<String, Item> {
        self.committer.commit_pending().unwrap();
        let mut out = BTreeMap::new();
        let history = self.committer.history();
        let Some(head) = history.head().unwrap() else {
            return out;
        };
        let repo = history.repo();
        head.tree()
            .unwrap()
            .walk(git2::TreeWalkMode::PreOrder, |root, e| {
                if e.kind() == Some(git2::ObjectType::Blob) {
                    let blob = repo.find_blob(e.id()).unwrap();
                    let text = String::from_utf8_lossy(blob.content()).into_owned();
                    let item = match e.filemode() {
                        0o100755 => Item::Exec(text),
                        0o120000 => Item::Link(text),
                        _ => Item::File(text),
                    };
                    out.insert(format!("{root}{}", e.name().unwrap()), item);
                }
                git2::TreeWalkResult::Ok
            })
            .unwrap();
        out
    }

    fn id(&self, path: &str) -> u64 {
        let mut id = ROOT;
        for part in path.split('/').filter(|p| !p.is_empty()) {
            id = self.ops.lookup(id, part.as_bytes()).unwrap().id;
        }
        id
    }

    fn split<'a>(&self, path: &'a str) -> (u64, &'a str) {
        match path.rsplit_once('/') {
            Some((dir, name)) => (self.id(dir), name),
            None => (ROOT, path),
        }
    }

    fn mkdir(&self, path: &str) -> u64 {
        let (parent, name) = self.split(path);
        self.ops
            .mkdir(parent, name.as_bytes(), 0o755, C)
            .unwrap()
            .id
    }

    fn create(&self, path: &str) -> (u64, u64) {
        let (parent, name) = self.split(path);
        let (entry, fh, _) = self
            .ops
            .create(parent, name.as_bytes(), 0o100644, O_WRONLY | 0o100, C)
            .unwrap();
        (entry.id, fh)
    }

    fn open(&self, path: &str, flags: i32) -> u64 {
        let id = self.id(path);
        self.ops.open(id, flags, C).unwrap().0
    }

    /// create, write, close (flush and release).
    fn put(&self, path: &str, content: &str) {
        let (_, fh) = self.create(path);
        self.ops.write(fh, 0, content.as_bytes(), C).unwrap();
        self.ops.flush(fh, C).unwrap();
        self.ops.release(fh).unwrap();
    }

    fn close(&self, fh: u64) {
        self.ops.flush(fh, C).unwrap();
        self.ops.release(fh).unwrap();
    }

    fn rename(&self, from: &str, to: &str, flags: u32) -> OpResult<()> {
        let (p1, n1) = self.split(from);
        let (p2, n2) = self.split(to);
        self.ops
            .rename(p1, n1.as_bytes(), p2, n2.as_bytes(), flags, C)
    }

    fn refused(&self) -> Vec<String> {
        self.sink
            .snapshot()
            .into_iter()
            .filter_map(|e| match e {
                Event::PathRefused { path, .. } => Some(path),
                _ => None,
            })
            .collect()
    }
}

fn map(items: &[(&str, Item)]) -> BTreeMap<String, Item> {
    items
        .iter()
        .map(|(p, i)| ((*p).to_owned(), i.clone()))
        .collect()
}

#[test]
fn create_write_close_records_once() {
    let fx = Fx::new();
    let (_, fh) = fx.create("a");
    fx.ops.write(fh, 0, b"hello", C).unwrap();
    fx.ops.flush(fh, C).unwrap();
    assert_eq!(fx.backlog(), 1);
    // A second close with nothing new records nothing.
    fx.ops.flush(fh, C).unwrap();
    assert_eq!(fx.backlog(), 1);
    fx.ops.release(fh).unwrap();
    assert_eq!(fx.backlog(), 1, "release: unchanged since the flush");
    assert_eq!(fx.history(), map(&[("a", file("hello"))]));
    assert_eq!(fs::read_to_string(fx.backing().join("a")).unwrap(), "hello");

    // A read-only open and close of an unchanged file records nothing.
    let fh = fx.open("a", O_RDONLY);
    assert_eq!(fx.ops.read(fh, 0, 100).unwrap(), b"hello");
    fx.close(fh);
    // A write-open without writes records nothing either.
    let fh = fx.open("a", O_WRONLY);
    fx.close(fh);
    assert_eq!(fx.backlog(), 0);
}

#[test]
fn touch_records_an_empty_file() {
    let fx = Fx::new();
    let (_, fh) = fx.create("empty");
    fx.close(fh);
    assert_eq!(fx.history(), map(&[("empty", file(""))]));
}

#[test]
fn append_truncate_and_o_trunc() {
    let fx = Fx::new();
    fx.put("f", "one");
    let fh = fx.open("f", O_WRONLY | O_APPEND);
    fx.ops.write(fh, 3, b"two", C).unwrap();
    fx.close(fh);
    assert_eq!(fx.history(), map(&[("f", file("onetwo"))]));

    // truncate(1) by path: a setattr boundary.
    let id = fx.id("f");
    let set = SetAttr {
        size: Some(3),
        ..SetAttr::default()
    };
    fx.ops.setattr(id, &set, C).unwrap();
    assert_eq!(fx.history(), map(&[("f", file("one"))]));

    // ftruncate on an open handle.
    let fh = fx.open("f", O_RDWR);
    let set = SetAttr {
        size: Some(1),
        fh: Some(fh),
        ..SetAttr::default()
    };
    fx.ops.setattr(id, &set, C).unwrap();
    assert_eq!(fx.backlog(), 1);
    fx.close(fh);
    assert_eq!(fx.backlog(), 1, "nothing changed after the truncate");
    assert_eq!(fx.history(), map(&[("f", file("o"))]));

    // `>` on an existing file: O_TRUNC at open, one record at close.
    let fh = fx.open("f", O_WRONLY | O_TRUNC);
    fx.ops.write(fh, 0, b"new", C).unwrap();
    fx.close(fh);
    assert_eq!(fx.backlog(), 1);
    assert_eq!(fx.history(), map(&[("f", file("new"))]));

    // `: > f`: truncated to empty, no write, still recorded.
    let fh = fx.open("f", O_WRONLY | O_TRUNC);
    fx.close(fh);
    assert_eq!(fx.history(), map(&[("f", file(""))]));
}

#[test]
fn same_size_rewrites_are_caught_by_the_racy_rule() {
    let fx = Fx::with(OpsConfig::default());
    fx.put("f", "aaa");
    for content in ["bbb", "ccc", "ddd"] {
        let fh = fx.open("f", O_WRONLY);
        fx.ops.write(fh, 0, content.as_bytes(), C).unwrap();
        fx.close(fh);
        assert_eq!(fx.history(), map(&[("f", file(content))]), "{content}");
    }
}

#[test]
fn exec_bit_and_other_metadata() {
    let fx = Fx::new();
    fx.put("s", "#!/bin/sh");
    fx.history();
    let id = fx.id("s");
    let chmod = |mode| SetAttr {
        mode: Some(mode),
        ..SetAttr::default()
    };
    fx.ops.setattr(id, &chmod(0o755), C).unwrap();
    assert_eq!(fx.history(), map(&[("s", Item::Exec("#!/bin/sh".into()))]));
    // Other bits: passed through, not recorded.
    fx.ops.setattr(id, &chmod(0o700), C).unwrap();
    assert_eq!(fx.backlog(), 0);
    fx.ops.setattr(id, &chmod(0o600), C).unwrap();
    assert_eq!(fx.history(), map(&[("s", file("#!/bin/sh"))]));
    fx.ops.setattr(id, &chmod(0o400), C).unwrap();
    assert_eq!(fx.backlog(), 0);
    let mode = fs::metadata(fx.backing().join("s"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o7777, 0o400, "chmod persists on the backing tree");

    // chown to our own ids and utimens: passed through, not recorded.
    let me = fs::metadata(fx.backing().join("s")).unwrap();
    let set = SetAttr {
        uid: Some(me.uid()),
        gid: Some(me.gid()),
        ..SetAttr::default()
    };
    fx.ops.setattr(id, &set, C).unwrap();
    let when = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
    let set = SetAttr {
        mtime: Some(TimeOrNow::SpecificTime(when)),
        atime: Some(TimeOrNow::Now),
        ..SetAttr::default()
    };
    let attr = fx.ops.setattr(id, &set, C).unwrap();
    assert_eq!(attr.mtime, when);
    assert_eq!(
        fs::metadata(fx.backing().join("s")).unwrap().mtime(),
        1_000_000
    );
    assert_eq!(fx.backlog(), 0);
}

#[test]
fn rename_records_removal_and_destination() {
    let fx = Fx::new();
    fx.put("a", "1");
    fx.rename("a", "b", 0).unwrap();
    assert_eq!(fx.history(), map(&[("b", file("1"))]));

    // An editor's save: write a temporary file, rename it over the target.
    fx.put("target", "old");
    fx.put(".target.swp", "new");
    fx.rename(".target.swp", "target", 0).unwrap();
    assert_eq!(
        fx.history(),
        map(&[("b", file("1")), ("target", file("new"))])
    );
}

#[test]
fn directory_renames_move_every_file() {
    let fx = Fx::new();
    fx.mkdir("d");
    fx.mkdir("d/e");
    fx.put("d/x", "x");
    fx.put("d/e/y", "y");
    let (p, _) = fx.split("d/e/y");
    fx.ops.symlink(p, b"l", b"y", C).unwrap();
    fx.rename("d", "moved", 0).unwrap();
    assert_eq!(
        fx.history(),
        map(&[
            ("moved/e/l", Item::Link("y".into())),
            ("moved/e/y", file("y")),
            ("moved/x", file("x")),
        ])
    );
    // Paths of open nodes follow the rename.
    let fh = fx.open("moved/x", O_WRONLY | O_APPEND);
    fx.ops.write(fh, 1, b"2", C).unwrap();
    fx.close(fh);
    assert_eq!(fx.history()["moved/x"], file("x2"));
}

#[test]
fn large_directory_renames_are_chunked() {
    let fx = Fx::with(OpsConfig {
        granularity_ns: 0,
        chunk: 3,
    });
    fx.mkdir("big");
    for i in 0..8 {
        fx.put(&format!("big/f{i}"), &i.to_string());
    }
    fx.history();
    fx.rename("big", "big2", 0).unwrap();
    assert_eq!(fx.backlog(), 3, "8 files in chunks of 3");
    let history = fx.history();
    assert_eq!(history.len(), 8);
    assert!(
        history.keys().all(|k| k.starts_with("big2/")),
        "{history:?}"
    );
}

#[test]
fn rename_flags() {
    let fx = Fx::new();
    fx.put("a", "A");
    fx.put("b", "BB");
    fx.history();
    let noreplace = RenameFlags::NOREPLACE.bits();
    assert_eq!(fx.rename("a", "b", noreplace), Err(Errno::EEXIST));
    assert_eq!(fx.backlog(), 0);
    fx.rename("a", "b", RenameFlags::EXCHANGE.bits()).unwrap();
    assert_eq!(fx.history(), map(&[("a", file("BB")), ("b", file("A"))]));
    assert_eq!(
        fx.rename("a", "b", RenameFlags::WHITEOUT.bits()),
        Err(Errno::EINVAL)
    );
    // A rename between two names of one inode changes nothing.
    let id = fx.id("a");
    fx.ops.link(id, ROOT, b"a2", C).unwrap();
    fx.history();
    fx.rename("a", "a2", 0).unwrap();
    assert!(fx.backing().join("a").exists());
    assert_eq!(fx.backlog(), 0);
}

#[test]
fn unlink_and_rm_r() {
    let fx = Fx::new();
    fx.mkdir("d");
    fx.mkdir("d/e");
    fx.put("d/e/f", "f");
    fx.put("keep", "k");
    fx.history();
    // rm -r: depth first.
    let e = fx.id("d/e");
    fx.ops.unlink(e, b"f", C).unwrap();
    let d = fx.id("d");
    fx.ops.rmdir(d, b"e", C).unwrap();
    fx.ops.rmdir(ROOT, b"d", C).unwrap();
    assert_eq!(fx.history(), map(&[("keep", file("k"))]));
    fx.ops.unlink(ROOT, b"keep", C).unwrap();
    assert_eq!(fx.history(), map(&[]));
    assert_eq!(fx.ops.unlink(ROOT, b"keep", C), Err(Errno::ENOENT));
}

#[test]
fn hard_links() {
    let fx = Fx::new();
    fx.put("a", "1");
    let id = fx.id("a");
    let entry = fx.ops.link(id, ROOT, b"b", C).unwrap();
    assert_eq!(entry.id, id, "one inode, one node, one st_ino");
    assert_eq!(fx.history(), map(&[("a", file("1")), ("b", file("1"))]));
    // A write through one name is recorded at both.
    let fh = fx.open("b", O_WRONLY | O_APPEND);
    fx.ops.write(fh, 1, b"2", C).unwrap();
    fx.close(fh);
    assert_eq!(fx.history(), map(&[("a", file("12")), ("b", file("12"))]));
}

#[test]
fn symlinks() {
    let fx = Fx::new();
    let entry = fx.ops.symlink(ROOT, b"l", b"some/target", C).unwrap();
    assert_eq!(entry.attr.kind, FileType::Symlink);
    assert_eq!(fx.ops.readlink(entry.id).unwrap(), b"some/target");
    assert_eq!(
        fx.history(),
        map(&[("l", Item::Link("some/target".into()))])
    );
    // chmod of a symlink is not supported (Linux has no lchmod).
    let set = SetAttr {
        mode: Some(0o700),
        ..SetAttr::default()
    };
    assert!(fx.ops.setattr(entry.id, &set, C).is_err());
}

#[test]
fn unlink_of_an_open_file() {
    let fx = Fx::new();
    fx.put("f", "x");
    fx.history();
    // exec 3>>f; rm f; echo y >&3; exec 3>&-
    let fh = fx.open("f", O_WRONLY | O_APPEND);
    fx.ops.unlink(ROOT, b"f", C).unwrap();
    fx.ops.write(fh, 1, b"y", C).unwrap();
    // Still answerable through the handle.
    let id = fx.ops.handle(fh).unwrap().node;
    assert_eq!(fx.ops.getattr(id, Some(fh)).unwrap().size, 2);
    assert_eq!(fx.ops.getattr(id, None).unwrap().size, 2);
    fx.close(fh);
    assert_eq!(fx.history(), map(&[]));
}

#[test]
fn rename_over_an_open_file() {
    let fx = Fx::new();
    fx.put("f", "old");
    fx.put("new", "fresh");
    let fh = fx.open("f", O_WRONLY | O_APPEND);
    fx.rename("new", "f", 0).unwrap();
    // The replaced inode has no name: its late write is not recorded.
    fx.ops.write(fh, 3, b"!", C).unwrap();
    fx.close(fh);
    assert_eq!(fx.history(), map(&[("f", file("fresh"))]));
}

#[test]
fn log_rotation_with_the_writer_open() {
    let fx = Fx::new();
    fx.put("app.log", "a\n");
    let fh = fx.open("app.log", O_WRONLY | O_APPEND);
    fx.ops.write(fh, 2, b"b\n", C).unwrap();
    fx.rename("app.log", "app.log.1", 0).unwrap();
    fx.put("app.log", "new\n");
    // The writer still holds the rotated file.
    fx.ops.write(fh, 4, b"late\n", C).unwrap();
    fx.close(fh);
    assert_eq!(
        fx.history(),
        map(&[
            ("app.log", file("new\n")),
            ("app.log.1", file("a\nb\nlate\n")),
        ])
    );
}

#[test]
fn git_internals_are_never_recorded() {
    let fx = Fx::new();
    // `git init` at the root.
    fx.mkdir(".git");
    fx.put(".git/HEAD", "ref: refs/heads/main\n");
    fx.put("README", "r");
    // A nested repository in a subdirectory.
    fx.mkdir("sub");
    fx.mkdir("sub/.git");
    fx.put("sub/.git/config", "[core]\n");
    fx.rename(".git/HEAD", ".git/HEAD2", 0).unwrap();
    let git = fx.id(".git");
    fx.ops.unlink(git, b"HEAD2", C).unwrap();
    assert_eq!(fx.history(), map(&[("README", file("r"))]));
    assert!(fx.refused().is_empty(), "{:?}", fx.refused());

    // A look-alike is refused, with an alarm.
    fx.mkdir(".GIT");
    fx.put(".GIT/x", "x");
    assert_eq!(fx.refused(), [".GIT/x"]);
    assert_eq!(fx.history(), map(&[("README", file("r"))]));
}

#[test]
fn non_utf8_names_are_refused_but_work() {
    let fx = Fx::new();
    let (entry, fh, _) = fx
        .ops
        .create(ROOT, b"bad\xff", 0o100644, O_WRONLY, C)
        .unwrap();
    fx.ops.write(fh, 0, b"data", C).unwrap();
    fx.close(fh);
    assert_eq!(fx.refused().len(), 1, "{:?}", fx.refused());
    assert_eq!(fx.history(), map(&[]));
    assert_eq!(fx.ops.getattr(entry.id, None).unwrap().size, 4);
    // A symlink whose target is not UTF-8 is refused too.
    fx.ops.symlink(ROOT, b"l", b"\xfe", C).unwrap();
    assert_eq!(fx.refused().len(), 2);
}

#[test]
fn record_failures_return_eio_and_are_repaired() {
    let fx = Fx::new();
    if rustix::process::geteuid().is_root() {
        // Root ignores the permission bits this test relies on.
        return;
    }
    fx.put("keep", "k");
    fx.put("gone", "g");
    fx.history();
    let pending = fx.state().pending();
    let journal_dir = fx.state().journal().parent().unwrap().to_path_buf();
    let lock = |mode| {
        fs::set_permissions(&pending, fs::Permissions::from_mode(mode)).unwrap();
        fs::set_permissions(&journal_dir, fs::Permissions::from_mode(mode)).unwrap();
    };

    let fh = fx.open("keep", O_WRONLY | O_APPEND);
    fx.ops.write(fh, 1, b"2", C).unwrap();
    lock(0o500);
    assert_eq!(fx.ops.flush(fh, C), Err(Errno::EIO));
    assert!(
        fx.sink
            .snapshot()
            .iter()
            .any(|e| matches!(e, Event::RecordFailed { .. }))
    );
    lock(0o700);
    // Still dirty: the next close records it.
    fx.ops.flush(fh, C).unwrap();
    fx.ops.release(fh).unwrap();
    assert_eq!(fx.history()["keep"], file("k2"));

    // A namespace operation that already happened is repaired by the sweep.
    fx.ops.unlink(ROOT, b"gone", C).unwrap_or(());
    fx.put("gone", "again");
    fx.history();
    // Make the pending directory unreadable: the journal syncs it before
    // every write, so even a removal fails to journal.
    fs::set_permissions(&pending, fs::Permissions::from_mode(0o000)).unwrap();
    assert_eq!(fx.ops.unlink(ROOT, b"gone", C), Err(Errno::EIO));
    assert!(!fx.backing().join("gone").exists(), "the unlink happened");
    assert_eq!(fx.ops.boundary().pending_repairs(), 1);
    fs::set_permissions(&pending, fs::Permissions::from_mode(0o700)).unwrap();
    let report = fx.ops.sweep(false);
    assert_eq!(report.repaired, 1);
    assert_eq!(fx.history(), map(&[("keep", file("k2"))]));
}

#[test]
fn release_and_sweep_backstops() {
    let fx = Fx::new();
    fx.put("m", "0");
    fx.history();
    // A write that no close follows (a shared mapping, say): recorded
    // at RELEASE.
    let fh = fx.open("m", O_RDWR);
    fx.ops.write(fh, 0, b"12", C).unwrap();
    fx.ops.release(fh).unwrap();
    assert_eq!(fx.history()["m"], file("12"));
    let triggers = fx.committer.history().head().unwrap().unwrap();
    assert!(
        triggers
            .message()
            .unwrap()
            .contains("Agentfs-Trigger: release"),
        "{:?}",
        triggers.message()
    );

    // A file held open and changed: recorded by the sweep.
    let fh = fx.open("m", O_WRONLY | O_APPEND);
    fx.ops.write(fh, 2, b"345", C).unwrap();
    let report = fx.ops.sweep(false);
    assert_eq!(report.recorded, 1);
    assert_eq!(fx.history()["m"], file("12345"));
    assert_eq!(fx.ops.sweep(false).recorded, 0, "unchanged since");
    fx.close(fh);
    assert_eq!(fx.backlog(), 0);
}

#[test]
fn stale_backing_inodes_are_detected() {
    let fx = Fx::new();
    fx.put("f", "1");
    let id = fx.id("f");
    // Replaced behind the mount's back. Holding the old inode open keeps
    // the filesystem from reusing its number (ext4 would).
    let _old = fs::File::open(fx.backing().join("f")).unwrap();
    fs::remove_file(fx.backing().join("f")).unwrap();
    fs::write(fx.backing().join("f"), "other").unwrap();
    assert_eq!(fx.ops.getattr(id, None), Err(estale()));
    assert_eq!(fx.ops.open(id, O_RDONLY, C).map(|_| ()), Err(estale()));
}

#[test]
fn symlinked_components_never_escape() {
    let fx = Fx::new();
    fs::write(fx.dir.path().join("outside"), "secret").unwrap();
    fx.mkdir("d");
    fx.put("d/f", "x");
    let f = fx.id("d/f");
    // Swap the directory for a symlink to outside the tree.
    fs::rename(fx.backing().join("d"), fx.backing().join("d.real")).unwrap();
    std::os::unix::fs::symlink(fx.dir.path(), fx.backing().join("d")).unwrap();
    assert!(fx.ops.open(f, O_RDONLY, C).is_err());
    // And a lookup through it sees a symlink, not a directory.
    let d = fx.ops.lookup(ROOT, b"d").unwrap();
    assert_eq!(d.attr.kind, FileType::Symlink);
    assert!(fx.ops.lookup(d.id, b"outside").is_err());
}

#[test]
fn readdir_lists_backing_entries() {
    let fx = Fx::new();
    fx.put("a", "");
    fx.mkdir("d");
    let a = fx.id("a");
    let fh = fx.ops.opendir(ROOT).unwrap();
    let all = fx.ops.readdir(fh, 0).unwrap();
    let mut names: Vec<(String, FileType)> = all
        .iter()
        .map(|(_, e)| (String::from_utf8_lossy(&e.name).into_owned(), e.kind))
        .collect();
    names.sort_by(|x, y| x.0.cmp(&y.0));
    assert_eq!(
        names,
        [
            (".".into(), FileType::Directory),
            ("..".into(), FileType::Directory),
            ("a".into(), FileType::RegularFile),
            ("d".into(), FileType::Directory),
        ]
    );
    let entry_a = all.iter().find(|(_, e)| e.name == b"a").unwrap();
    assert_eq!(entry_a.1.id, a, "d_ino matches st_ino");
    // Offsets resume after the entry they belong to.
    let (off, _) = &all[1];
    let rest = fx.ops.readdir(fh, *off).unwrap();
    assert_eq!(rest.len(), all.len() - 2);
    assert!(fx.ops.readdir(fh, 100).unwrap().is_empty());
    fx.ops.releasedir(fh);
    assert_eq!(fx.ops.readdir(fh, 0).map(|_| ()), Err(Errno::EBADF));
}

#[test]
fn xattrs_pass_through() {
    let fx = Fx::new();
    fx.put("f", "x");
    fx.history();
    let id = fx.id("f");
    match fx.ops.setxattr(id, b"user.agentfs-test", b"value", 0) {
        Ok(()) => {}
        // The temporary filesystem may not support user xattrs.
        Err(e) if e == errno(rustix::io::Errno::OPNOTSUPP) => return,
        Err(e) => panic!("{e:?}"),
    }
    assert_eq!(
        fx.ops.getxattr(id, b"user.agentfs-test", 0).unwrap(),
        Xattr::Size(5)
    );
    assert_eq!(
        fx.ops.getxattr(id, b"user.agentfs-test", 64).unwrap(),
        Xattr::Data(b"value".to_vec())
    );
    let Xattr::Data(list) = fx.ops.listxattr(id, 1024).unwrap() else {
        panic!()
    };
    assert!(list.windows(17).any(|w| w == b"user.agentfs-test"));
    fx.ops.removexattr(id, b"user.agentfs-test").unwrap();
    assert!(fx.ops.getxattr(id, b"user.agentfs-test", 64).is_err());
    assert_eq!(fx.backlog(), 0);
    let l = fx.ops.symlink(ROOT, b"l", b"f", C).unwrap();
    assert!(fx.ops.getxattr(l.id, b"user.x", 0).is_err());
}

#[test]
fn created_objects_keep_the_requested_mode() {
    let fx = Fx::new();
    // Not root: ownership stays the daemon's; the mode is exact even with
    // a umask that would clear group write.
    let (parent, _) = fx.split("x");
    let (entry, fh, _) = fx.ops.create(parent, b"g", 0o100664, O_WRONLY, C).unwrap();
    fx.close(fh);
    assert_eq!(u32::from(entry.attr.perm), 0o664);
    let d = fx.ops.mkdir(ROOT, b"dir", 0o775, C).unwrap();
    assert_eq!(
        fs::metadata(fx.backing().join("dir"))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777,
        0o775
    );
    let _ = d;
    let me = rustix::process::geteuid().as_raw();
    assert_eq!(fs::metadata(fx.backing().join("g")).unwrap().uid(), me);
}

#[test]
fn mknod_regular_files_are_recorded_fifos_are_not() {
    let fx = Fx::new();
    fx.ops.mknod(ROOT, b"reg", 0o100644, 0, C).unwrap();
    fx.ops.mknod(ROOT, b"fifo", 0o010644, 0, C).unwrap();
    assert_eq!(fx.history(), map(&[("reg", file(""))]));
    // Opening a FIFO's attributes still works.
    let fifo = fx.id("fifo");
    assert_eq!(
        fx.ops.getattr(fifo, None).unwrap().kind,
        FileType::NamedPipe
    );
}

#[test]
fn names_are_checked() {
    let fx = Fx::new();
    for bad in [&b".."[..], b".", b"", b"a/b"] {
        assert_eq!(fx.ops.lookup(ROOT, bad).map(|_| ()), Err(Errno::EINVAL));
    }
}

#[test]
fn forget_drops_nodes() {
    let fx = Fx::new();
    fx.put("f", "x");
    let id = fx.id("f");
    // create and lookup each counted once.
    fx.ops.forget(id, 2);
    assert!(fx.ops.table().get(id).is_none());
    assert_eq!(fx.id("f"), id, "the same backing inode keeps its id");
}

#[test]
fn rules_still_apply() {
    let fx = Fx::new();
    let exclude = fx.state().git().join("info/exclude");
    fs::create_dir_all(exclude.parent().unwrap()).unwrap();
    fs::write(&exclude, "*.log\n").unwrap();
    // Rules are read when the volume opens.
    let dir = {
        let Fx { dir, .. } = fx;
        dir
    };
    let fx = Fx::open_on(
        dir,
        OpsConfig {
            granularity_ns: 0,
            chunk: RENAME_CHUNK,
        },
    );
    fx.put("a.log", "x");
    fx.put("a.txt", "y");
    assert_eq!(fx.history(), map(&[("a.txt", file("y"))]));
}

#[test]
fn final_sweep_covers_read_only_handles_of_armed_inodes() {
    let fx = Fx::new();
    fx.put("f", "1");
    fx.history();
    let w = fx.open("f", O_WRONLY | O_APPEND);
    let r = fx.open("f", O_RDONLY);
    fx.ops.write(w, 1, b"2", C).unwrap();
    // The writer's handle goes without a flush; the reader stays open.
    let id = fx.ops.handle(w).unwrap().node;
    {
        let h = fx.ops.handles().remove(&w).unwrap();
        let mut table = fx.ops.table();
        let node = table.get_mut(id).unwrap();
        node.handles.retain(|f| *f != w);
        node.write_handles -= 1;
        drop(h);
    }
    assert_eq!(fx.ops.sweep(false).recorded, 0, "no write handle left");
    assert_eq!(fx.ops.sweep(true).recorded, 1);
    assert_eq!(fx.history()["f"], file("12"));
    fx.close(r);
}
