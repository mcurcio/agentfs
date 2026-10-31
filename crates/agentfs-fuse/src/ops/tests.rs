//! `Ops` against a real backing directory and a real state area, without
//! the kernel: every boundary of design §3.1 and the backstops.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{FileExt, MetadataExt, PermissionsExt};
use std::path::PathBuf;
use std::sync::Arc;

use agentfs_core::events::{Event, MemorySink};
use agentfs_core::identity::Context;
use agentfs_core::telemetry::Telemetry;
use agentfs_core::volume::{self, Committer, StateDir};

use super::*;
use crate::creds::Supplementary;

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
            .mkdir(parent, name.as_bytes(), 0o755, Actor::of_process(C))
            .unwrap()
            .id
    }

    fn create(&self, path: &str) -> (u64, u64) {
        let (parent, name) = self.split(path);
        let (entry, fh, _) = self
            .ops
            .create(
                parent,
                name.as_bytes(),
                0o100644,
                O_WRONLY | 0o100,
                Actor::of_process(C),
            )
            .unwrap();
        (entry.id, fh)
    }

    fn open(&self, path: &str, flags: i32) -> u64 {
        let id = self.id(path);
        self.ops.open(id, flags, Actor::of_process(C)).unwrap().0
    }

    /// create, write, close (flush and release).
    fn put(&self, path: &str, content: &str) {
        let (_, fh) = self.create(path);
        self.ops
            .write(fh, 0, content.as_bytes(), Actor::of_process(C))
            .unwrap();
        self.ops.flush(fh, Actor::of_process(C)).unwrap();
        self.ops.release(fh).unwrap();
    }

    fn close(&self, fh: u64) {
        self.ops.flush(fh, Actor::of_process(C)).unwrap();
        self.ops.release(fh).unwrap();
    }

    fn rename(&self, from: &str, to: &str, flags: u32) -> OpResult<()> {
        let (p1, n1) = self.split(from);
        let (p2, n2) = self.split(to);
        self.ops.rename(
            p1,
            n1.as_bytes(),
            p2,
            n2.as_bytes(),
            flags,
            Actor::of_process(C),
        )
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
    fx.ops.write(fh, 0, b"hello", Actor::of_process(C)).unwrap();
    fx.ops.flush(fh, Actor::of_process(C)).unwrap();
    assert_eq!(fx.backlog(), 1);
    // A second close with nothing new records nothing.
    fx.ops.flush(fh, Actor::of_process(C)).unwrap();
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
    fx.ops.write(fh, 3, b"two", Actor::of_process(C)).unwrap();
    fx.close(fh);
    assert_eq!(fx.history(), map(&[("f", file("onetwo"))]));

    // truncate(1) by path: a setattr boundary.
    let id = fx.id("f");
    let set = SetAttr {
        size: Some(3),
        ..SetAttr::default()
    };
    fx.ops.setattr(id, &set, Actor::of_process(C)).unwrap();
    assert_eq!(fx.history(), map(&[("f", file("one"))]));

    // ftruncate on an open handle.
    let fh = fx.open("f", O_RDWR);
    let set = SetAttr {
        size: Some(1),
        fh: Some(fh),
        ..SetAttr::default()
    };
    fx.ops.setattr(id, &set, Actor::of_process(C)).unwrap();
    assert_eq!(fx.backlog(), 1);
    fx.close(fh);
    assert_eq!(fx.backlog(), 1, "nothing changed after the truncate");
    assert_eq!(fx.history(), map(&[("f", file("o"))]));

    // `>` on an existing file: O_TRUNC at open, one record at close.
    let fh = fx.open("f", O_WRONLY | O_TRUNC);
    fx.ops.write(fh, 0, b"new", Actor::of_process(C)).unwrap();
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
        fx.ops
            .write(fh, 0, content.as_bytes(), Actor::of_process(C))
            .unwrap();
        fx.close(fh);
        assert_eq!(fx.history(), map(&[("f", file(content))]), "{content}");
    }
}

/// A fixture whose file timestamps (this thread's stamps) and racy window
/// are both far coarser than the test: every record is racy, and a write
/// of the same size leaves the stamp unchanged, as a write within one tick
/// of the kernel's coarse clock does (design §3.1).
fn coarse_clock() -> Fx {
    const TICK: i128 = 1_000_000 * 1_000_000_000;
    crate::dirty::TEST_TICK_NS.with(|t| t.set(TICK));
    Fx::with(OpsConfig {
        granularity_ns: 2 * TICK,
        chunk: RENAME_CHUNK,
    })
}

/// Writes `content` over the backing file at `path` behind the daemon's
/// back, as a write through a shared mapping does.
fn write_behind(fx: &Fx, path: &str, content: &str) {
    fs::OpenOptions::new()
        .write(true)
        .open(fx.backing().join(path))
        .unwrap()
        .write_all_at(content.as_bytes(), 0)
        .unwrap();
}

/// Design §3.1 and §19.2 item 4: a file written and closed within the
/// racy-timestamp window is recorded once. RELEASE finds its stamp
/// unchanged but racy, compares its bytes with the copy FLUSH froze, and
/// skips the record since they are equal and that copy is still the last
/// state journaled at the path.
#[test]
fn a_racy_close_is_not_recorded_again_at_release() {
    let fx = coarse_clock();
    let (_, fh) = fx.create("a");
    fx.ops.write(fh, 0, b"hello", Actor::of_process(C)).unwrap();
    fx.ops.flush(fh, Actor::of_process(C)).unwrap();
    assert_eq!(fx.backlog(), 1, "flush records the close");
    fx.ops.release(fh).unwrap();
    assert_eq!(fx.backlog(), 1, "release proves the bytes recorded");
    assert_eq!(fx.history(), map(&[("a", file("hello"))]));
}

/// A boundary that proves a racy record's bytes unchanged recomputes
/// `racy` as a record would: once the window has passed, later boundaries
/// trust the stamp and stop comparing, and the pinned copy is let go.
#[test]
fn a_proven_racy_skip_clears_racy_once_the_window_passed() {
    const TICK: i128 = 200_000_000;
    crate::dirty::TEST_TICK_NS.with(|t| t.set(TICK));
    let fx = Fx::with(OpsConfig {
        granularity_ns: 2 * TICK,
        chunk: RENAME_CHUNK,
    });
    let (id, fh) = fx.create("a");
    fx.ops.write(fh, 0, b"hello", Actor::of_process(C)).unwrap();
    fx.ops.flush(fh, Actor::of_process(C)).unwrap();
    assert_eq!(fx.backlog(), 1);
    let racy = |fx: &Fx| {
        let table = fx.ops.table();
        let node = table.get(id).unwrap();
        (node.dirty.racy, node.dirty.frozen.is_some())
    };
    assert_eq!(racy(&fx), (true, true), "recorded within the window");
    std::thread::sleep(std::time::Duration::from_nanos(3 * TICK as u64));
    fx.ops.flush(fh, Actor::of_process(C)).unwrap();
    assert_eq!(fx.backlog(), 1, "proven unchanged: skipped");
    assert_eq!(racy(&fx), (false, false), "and no longer racy");
    fx.ops.release(fh).unwrap();
    assert_eq!(fx.backlog(), 1);
    assert_eq!(fx.history(), map(&[("a", file("hello"))]));
}

/// The race the RELEASE record is for: a write in the same tick as the
/// close's record (through a shared mapping) leaves the stamp unchanged;
/// the bytes differ from the frozen copy, so RELEASE records them.
#[test]
fn a_same_tick_write_after_a_racy_close_is_recorded_at_release() {
    let fx = coarse_clock();
    let (_, fh) = fx.create("a");
    fx.ops.write(fh, 0, b"hello", Actor::of_process(C)).unwrap();
    fx.ops.flush(fh, Actor::of_process(C)).unwrap();
    let before = fs::metadata(fx.backing().join("a")).unwrap();
    write_behind(&fx, "a", "jello");
    let after = fs::metadata(fx.backing().join("a")).unwrap();
    assert_eq!(before.size(), after.size());
    fx.ops.release(fh).unwrap();
    assert_eq!(fx.backlog(), 2, "the same-tick write is recorded");
    assert_eq!(fx.history(), map(&[("a", file("jello"))]));
}

/// The same, with a close in between: a second FLUSH that finds the stamp
/// unchanged but racy compares too.
#[test]
fn a_same_tick_write_is_recorded_at_the_next_close() {
    let fx = coarse_clock();
    let (_, fh) = fx.create("a");
    fx.ops.write(fh, 0, b"hello", Actor::of_process(C)).unwrap();
    fx.ops.flush(fh, Actor::of_process(C)).unwrap();
    fx.ops.flush(fh, Actor::of_process(C)).unwrap();
    assert_eq!(fx.backlog(), 1, "an unchanged racy close records nothing");
    write_behind(&fx, "a", "jello");
    fx.ops.flush(fh, Actor::of_process(C)).unwrap();
    assert_eq!(fx.backlog(), 2);
    fx.ops.release(fh).unwrap();
    assert_eq!(fx.backlog(), 2);
    assert_eq!(fx.history(), map(&[("a", file("jello"))]));
}

/// A later record of the path by anything else (here the sweep's repair,
/// which freezes its own copy) means the pinned copy is no longer the last
/// state journaled: the next boundary (here the same sweep's check of the
/// open file) records, so history ends on the file's bytes whatever the
/// order of the two. That record pins its own copy, so RELEASE skips.
#[test]
fn release_records_when_the_pinned_copy_is_no_longer_last() {
    let fx = coarse_clock();
    let (_, fh) = fx.create("a");
    fx.ops.write(fh, 0, b"hello", Actor::of_process(C)).unwrap();
    fx.ops.flush(fh, Actor::of_process(C)).unwrap();
    fx.ops.boundary().queue_repair([b"a".to_vec()], C);
    fx.ops.sweep(false);
    assert_eq!(fx.backlog(), 3, "the repair, then the open file again");
    fx.ops.release(fh).unwrap();
    assert_eq!(fx.backlog(), 3, "the sweep's copy is the last");
    assert_eq!(fx.history(), map(&[("a", file("hello"))]));
}

/// The pinned copy outlives its removal by a commit in between.
#[test]
fn a_commit_between_close_and_release_does_not_lose_the_pin() {
    let fx = coarse_clock();
    let (_, fh) = fx.create("a");
    fx.ops.write(fh, 0, b"hello", Actor::of_process(C)).unwrap();
    fx.ops.flush(fh, Actor::of_process(C)).unwrap();
    assert_eq!(fx.history(), map(&[("a", file("hello"))]));
    fx.ops.release(fh).unwrap();
    assert_eq!(fx.backlog(), 0, "nothing recorded again");
    // And a same-tick write after a commit is still caught.
    let fh = fx.open("a", O_WRONLY);
    fx.ops.write(fh, 0, b"hallo", Actor::of_process(C)).unwrap();
    fx.ops.flush(fh, Actor::of_process(C)).unwrap();
    fx.history();
    write_behind(&fx, "a", "jallo");
    fx.ops.release(fh).unwrap();
    assert_eq!(fx.history(), map(&[("a", file("jallo"))]));
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
    fx.ops
        .setattr(id, &chmod(0o755), Actor::of_process(C))
        .unwrap();
    assert_eq!(fx.history(), map(&[("s", Item::Exec("#!/bin/sh".into()))]));
    // Other bits: passed through, not recorded.
    fx.ops
        .setattr(id, &chmod(0o700), Actor::of_process(C))
        .unwrap();
    assert_eq!(fx.backlog(), 0);
    fx.ops
        .setattr(id, &chmod(0o600), Actor::of_process(C))
        .unwrap();
    assert_eq!(fx.history(), map(&[("s", file("#!/bin/sh"))]));
    fx.ops
        .setattr(id, &chmod(0o400), Actor::of_process(C))
        .unwrap();
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
    fx.ops.setattr(id, &set, Actor::of_process(C)).unwrap();
    let when = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
    let set = SetAttr {
        mtime: Some(TimeOrNow::SpecificTime(when)),
        atime: Some(TimeOrNow::Now),
        ..SetAttr::default()
    };
    let attr = fx.ops.setattr(id, &set, Actor::of_process(C)).unwrap();
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
    fx.ops.symlink(p, b"l", b"y", Actor::of_process(C)).unwrap();
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
    fx.ops.write(fh, 1, b"2", Actor::of_process(C)).unwrap();
    fx.close(fh);
    assert_eq!(fx.history()["moved/x"], file("x2"));
}

#[test]
fn a_directory_rename_is_one_record() {
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
    assert_eq!(fx.backlog(), 1, "one move, whatever is beneath");
    let history = fx.history();
    assert_eq!(history.len(), 8);
    assert!(
        history.keys().all(|k| k.starts_with("big2/")),
        "{history:?}"
    );
}

#[test]
fn large_directory_exchanges_are_chunked() {
    let fx = Fx::with(OpsConfig {
        granularity_ns: 0,
        chunk: 3,
    });
    fx.mkdir("a");
    fx.mkdir("b");
    for i in 0..5 {
        fx.put(&format!("a/f{i}"), "a");
    }
    for i in 0..3 {
        fx.put(&format!("b/g{i}"), "b");
    }
    fx.history();
    fx.rename("a", "b", RenameFlags::EXCHANGE.bits()).unwrap();
    assert_eq!(fx.backlog(), 3, "8 files in chunks of 3");
    let history = fx.history();
    assert_eq!(history.len(), 8);
    assert!(history.keys().filter(|k| k.starts_with("b/f")).count() == 5);
    assert!(history.keys().filter(|k| k.starts_with("a/g")).count() == 3);
}

#[test]
fn a_write_in_flight_across_a_directory_rename_lands_at_the_new_path() {
    let fx = Fx::new();
    fx.mkdir("d");
    fx.put("d/x", "1");
    fx.history();
    let fh = fx.open("d/x", O_WRONLY | O_APPEND);
    fx.ops.write(fh, 1, b"2", Actor::of_process(C)).unwrap();
    fx.rename("d", "m", 0).unwrap();
    fx.close(fh);
    assert_eq!(fx.history(), map(&[("m/x", file("12"))]));
}

#[test]
fn directory_renames_follow_the_rules_at_the_new_path() {
    let fx = Fx::new();
    let exclude = fx.state().git().join("info/exclude");
    fs::create_dir_all(exclude.parent().unwrap()).unwrap();
    fs::write(&exclude, "ignored/\n").unwrap();
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
    fx.mkdir("src");
    fx.put("src/a", "a");
    fx.mkdir("ignored");
    fx.put("ignored/b", "b");
    assert_eq!(fx.history(), map(&[("src/a", file("a"))]));
    // Into an ignored directory: dropped, as recording it there would.
    fx.rename("src", "ignored/src", 0).unwrap();
    assert_eq!(fx.history(), map(&[]));
    // An ignored directory renamed to where nothing ignores it: recorded.
    fx.rename("ignored", "kept", 0).unwrap();
    assert_eq!(
        fx.history(),
        map(&[("kept/b", file("b")), ("kept/src/a", file("a"))])
    );
    // Within an ignored tree nothing is journaled at all.
    fx.mkdir("kept/ignored");
    fx.mkdir("kept/ignored/x");
    fx.put("kept/ignored/x/c", "c");
    fx.history();
    fx.rename("kept/ignored/x", "kept/ignored/y", 0).unwrap();
    assert_eq!(fx.backlog(), 0);
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
    fx.ops.link(id, ROOT, b"a2", Actor::of_process(C)).unwrap();
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
    fx.ops.unlink(e, b"f", Actor::of_process(C)).unwrap();
    let d = fx.id("d");
    fx.ops.rmdir(d, b"e", Actor::of_process(C)).unwrap();
    fx.ops.rmdir(ROOT, b"d", Actor::of_process(C)).unwrap();
    assert_eq!(fx.history(), map(&[("keep", file("k"))]));
    fx.ops.unlink(ROOT, b"keep", Actor::of_process(C)).unwrap();
    assert_eq!(fx.history(), map(&[]));
    assert_eq!(
        fx.ops.unlink(ROOT, b"keep", Actor::of_process(C)),
        Err(Errno::ENOENT)
    );
}

#[test]
fn hard_links() {
    let fx = Fx::new();
    fx.put("a", "1");
    let id = fx.id("a");
    let entry = fx.ops.link(id, ROOT, b"b", Actor::of_process(C)).unwrap();
    assert_eq!(entry.id, id, "one inode, one node, one st_ino");
    assert_eq!(fx.history(), map(&[("a", file("1")), ("b", file("1"))]));
    // A write through one name is recorded at both.
    let fh = fx.open("b", O_WRONLY | O_APPEND);
    fx.ops.write(fh, 1, b"2", Actor::of_process(C)).unwrap();
    fx.close(fh);
    assert_eq!(fx.history(), map(&[("a", file("12")), ("b", file("12"))]));
}

#[test]
fn symlinks() {
    let fx = Fx::new();
    let entry = fx
        .ops
        .symlink(ROOT, b"l", b"some/target", Actor::of_process(C))
        .unwrap();
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
    assert!(
        fx.ops
            .setattr(entry.id, &set, Actor::of_process(C))
            .is_err()
    );
}

#[test]
fn unlink_of_an_open_file() {
    let fx = Fx::new();
    fx.put("f", "x");
    fx.history();
    // exec 3>>f; rm f; echo y >&3; exec 3>&-
    let fh = fx.open("f", O_WRONLY | O_APPEND);
    fx.ops.unlink(ROOT, b"f", Actor::of_process(C)).unwrap();
    fx.ops.write(fh, 1, b"y", Actor::of_process(C)).unwrap();
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
    fx.ops.write(fh, 3, b"!", Actor::of_process(C)).unwrap();
    fx.close(fh);
    assert_eq!(fx.history(), map(&[("f", file("fresh"))]));
}

#[test]
fn log_rotation_with_the_writer_open() {
    let fx = Fx::new();
    fx.put("app.log", "a\n");
    let fh = fx.open("app.log", O_WRONLY | O_APPEND);
    fx.ops.write(fh, 2, b"b\n", Actor::of_process(C)).unwrap();
    fx.rename("app.log", "app.log.1", 0).unwrap();
    fx.put("app.log", "new\n");
    // The writer still holds the rotated file.
    fx.ops
        .write(fh, 4, b"late\n", Actor::of_process(C))
        .unwrap();
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
    fx.put("sub/.git/config", "[core]\n\trepositoryformatversion = 0\n");
    fx.rename(".git/HEAD", ".git/HEAD2", 0).unwrap();
    let git = fx.id(".git");
    fx.ops.unlink(git, b"HEAD2", Actor::of_process(C)).unwrap();
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
        .create(ROOT, b"bad\xff", 0o100644, O_WRONLY, Actor::of_process(C))
        .unwrap();
    fx.ops.write(fh, 0, b"data", Actor::of_process(C)).unwrap();
    fx.close(fh);
    assert_eq!(fx.refused().len(), 1, "{:?}", fx.refused());
    assert_eq!(fx.history(), map(&[]));
    assert_eq!(fx.ops.getattr(entry.id, None).unwrap().size, 4);
    // A symlink whose target is not UTF-8 is refused too.
    fx.ops
        .symlink(ROOT, b"l", b"\xfe", Actor::of_process(C))
        .unwrap();
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
    fx.ops.write(fh, 1, b"2", Actor::of_process(C)).unwrap();
    lock(0o500);
    assert_eq!(fx.ops.flush(fh, Actor::of_process(C)), Err(Errno::EIO));
    assert!(
        fx.sink
            .snapshot()
            .iter()
            .any(|e| matches!(e, Event::RecordFailed { .. }))
    );
    lock(0o700);
    // Still dirty: the next close records it.
    fx.ops.flush(fh, Actor::of_process(C)).unwrap();
    fx.ops.release(fh).unwrap();
    assert_eq!(fx.history()["keep"], file("k2"));

    // A namespace operation that already happened is repaired by the sweep.
    fx.ops
        .unlink(ROOT, b"gone", Actor::of_process(C))
        .unwrap_or(());
    fx.put("gone", "again");
    fx.history();
    // Make the pending directory unreadable: the journal syncs it before
    // every write, so even a removal fails to journal.
    fs::set_permissions(&pending, fs::Permissions::from_mode(0o000)).unwrap();
    assert_eq!(
        fx.ops.unlink(ROOT, b"gone", Actor::of_process(C)),
        Err(Errno::EIO)
    );
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
    fx.ops.write(fh, 0, b"12", Actor::of_process(C)).unwrap();
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
    fx.ops.write(fh, 2, b"345", Actor::of_process(C)).unwrap();
    let report = fx.ops.sweep(false);
    assert_eq!(report.recorded, 1);
    assert_eq!(fx.history()["m"], file("12345"));
    assert_eq!(fx.ops.sweep(false).recorded, 0, "unchanged since");
    fx.close(fh);
    assert_eq!(fx.backlog(), 0);
}

/// A writer that shared a passthrough backing id (design §17.1): its
/// mapping may write after RELEASE without the daemon seeing it, so the
/// node is swept with no handle open and recorded at FORGET.
#[test]
fn a_mapping_that_may_write_unseen_is_watched_until_forget() {
    let fx = Fx::new();
    fx.put("m", "0");
    fx.put("plain", "0");
    fx.history();
    let id = fx.id("m");
    let fh = fx.open("m", O_RDWR);
    fx.ops.mapping_may_write(id);
    fx.close(fh);
    let plain = fx.open("plain", O_RDWR);
    fx.close(plain);
    // Written through the mapping after the last RELEASE.
    fs::write(fx.backing().join("m"), "11").unwrap();
    fs::write(fx.backing().join("plain"), "11").unwrap();
    assert_eq!(fx.ops.sweep(false).recorded, 1);
    assert_eq!(fx.history()["m"], file("11"));
    // Only the watched node: a plain one is recorded by its next boundary.
    assert_eq!(fx.history()["plain"], file("0"));
    assert_eq!(fx.ops.sweep(false).recorded, 0, "unchanged since");
    // Written again; the kernel forgets the node only once no mapping is
    // left, so FORGET records it.
    fs::write(fx.backing().join("m"), "222").unwrap();
    fx.ops.forget(id, u64::MAX);
    assert_eq!(fx.history()["m"], file("222"));
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
    assert_eq!(
        fx.ops.open(id, O_RDONLY, Actor::of_process(C)).map(|_| ()),
        Err(estale())
    );
}

#[test]
fn names_another_process_moved_away_are_dropped_at_lookup() {
    // Another front of the same backing tree (design §14) renames a file
    // this mount knows by its old name.
    let fx = Fx::new();
    fx.put("old", "1");
    let id = fx.id("old");
    fs::rename(fx.backing().join("old"), fx.backing().join("new")).unwrap();
    assert_eq!(fx.id("new"), id);
    // The old name is gone from the node: it opens, and records, by the
    // new one.
    let fh = fx.open("new", O_WRONLY | O_TRUNC);
    fx.ops.write(fh, 0, b"2", Actor::of_process(C)).unwrap();
    fx.close(fh);
    assert_eq!(fx.history().get("new"), Some(&file("2")));
    // A real hard link keeps both names.
    fs::hard_link(fx.backing().join("new"), fx.backing().join("link")).unwrap();
    assert_eq!(fx.id("link"), id);
    assert_eq!(fx.id("new"), id);
    let fh = fx.open("new", O_WRONLY | O_TRUNC);
    fx.ops.write(fh, 0, b"3", Actor::of_process(C)).unwrap();
    fx.close(fh);
    let history = fx.history();
    assert_eq!(history.get("new"), Some(&file("3")), "{history:?}");
    assert_eq!(history.get("link"), Some(&file("3")), "{history:?}");
}

#[test]
fn an_open_file_another_process_renamed_is_recorded_at_its_new_name() {
    // Another front renames a file open here (design §14); its own boundary
    // records the rename. This mount's close must not record the old name.
    let fx = Fx::new();
    fx.put("old", "1");
    let fh = fx.open("old", O_WRONLY | O_TRUNC);
    fs::rename(fx.backing().join("old"), fx.backing().join("new")).unwrap();
    fx.ops.write(fh, 0, b"2", Actor::of_process(C)).unwrap();
    fx.close(fh);
    let history = fx.history();
    assert_eq!(history.get("old"), Some(&file("1")), "{history:?}");
    assert_eq!(history.get("new"), Some(&file("2")), "{history:?}");
    // Its directory moved instead: recorded under the new directory.
    fx.mkdir("d");
    fx.put("d/f", "1");
    let fh = fx.open("d/f", O_WRONLY | O_TRUNC);
    fs::rename(fx.backing().join("d"), fx.backing().join("e")).unwrap();
    fx.ops.write(fh, 0, b"2", Actor::of_process(C)).unwrap();
    fx.close(fh);
    let history = fx.history();
    assert_eq!(history.get("d/f"), Some(&file("1")), "{history:?}");
    assert_eq!(history.get("e/f"), Some(&file("2")), "{history:?}");
    // The sweep of an open, changed file too.
    fx.put("s", "1");
    let fh = fx.open("s", O_WRONLY | O_TRUNC);
    fs::rename(fx.backing().join("s"), fx.backing().join("t")).unwrap();
    fx.ops.write(fh, 0, b"2", Actor::of_process(C)).unwrap();
    assert_eq!(fx.ops.sweep(false).recorded, 1);
    let history = fx.history();
    assert_eq!(history.get("s"), Some(&file("1")), "{history:?}");
    assert_eq!(history.get("t"), Some(&file("2")), "{history:?}");
    fx.close(fh);
}

#[test]
fn an_open_file_another_process_removed_is_recorded_nowhere() {
    // Another front removes a file open here and records the removal
    // (design §14); this mount's close must not bring the path back.
    let fx = Fx::new();
    fx.put("x", "1");
    let fh = fx.open("x", O_WRONLY | O_TRUNC);
    fs::remove_file(fx.backing().join("x")).unwrap();
    fx.ops.write(fh, 0, b"2", Actor::of_process(C)).unwrap();
    fx.close(fh);
    assert_eq!(fx.history(), map(&[("x", file("1"))]));
    // Removed, and the name reused for another file.
    fx.put("y", "1");
    let fh = fx.open("y", O_WRONLY | O_TRUNC);
    fs::remove_file(fx.backing().join("y")).unwrap();
    fs::write(fx.backing().join("y"), "other").unwrap();
    fx.ops.write(fh, 0, b"2", Actor::of_process(C)).unwrap();
    fx.close(fh);
    assert_eq!(fx.history().get("y"), Some(&file("1")));
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
    assert!(fx.ops.open(f, O_RDONLY, Actor::of_process(C)).is_err());
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
    let l = fx
        .ops
        .symlink(ROOT, b"l", b"f", Actor::of_process(C))
        .unwrap();
    assert!(fx.ops.getxattr(l.id, b"user.x", 0).is_err());
}

#[test]
fn created_objects_keep_the_requested_mode() {
    let fx = Fx::new();
    // The mode is exact even with a umask that would clear group write.
    let (parent, _) = fx.split("x");
    let (entry, fh, _) = fx
        .ops
        .create(parent, b"g", 0o100664, O_WRONLY, Actor::of_process(C))
        .unwrap();
    fx.close(fh);
    assert_eq!(u32::from(entry.attr.perm), 0o664);
    let d = fx
        .ops
        .mkdir(ROOT, b"dir", 0o775, Actor::of_process(C))
        .unwrap();
    assert_eq!(
        fs::metadata(fx.backing().join("dir"))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777,
        0o775
    );
    let _ = d;
    let g = fs::metadata(fx.backing().join("g")).unwrap();
    if crate::privileges::Privileges::current().can_create_as_caller() {
        // Root (or the same capabilities): objects are created as their
        // caller, so the backing file is the caller's.
        assert_eq!((g.uid(), g.gid()), (C.uid, C.gid));
        let dir = fs::metadata(fx.backing().join("dir")).unwrap();
        assert_eq!((dir.uid(), dir.gid()), (C.uid, C.gid));
    } else {
        // Not root: ownership stays the daemon's.
        let me = rustix::process::geteuid().as_raw();
        assert_eq!(g.uid(), me);
    }
}

#[test]
fn mknod_regular_files_are_recorded_fifos_are_not() {
    let fx = Fx::new();
    fx.ops
        .mknod(ROOT, b"reg", 0o100644, 0, Actor::of_process(C))
        .unwrap();
    fx.ops
        .mknod(ROOT, b"fifo", 0o010644, 0, Actor::of_process(C))
        .unwrap();
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
    fx.ops.write(w, 1, b"2", Actor::of_process(C)).unwrap();
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

// ---- review fixes: confinement, races, the repair queue ----

fn mode_of_path(p: &std::path::Path) -> u32 {
    fs::symlink_metadata(p).unwrap().permissions().mode() & 0o7777
}

#[test]
fn a_new_objects_name_swapped_for_a_symlink_or_link_is_never_followed() {
    let fx = Fx::new();
    let outside = fx.dir.path().join("outside");
    fs::write(&outside, "host file").unwrap();
    fs::set_permissions(&outside, fs::Permissions::from_mode(0o600)).unwrap();
    let dir = fx.ops.open_dir_node(ROOT).unwrap();
    let pst = rustix::fs::fstat(&dir).unwrap();
    // mkdir made `x`, then something replaced it with a symlink out of the
    // tree before the mode was set.
    std::os::unix::fs::symlink(&outside, fx.backing().join("x")).unwrap();
    for kind in [Kind::Dir, Kind::File, Kind::Other] {
        for as_caller in [false, true] {
            assert_eq!(
                fx.ops
                    .own_new(&dir, b"x", None, &pst, C, 0o4777, kind, as_caller),
                Err(estale())
            );
        }
    }
    assert_eq!(mode_of_path(&outside), 0o600);
    // mknod made `y`, then it was replaced by a hard link to another file.
    fs::hard_link(&outside, fx.backing().join("y")).unwrap();
    for as_caller in [false, true] {
        assert_eq!(
            fx.ops
                .own_new(&dir, b"y", None, &pst, C, 0o4777, Kind::File, as_caller),
            Err(estale())
        );
    }
    assert_eq!(mode_of_path(&outside), 0o600);
    // The real thing still works.
    fx.ops
        .mkdir(ROOT, b"d", 0o1777, Actor::of_process(C))
        .unwrap();
    assert_eq!(mode_of_path(&fx.backing().join("d")), 0o1777);
    fx.ops
        .mknod(ROOT, b"n", 0o100640, 0, Actor::of_process(C))
        .unwrap();
    assert_eq!(mode_of_path(&fx.backing().join("n")), 0o640);
}

#[test]
fn mkdir_and_mknod_race_a_symlink_swap_without_following_it() {
    let fx = Fx::new();
    let outside = fx.dir.path().join("outside");
    fs::write(&outside, "host file").unwrap();
    fs::set_permissions(&outside, fs::Permissions::from_mode(0o600)).unwrap();
    let backing = fx.backing();
    let ops = Arc::new(fx.ops);
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    // A writer changing the backing tree directly.
    let swapper = {
        let stop = stop.clone();
        let target = outside.clone();
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                for name in ["x", "y"] {
                    let p = backing.join(name);
                    let _ = fs::remove_dir(&p);
                    let _ = fs::remove_file(&p);
                    let _ = std::os::unix::fs::symlink(&target, &p);
                    let _ = fs::remove_file(&p);
                }
            }
        })
    };
    let start = std::time::Instant::now();
    while start.elapsed() < std::time::Duration::from_secs(2) {
        let _ = ops.mkdir(ROOT, b"x", 0o1777, Actor::of_process(C));
        let _ = ops.mknod(ROOT, b"y", 0o104777, 0, Actor::of_process(C));
        assert_eq!(
            mode_of_path(&outside),
            0o600,
            "a swapped-in symlink was followed"
        );
    }
    stop.store(true, Ordering::Relaxed);
    swapper.join().unwrap();
}

#[test]
fn getattr_never_fails_while_an_ancestor_is_renamed() {
    let fx = Fx::new();
    fx.mkdir("d");
    fx.put("d/f", "x");
    let f = fx.id("d/f");
    let ops = Arc::new(fx.ops);
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let renamer = {
        let ops = ops.clone();
        let stop = stop.clone();
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                ops.rename(ROOT, b"d", ROOT, b"d2", 0, Actor::of_process(C))
                    .unwrap();
                ops.rename(ROOT, b"d2", ROOT, b"d", 0, Actor::of_process(C))
                    .unwrap();
            }
        })
    };
    let start = std::time::Instant::now();
    let mut errors = 0;
    while start.elapsed() < std::time::Duration::from_secs(2) {
        if ops.getattr(f, None).is_err() {
            errors += 1;
        }
        if ops
            .listxattr(f, 0)
            .is_err_and(|e| e != errno(rustix::io::Errno::OPNOTSUPP))
        {
            errors += 1;
        }
    }
    stop.store(true, Ordering::Relaxed);
    renamer.join().unwrap();
    assert_eq!(errors, 0);
}

#[test]
fn create_over_an_existing_file_does_not_take_it_over() {
    let fx = Fx::new();
    let victim = fx.backing().join("victim");
    fs::write(&victim, "secret").unwrap();
    fs::set_permissions(&victim, fs::Permissions::from_mode(0o600)).unwrap();
    fx.history();
    let before = fx.backlog();
    // FUSE_CREATE after another writer made the name in the backing tree.
    let owner = Caller {
        uid: rustix::process::geteuid().as_raw(),
        gid: rustix::process::getegid().as_raw(),
        pid: 1,
    };
    let (_, fh, _) = fx
        .ops
        .create(ROOT, b"victim", 0o666, O_WRONLY, Actor::of_process(owner))
        .unwrap();
    assert_eq!(mode_of_path(&victim), 0o600, "mode rewritten");
    // Not marked new: closing it unchanged records nothing.
    fx.close(fh);
    assert_eq!(fx.backlog(), before);
    // An exclusive create fails as it would natively.
    let excl = 0o200;
    assert_eq!(
        fx.ops
            .create(
                ROOT,
                b"victim",
                0o666,
                O_WRONLY | excl,
                Actor::of_process(owner)
            )
            .err(),
        Some(Errno::EEXIST)
    );
    // Another user may not open it through create.
    let other = Caller {
        uid: owner.uid + 1,
        gid: owner.gid + 1,
        pid: 2,
    };
    assert_eq!(
        fx.ops
            .create(ROOT, b"victim", 0o666, O_RDWR, Actor::of_process(other))
            .err(),
        Some(Errno::EACCES)
    );
    // A FIFO in the way is not opened (that would block).
    rustix::fs::mknodat(
        rustix::fs::CWD,
        fx.backing().join("fifo"),
        rustix::fs::FileType::Fifo,
        Mode::from_raw_mode(0o666),
        0,
    )
    .unwrap();
    assert_eq!(
        fx.ops
            .create(ROOT, b"fifo", 0o666, O_RDONLY, Actor::of_process(C))
            .err(),
        Some(Errno::EEXIST)
    );
}

#[test]
fn a_dot_git_that_is_no_repository_raises_path_refused_once() {
    let fx = Fx::new();
    fx.mkdir("notes");
    fx.mkdir("notes/.git");
    fx.put("notes/.git/report.txt", "r");
    fx.put("notes/.git/more.txt", "m");
    assert_eq!(fx.refused(), ["notes/.git/report.txt"]);
    assert_eq!(fx.history(), map(&[]));
    // A file named .git that is no gitfile.
    fx.mkdir("plain");
    fx.put("plain/.git", "hello\n");
    assert_eq!(fx.refused(), ["notes/.git/report.txt", "plain/.git"]);
    // Removing such paths raises nothing.
    fx.ops
        .unlink(fx.id("plain"), b".git", Actor::of_process(C))
        .unwrap();
    assert_eq!(fx.refused().len(), 2);
}

#[test]
fn the_repair_queue_survives_the_process() {
    let fx = Fx::new();
    fx.put("big.log", "data");
    fx.put("keep", "k");
    assert_eq!(fx.history().len(), 2);
    let store = fx.state().repairs();
    fx.ops.boundary().persist_repairs(store.clone()).unwrap();
    // An unlink whose record failed: the file is gone from the tree, the
    // path is queued, and the process then ends without repairing it.
    fs::remove_file(fx.backing().join("big.log")).unwrap();
    fx.ops.boundary().queue_repair([b"big.log".to_vec()], C);
    assert!(store.exists());
    let Fx { dir, ops, .. } = fx;
    drop(ops);

    // The next mount loads the queue and repairs it from the backing tree.
    let fx = Fx::open_on(dir, OpsConfig::default());
    assert_eq!(fx.ops.boundary().persist_repairs(store.clone()).unwrap(), 1);
    let report = fx.ops.sweep(false);
    assert_eq!((report.repaired, report.still_queued), (1, 0));
    assert_eq!(fx.history(), map(&[("keep", file("k"))]));
    assert!(!store.exists(), "an empty queue leaves no file");
}

/// A crash after the new queue was written to the temporary file but
/// before the rename (failpoint `repairs.before_rename`): the next mount
/// loads the temporary file's complete lines too, and repairs them.
#[test]
fn a_queue_left_in_the_temporary_file_is_repaired() {
    let fx = Fx::new();
    fx.put("a.log", "a");
    fx.put("b.log", "b");
    fx.put("keep", "k");
    let store = fx.state().repairs();
    let tmp = store.with_extension("tmp");
    let Fx { dir, ops, .. } = fx;
    drop(ops);
    // Both removed from the tree; the old store names a.log, the new
    // queue in the temporary file names b.log, then a torn line.
    fs::remove_file(dir.path().join("backing/a.log")).unwrap();
    fs::remove_file(dir.path().join("backing/b.log")).unwrap();
    fs::write(&store, "0 0 1 612e6c6f67\n").unwrap();
    fs::write(&tmp, "0 0 1 622e6c6f67\n0 0 1 6b65").unwrap();

    let fx = Fx::open_on(dir, OpsConfig::default());
    assert_eq!(fx.ops.boundary().persist_repairs(store.clone()).unwrap(), 2);
    assert!(!tmp.exists(), "renamed over the store");
    let report = fx.ops.sweep(false);
    assert_eq!((report.repaired, report.still_queued), (2, 0));
    assert_eq!(fx.history(), map(&[("keep", file("k"))]));
    assert!(!store.exists() && !tmp.exists());
}

#[test]
fn an_inherited_repair_keeps_the_pod_that_failed_it() {
    // A slot's repair file left by a dead front (pod-a): its entries are
    // recorded as pod-a's, by a sweep (design §14.1).
    let fx = Fx::new();
    fx.put("a.log", "data");
    let store = fx.state().fronts_dir().join("slot-1.repairs");
    let origin = agentfs_core::record::Origin {
        namespace: "ns".into(),
        pod: "pod-a".into(),
        pod_uid: "uid-a".into(),
        node: "n".into(),
        replica: "pod-a".into(),
    };
    let json = serde_json::to_string(&origin).unwrap();
    fs::write(&store, format!("@origin {json}\n1000 1000 7 612e6c6f67\n")).unwrap();
    fs::write(fx.backing().join("a.log"), "changed").unwrap();
    assert_eq!(fx.ops.boundary().persist_repairs(store.clone()).unwrap(), 1);
    assert_eq!(
        fx.ops.boundary().repairs()[0].origin.as_ref(),
        Some(&origin)
    );
    let before = fx.journal().len();
    let report = fx.ops.sweep(false);
    assert_eq!((report.repaired, report.still_queued), (1, 0));
    let journal = fx.journal();
    assert_eq!(journal.len(), before + 1);
    let record = journal.last().unwrap();
    assert_eq!(record.draft.trigger, Trigger::Sweep);
    assert_eq!(record.draft.origin.as_ref(), Some(&origin));
    assert!(!store.exists());
}

#[test]
fn a_repair_never_drops_a_path_queued_again_meanwhile() {
    let fx = Fx::new();
    fx.ops.boundary().queue_repair([b"a".to_vec()], C);
    let listed = fx.ops.boundary().repairs();
    // Queued again (a newer failure) before the older repair finishes.
    fx.ops.boundary().queue_repair([b"a".to_vec()], C);
    fx.ops.boundary().repaired(&listed);
    assert_eq!(fx.ops.boundary().pending_repairs(), 1);
    let listed = fx.ops.boundary().repairs();
    fx.ops.boundary().repaired(&listed);
    assert_eq!(fx.ops.boundary().pending_repairs(), 0);
}

// ---- restores through the mount (design §15) ----

#[test]
fn open_for_writing_tracks_write_handles() {
    let fx = Fx::new();
    fx.put("f", "x");
    fx.mkdir("d");
    fx.put("d/g", "y");
    assert!(!fx.ops.open_for_writing(b"f"));
    let read = fx.open("f", O_RDONLY);
    assert!(!fx.ops.open_for_writing(b"f"), "a read handle");
    let write = fx.open("f", O_WRONLY);
    assert!(fx.ops.open_for_writing(b"f"));
    let nested = fx.open("d/g", O_RDWR);
    assert!(fx.ops.open_for_writing(b"d/g"));
    assert!(!fx.ops.open_for_writing(b"d"));
    assert!(!fx.ops.open_for_writing(b"unknown"));
    assert!(!fx.ops.open_for_writing(b"d/unknown/x"));
    fx.close(write);
    fx.close(nested);
    assert!(!fx.ops.open_for_writing(b"f"));
    assert!(!fx.ops.open_for_writing(b"d/g"));
    fx.close(read);
}

#[test]
fn records_by_a_restore_actor_belong_to_the_operation() {
    use agentfs_core::record::OperationKind;
    let fx = Fx::new();
    fx.put("a", "1");
    fx.put("b", "2");
    fx.history();
    let operator = Caller {
        uid: 0,
        gid: 0,
        pid: 999,
    };
    let actor = Caller { pid: 4321, ..C };
    let guard = fx
        .ops
        .boundary()
        .recorder()
        .begin_operation(
            OperationKind::Revert,
            "0123456789012345678901234567890123456789".into(),
            "revert 0123456: x".into(),
            operator,
            &[actor.pid],
        )
        .unwrap();
    // A write (close), a chmod +x (setattr) and an unlink by the actor.
    let fh = fx.open("a", O_WRONLY | O_TRUNC);
    fx.ops
        .write(fh, 0, b"restored", Actor::of_process(actor))
        .unwrap();
    fx.ops.flush(fh, Actor::of_process(actor)).unwrap();
    fx.ops.release(fh).unwrap();
    let id = fx.id("a");
    fx.ops
        .setattr(
            id,
            &SetAttr {
                mode: Some(0o100755),
                ..SetAttr::default()
            },
            Actor::of_process(actor),
        )
        .unwrap();
    fx.ops.unlink(ROOT, b"b", Actor::of_process(actor)).unwrap();
    // Someone else, meanwhile.
    fx.put("c", "3");
    let last = guard.finish();
    let journal: Vec<_> = agentfs_core::journal::read_records(&fx.state().journal())
        .unwrap()
        .0;
    let ours: Vec<_> = journal
        .iter()
        .filter(|r| r.draft.operation.is_some())
        .collect();
    assert_eq!(ours.len(), 3, "{journal:#?}");
    for r in &ours {
        assert_eq!(r.draft.trigger, Trigger::Restore);
        assert_eq!(r.draft.caller, operator);
    }
    assert_eq!(last, Some(ours[2].seq));
    let theirs = journal
        .iter()
        .find(|r| r.draft.operation.is_none())
        .unwrap();
    assert_eq!(theirs.draft.caller, C);
    assert_eq!(
        fx.history(),
        map(&[("a", Item::Exec("restored".into())), ("c", file("3"))])
    );
    let head = fx.committer.history().head().unwrap().unwrap();
    let parent = head.parent(0).unwrap();
    let message = parent.message().unwrap().to_owned();
    assert!(message.starts_with("revert 0123456: x"), "{message}");
    assert!(
        message.contains("Agentfs-Revert: 0123456789012345678901234567890123456789"),
        "{message}"
    );
}

// ---- the fsync class (design §8) ----

const SQLITE: &str = "*.db* agentfs.commit=fsync agentfs.group=sqlite agentfs.coalesce=5s\n";

impl Fx {
    /// A fixture whose operator `info/attributes` is `attributes`.
    fn with_attributes(attributes: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("backing")).unwrap();
        let state = StateDir::new(dir.path().join("state"));
        volume::init(&state, "vol", None).unwrap();
        fs::write(state.git().join("info/attributes"), attributes).unwrap();
        Self::open_on(
            dir,
            OpsConfig {
                granularity_ns: 0,
                chunk: RENAME_CHUNK,
            },
        )
    }

    fn journal(&self) -> Vec<agentfs_core::record::Record> {
        agentfs_core::journal::read_records(&self.state().journal())
            .unwrap()
            .0
    }

    /// Runs the trailing timer as if `secs` had passed.
    fn later(&self, secs: u64) -> usize {
        self.ops
            .capture_due(Instant::now() + std::time::Duration::from_secs(secs))
    }
}

#[test]
fn fsync_class_files_are_captured_whole_at_fsync_at_most_once_per_interval() {
    let fx = Fx::with_attributes(SQLITE);
    let (_, db) = fx.create("s.db");
    let (_, wal) = fx.create("s.db-wal");
    fx.ops.write(db, 0, b"db1", Actor::of_process(C)).unwrap();
    fx.ops.write(wal, 0, b"wal1", Actor::of_process(C)).unwrap();
    // Nothing is recorded per file.
    assert_eq!(fx.backlog(), 0);
    fx.ops.fsync(wal, false).unwrap();
    let journal = fx.journal();
    assert_eq!(journal.len(), 1);
    assert_eq!(journal[0].draft.trigger, Trigger::Fsync);
    assert!(journal[0].draft.atomic);
    assert_eq!(
        fx.history(),
        map(&[("s.db", file("db1")), ("s.db-wal", file("wal1"))])
    );
    // A second fsync within the interval records nothing...
    fx.ops.write(db, 0, b"db2", Actor::of_process(C)).unwrap();
    fx.ops.fsync(db, true).unwrap();
    assert_eq!(fx.backlog(), 0);
    // ...nor does the timer before the interval has passed...
    assert_eq!(fx.later(1), 0);
    // ...but it does after.
    assert_eq!(fx.later(6), 1);
    assert_eq!(
        fx.journal().last().unwrap().draft.trigger,
        Trigger::Interval
    );
    assert_eq!(fx.history()["s.db"], file("db2"));
    // Clean: nothing more to capture, not even at close.
    assert_eq!(fx.later(60), 0);
    fx.close(db);
    fx.close(wal);
    assert_eq!(fx.backlog(), 0);
    assert!(fx.ops.groups().is_empty() || fx.ops.groups().dirty().is_empty());
}

#[test]
fn close_captures_and_shm_is_never_recorded() {
    let fx = Fx::with_attributes(SQLITE);
    fx.put("s.db-shm", "shared");
    assert_eq!(fx.backlog(), 0);
    fx.put("s.db", "v1");
    let journal = fx.journal();
    assert_eq!(journal.len(), 1);
    assert_eq!(journal[0].draft.trigger, Trigger::Close);
    fx.put("s.db-journal", "hot");
    assert_eq!(
        fx.history(),
        map(&[("s.db", file("v1")), ("s.db-journal", file("hot"))])
    );
    // An unlink of a member is captured with the group, not alone.
    fx.ops
        .unlink(ROOT, b"s.db-journal", Actor::of_process(C))
        .unwrap();
    assert_eq!(fx.backlog(), 0);
    assert_eq!(fx.later(6), 1);
    assert_eq!(fx.history(), map(&[("s.db", file("v1"))]));
    // Ordinary files are untouched by all this.
    fx.put("a.txt", "x");
    assert_eq!(fx.history()["a.txt"], file("x"));
}

#[test]
fn class_handles_never_use_passthrough() {
    let fx = Fx::with_attributes(SQLITE);
    fx.put("s.db", "v1");
    fx.put("a.txt", "x");
    let id = fx.id("s.db");
    let (_, h) = fx.ops.open(id, O_RDONLY, Actor::of_process(C)).unwrap();
    assert!(h.daemon_only());
    let id = fx.id("a.txt");
    let (_, h) = fx.ops.open(id, O_RDONLY, Actor::of_process(C)).unwrap();
    assert!(!h.daemon_only());
    // Without the class, nothing is daemon-only.
    let fx = Fx::new();
    fx.put("s.db", "v1");
    let id = fx.id("s.db");
    assert!(
        !fx.ops
            .open(id, O_RDONLY, Actor::of_process(C))
            .unwrap()
            .1
            .daemon_only()
    );
}

#[test]
fn a_capture_holds_writes_to_every_member() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let fx = Fx::with_attributes(SQLITE);
    let (_, db) = fx.create("s.db");
    fx.ops.write(db, 0, b"1", Actor::of_process(C)).unwrap();
    let (_, wal) = fx.create("s.db-wal");
    let group = fx.ops.groups().dirty().pop().unwrap();
    let frozen = group.gate().freeze();
    let done = AtomicBool::new(false);
    std::thread::scope(|s| {
        s.spawn(|| {
            fx.ops.write(wal, 0, b"w", Actor::of_process(C)).unwrap();
            done.store(true, Ordering::SeqCst);
        });
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(
            !done.load(Ordering::SeqCst),
            "the write waits for the capture"
        );
        drop(frozen);
    });
    assert!(done.load(Ordering::SeqCst));
    fx.close(db);
    fx.close(wal);
    assert_eq!(
        fx.history(),
        map(&[("s.db", file("1")), ("s.db-wal", file("w"))])
    );
}

#[test]
fn a_rename_into_the_class_is_captured_with_the_group() {
    let fx = Fx::with_attributes(SQLITE);
    fx.put("tmp", "new db");
    fx.history();
    fx.rename("tmp", "s.db", 0).unwrap();
    // The removal of `tmp` is recorded at once; `s.db` by the timer.
    assert_eq!(fx.history(), map(&[]));
    assert_eq!(fx.later(6), 1);
    assert_eq!(fx.history(), map(&[("s.db", file("new db"))]));
}

#[test]
fn the_last_release_and_unmount_capture_what_changed() {
    let fx = Fx::with_attributes(SQLITE);
    // A write no close follows (a mapping, say): captured at release.
    let (_, db) = fx.create("s.db");
    fx.ops
        .write(db, 0, b"mapped", Actor::of_process(C))
        .unwrap();
    fx.ops.release(db).unwrap();
    assert_eq!(fx.journal().last().unwrap().draft.trigger, Trigger::Release);
    assert_eq!(fx.history()["s.db"], file("mapped"));
    // A truncate by path, never closed: captured at unmount.
    let id = fx.id("s.db");
    fx.ops
        .setattr(
            id,
            &SetAttr {
                size: Some(3),
                ..SetAttr::default()
            },
            Actor::of_process(C),
        )
        .unwrap();
    assert_eq!(fx.backlog(), 0);
    assert_eq!(fx.ops.capture_all(), 1);
    assert_eq!(fx.history()["s.db"], file("map"));
    assert_eq!(fx.ops.capture_all(), 0);
}

#[test]
fn a_member_over_max_size_refuses_the_whole_group() {
    let fx = Fx::with_attributes(&format!("{SQLITE}*.db-wal agentfs.maxSize=4\n"));
    let (_, db) = fx.create("s.db");
    let (_, wal) = fx.create("s.db-wal");
    fx.ops.write(db, 0, b"db", Actor::of_process(C)).unwrap();
    fx.ops
        .write(wal, 0, b"0123456789", Actor::of_process(C))
        .unwrap();
    // A refusal is not a failure: the last close succeeds, nothing is
    // recorded, and the group is clean.
    fx.close(db);
    fx.close(wal);
    assert_eq!(fx.backlog(), 0);
    assert_eq!(fx.refused(), ["s.db-wal"]);
    assert_eq!(fx.later(60), 0);
}

#[test]
fn a_restore_actor_s_changes_are_captured_whole_at_the_operation_s_end() {
    use agentfs_core::record::OperationKind;
    let fx = Fx::with_attributes(SQLITE);
    fx.put("s.db", "v1");
    fx.put("s.db-journal", "j");
    fx.history();
    let records = fx.journal().len();
    let operator = Caller {
        uid: 0,
        gid: 0,
        pid: 999,
    };
    let actor = Caller { pid: 4321, ..C };
    // The workload holds the database open meanwhile (a forced restore).
    let workload = fx.open("s.db", O_RDONLY);
    let guard = fx
        .ops
        .boundary()
        .recorder()
        .begin_operation(
            OperationKind::Restore,
            "0123456789012345678901234567890123456789".into(),
            "restore s.db from 0123456".into(),
            operator,
            &[actor.pid],
        )
        .unwrap();
    // The actor removes the journal and rewrites the database, as a
    // restore does, one member at a time.
    fx.ops
        .unlink(ROOT, b"s.db-journal", Actor::of_process(actor))
        .unwrap();
    let id = fx.id("s.db");
    let (fh, _) = fx
        .ops
        .open(id, O_WRONLY | O_TRUNC, Actor::of_process(actor))
        .unwrap();
    fx.ops
        .write(fh, 0, b"v0", Actor::of_process(actor))
        .unwrap();
    // No trigger records the half-restored group: not the timer, not an
    // fsync, not the actor's close or release.
    assert_eq!(fx.later(600), 0);
    fx.ops.fsync(workload, false).unwrap();
    fx.ops.flush(fh, Actor::of_process(actor)).unwrap();
    fx.ops.release(fh).unwrap();
    assert_eq!(fx.journal().len(), records);
    // The operation's end records it whole, as part of the operation.
    assert_eq!(fx.ops.capture_operation().unwrap(), 1);
    guard.finish();
    let journal = fx.journal();
    assert_eq!(journal.len(), records + 1);
    let last = journal.last().unwrap();
    assert_eq!(last.draft.trigger, Trigger::Restore);
    assert_eq!(last.draft.caller, operator);
    assert!(last.draft.atomic);
    assert!(last.draft.operation.is_some());
    assert_eq!(fx.history(), map(&[("s.db", file("v0"))]));
    // Clean afterwards: nothing for the timer or the last close.
    assert_eq!(fx.later(600), 0);
    fx.close(workload);
    assert_eq!(fx.backlog(), 0);
}

#[test]
fn closing_one_member_while_another_is_open_does_not_capture() {
    // SQLite in DELETE mode opens, writes, fsyncs and closes (or unlinks)
    // the -journal on every transaction while the database stays open:
    // the close must not capture, or the rate is unbounded.
    let fx = Fx::with_attributes(SQLITE);
    let (_, db) = fx.create("s.db");
    fx.ops.write(db, 0, b"db0", Actor::of_process(C)).unwrap();
    fx.ops.fsync(db, false).unwrap();
    assert_eq!(fx.journal().len(), 1, "the first fsync captures");
    for i in 1..=5 {
        let (_, journal) = fx.create("s.db-journal");
        fx.ops
            .write(journal, 0, b"hot", Actor::of_process(C))
            .unwrap();
        fx.ops.fsync(journal, false).unwrap();
        fx.close(journal);
        fx.ops
            .write(db, 0, format!("db{i}").as_bytes(), Actor::of_process(C))
            .unwrap();
        fx.ops.fsync(db, false).unwrap();
        fx.ops
            .unlink(ROOT, b"s.db-journal", Actor::of_process(C))
            .unwrap();
    }
    // Five transactions inside one interval: still one capture.
    assert_eq!(fx.journal().len(), 1);
    // A reader opening and closing the database does not capture either.
    let reader = fx.open("s.db", O_RDONLY);
    fx.close(reader);
    assert_eq!(fx.journal().len(), 1);
    // The trailing timer, one interval on, records the last state...
    assert_eq!(fx.later(6), 1);
    assert_eq!(fx.history(), map(&[("s.db", file("db5"))]));
    // ...and the last handle's close what changed after it.
    fx.ops.write(db, 0, b"db6", Actor::of_process(C)).unwrap();
    fx.close(db);
    assert_eq!(fx.journal().last().unwrap().draft.trigger, Trigger::Close);
    assert_eq!(fx.history(), map(&[("s.db", file("db6"))]));
}

#[test]
fn a_failed_fsync_capture_returns_ok_and_leaves_the_group_changed() {
    if rustix::process::geteuid().is_root() {
        eprintln!("root ignores the permission this test takes away: skipped");
        return;
    }
    let fx = Fx::with_attributes(SQLITE);
    let (_, db) = fx.create("s.db");
    fx.ops.write(db, 0, b"db1", Actor::of_process(C)).unwrap();
    let pending = fx.state().pending();
    let mode = fs::metadata(&pending).unwrap().permissions();
    fs::set_permissions(&pending, fs::Permissions::from_mode(0o500)).unwrap();
    // The data reached the disk: the fsync succeeds whatever the capture.
    fx.ops.fsync(db, false).unwrap();
    assert_eq!(fx.backlog(), 0);
    let group = fx.ops.groups().dirty().pop().expect("still changed");
    assert!(group.state().last_attempt.is_some());
    assert!(
        fx.sink
            .snapshot()
            .iter()
            .any(|e| matches!(e, Event::RecordFailed { .. }))
    );
    fs::set_permissions(&pending, mode).unwrap();
    // The timer retries it one interval later.
    assert_eq!(fx.later(6), 1);
    assert_eq!(fx.history(), map(&[("s.db", file("db1"))]));
    fx.close(db);
}

#[test]
fn a_database_whose_directory_moved_is_captured_at_its_new_path() {
    let fx = Fx::with_attributes(SQLITE);
    fx.mkdir("d");
    let (_, db) = fx.create("d/s.db");
    fx.ops.write(db, 0, b"v1", Actor::of_process(C)).unwrap();
    fx.rename("d", "e", 0).unwrap();
    // Writes through the handle opened before the rename change the group
    // at the new path.
    fx.ops.write(db, 0, b"v2", Actor::of_process(C)).unwrap();
    assert_eq!(fx.later(6), 1);
    assert_eq!(fx.history(), map(&[("e/s.db", file("v2"))]));
    fx.ops.write(db, 0, b"v3", Actor::of_process(C)).unwrap();
    assert_eq!(fx.later(12), 1);
    assert_eq!(fx.history(), map(&[("e/s.db", file("v3"))]));
    // The handle counts in its new group: its close is the last one.
    fx.ops.write(db, 0, b"v4", Actor::of_process(C)).unwrap();
    fx.close(db);
    assert_eq!(fx.history(), map(&[("e/s.db", file("v4"))]));
    assert!(fx.ops.groups().dirty().is_empty());
}

#[test]
fn a_member_the_rules_leave_out_refuses_the_whole_group() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("backing")).unwrap();
    let state = StateDir::new(dir.path().join("state"));
    volume::init(&state, "vol", None).unwrap();
    fs::write(state.git().join("info/attributes"), SQLITE).unwrap();
    fs::write(state.git().join("info/exclude"), "*-wal\n").unwrap();
    let fx = Fx::open_on(
        dir,
        OpsConfig {
            granularity_ns: 0,
            chunk: RENAME_CHUNK,
        },
    );
    let (_, db) = fx.create("s.db");
    let (_, wal) = fx.create("s.db-wal");
    fx.ops.write(db, 0, b"db", Actor::of_process(C)).unwrap();
    fx.ops
        .write(wal, 0, b"frames", Actor::of_process(C))
        .unwrap();
    fx.close(db);
    fx.close(wal);
    // Without its WAL the database could be torn: nothing is recorded,
    // and the refusal is raised.
    assert_eq!(fx.backlog(), 0);
    assert_eq!(fx.refused(), ["s.db-wal"]);
    // A database with no WAL is recorded.
    fx.ops
        .unlink(ROOT, b"s.db-wal", Actor::of_process(C))
        .unwrap();
    assert_eq!(fx.later(6), 1);
    assert_eq!(fx.history(), map(&[("s.db", file("db"))]));
}

#[test]
fn cached_at_names_every_entry_on_the_way_and_the_known_node() {
    let fx = Fx::new();
    let dir = fx.mkdir("d");
    fx.put("d/f", "x");
    let f = fx.id("d/f");
    assert_eq!(
        fx.ops.cached_at(b"d/f"),
        vec![
            Cached::Entry(ROOT, b"d".to_vec()),
            Cached::Entry(dir, b"f".to_vec()),
            Cached::Node(f),
        ]
    );
    // A name the mount never looked up: its entry, and nothing below it.
    assert_eq!(
        fx.ops.cached_at(b"d/new/deeper"),
        vec![
            Cached::Entry(ROOT, b"d".to_vec()),
            Cached::Entry(dir, b"new".to_vec()),
        ]
    );
    assert_eq!(fx.ops.cached_at(b""), vec![Cached::Node(ROOT)]);
}

// ---- moving a file into the tree from outside it (sandbox overlay) ----

/// A [`crate::session::NameCheck`] for `move_in` that answers `answer` and
/// keeps what it was shown.
struct MoveCheck {
    answer: OpResult<()>,
    seen: std::sync::Mutex<Vec<(String, Option<Kind>)>>,
}

impl MoveCheck {
    fn answering(answer: OpResult<()>) -> Self {
        Self {
            answer,
            seen: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn seen(&self) -> Vec<(String, Option<Kind>)> {
        self.seen.lock().unwrap().clone()
    }
}

impl crate::session::NameCheck for MoveCheck {
    fn create(&self, _at: crate::session::Found<'_>, _kind: Kind) -> OpResult<()> {
        unreachable!("move_in asks only move_in")
    }

    fn link(&self, _at: crate::session::Found<'_>) -> OpResult<()> {
        unreachable!("move_in asks only move_in")
    }

    fn remove(&self, _at: crate::session::Found<'_>) -> OpResult<()> {
        unreachable!("move_in asks only move_in")
    }

    fn rename(
        &self,
        _from: crate::session::Found<'_>,
        _to: crate::session::Found<'_>,
        _flags: u32,
    ) -> OpResult<()> {
        unreachable!("move_in asks only move_in")
    }

    fn move_in(&self, to: crate::session::Found<'_>) -> OpResult<()> {
        self.seen
            .lock()
            .unwrap()
            .push((String::from_utf8(to.path.to_vec()).unwrap(), to.kind));
        self.answer
    }
}

impl Fx {
    /// A directory outside the tree, on its filesystem, holding `name`
    /// with `content`: a descriptor of it.
    fn outside(&self, name: &str, content: &str) -> OwnedFd {
        let store = self.dir.path().join("store");
        fs::create_dir_all(&store).unwrap();
        fs::write(store.join(name), content).unwrap();
        rustix::fs::open(&store, OFlags::PATH | OFlags::DIRECTORY, Mode::empty()).unwrap()
    }

    fn outside_names(&self) -> Vec<String> {
        fs::read_dir(self.dir.path().join("store"))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect()
    }

    /// Moves `name` from `store` to `to` under the root as node `node`.
    fn move_in(
        &self,
        store: &OwnedFd,
        name: &str,
        to: &str,
        flags: u32,
        moved: crate::session::Moved,
        check: &MoveCheck,
    ) -> OpResult<Entry> {
        use crate::session::TreeOperations;
        self.ops.move_in(
            store.as_fd(),
            name.as_bytes(),
            ROOT,
            to.as_bytes(),
            flags,
            moved,
            check,
            C,
        )
    }
}

fn unopened(node: u64) -> crate::session::Moved {
    crate::session::Moved {
        node,
        lookups: 1,
        handles: Vec::new(),
        mapping_may_write: false,
    }
}

#[test]
fn move_in_records_the_new_path_only() {
    use crate::session::TreeOperations;
    let fx = Fx::new();
    fx.put("config.yaml", "old");
    let before = fx.journal().len();
    let store = fx.outside("17", "new");
    let node = fx.ops.new_node_id();
    let check = MoveCheck::answering(Ok(()));
    let entry = fx
        .move_in(&store, "17", "config.yaml", 0, unopened(node), &check)
        .unwrap();
    assert_eq!(entry.id, node);
    assert_eq!(entry.attr.ino.0, node);
    assert_eq!(check.seen(), [("config.yaml".to_owned(), Some(Kind::File))]);
    // One record, a rename of the new path only.
    let records = fx.journal();
    assert_eq!(records.len(), before + 1, "{records:#?}");
    let record = records.last().unwrap();
    assert_eq!(record.draft.trigger, Trigger::Rename);
    let paths: Vec<&str> = record
        .draft
        .changes
        .iter()
        .flat_map(|c| c.paths())
        .map(|p| p.as_str())
        .collect();
    assert_eq!(paths, ["config.yaml"]);
    assert_eq!(fx.history(), map(&[("config.yaml", file("new"))]));
    assert!(fx.outside_names().is_empty());
    assert_eq!(fx.ops.lookup(ROOT, b"config.yaml").unwrap().id, node);
    // Only a plain or no-replace move of a regular file, at a good name.
    for (flags, to) in [
        (RenameFlags::EXCHANGE.bits(), "x"),
        (RenameFlags::WHITEOUT.bits(), "x"),
        (0, "a/b"),
    ] {
        let store = fx.outside("18", "z");
        assert_eq!(
            fx.move_in(&store, "18", to, flags, unopened(node + 1), &check)
                .unwrap_err(),
            Errno::EINVAL,
            "{flags} {to}"
        );
    }
    fs::create_dir_all(fx.dir.path().join("store/dir")).unwrap();
    assert_eq!(
        fx.move_in(&store, "dir", "d", 0, unopened(node + 1), &check)
            .unwrap_err(),
        Errno::EINVAL
    );
    assert!(!fx.backing().join("d").exists());
}

#[test]
fn move_in_keeps_the_node_id_and_takes_over_handles() {
    use crate::session::TreeOperations;
    let fx = Fx::new();
    fx.put("other", "o");
    let store = fx.outside("17", "v1");
    let node = fx.ops.new_node_id();
    assert_ne!(fx.id("other"), node);
    let fd = rustix::fs::openat(&store, "17", OFlags::RDWR, Mode::empty()).unwrap();
    let fh = fx.ops.new_handle_id();
    assert!(fx.ops.new_handle_id() > fh, "from one sequence");
    let handle = Arc::new(Handle::new(node, fd, true, None, 0));
    let moved = crate::session::Moved {
        node,
        lookups: 2,
        handles: vec![(fh, handle)],
        mapping_may_write: false,
    };
    let check = MoveCheck::answering(Ok(()));
    fx.move_in(&store, "17", "config.yaml", 0, moved, &check)
        .unwrap();
    assert_eq!(fx.history().get("config.yaml"), Some(&file("v1")));
    // The handle the kernel holds writes on, and its close records the
    // new content at the new path.
    let before = fx.journal().len();
    fx.ops
        .write(fh, 0, b"v2 longer", Actor::of_process(C))
        .unwrap();
    fx.ops.flush(fh, Actor::of_process(C)).unwrap();
    let records = fx.journal();
    assert_eq!(records.len(), before + 1, "{records:#?}");
    assert_eq!(records.last().unwrap().draft.trigger, Trigger::Close);
    assert_eq!(
        fx.history(),
        map(&[("config.yaml", file("v2 longer")), ("other", file("o"))])
    );
    assert_eq!(fx.ops.getattr(node, Some(fh)).unwrap().ino.0, node);
    assert_eq!(fx.ops.open_handles(), 1);
    fx.ops.release(fh).unwrap();
    assert_eq!(fx.ops.open_handles(), 0);
    // The kernel's two lookups keep the node until it forgets them.
    fx.ops.forget(node, 1);
    assert_eq!(fx.ops.getattr(node, None).unwrap().size, 9);
    fx.ops.forget(node, 1);
    assert_eq!(fx.ops.lookup(ROOT, b"config.yaml").unwrap().id, node);
}

#[test]
fn move_in_refused_by_the_check_moves_nothing() {
    use crate::session::TreeOperations;
    let fx = Fx::new();
    fx.put("config.yaml", "old");
    let before = fx.journal().len();
    let store = fx.outside("17", "new");
    let node = fx.ops.new_node_id();
    let refuse = MoveCheck::answering(Err(Errno::EACCES));
    assert_eq!(
        fx.move_in(&store, "17", "new.yaml", 0, unopened(node), &refuse)
            .unwrap_err(),
        Errno::EACCES
    );
    assert_eq!(refuse.seen(), [("new.yaml".to_owned(), None)]);
    assert_eq!(fx.outside_names(), ["17"]);
    assert!(!fx.backing().join("new.yaml").exists());
    // The name is taken, and the move may not replace it.
    let check = MoveCheck::answering(Ok(()));
    assert_eq!(
        fx.move_in(
            &store,
            "17",
            "config.yaml",
            RenameFlags::NOREPLACE.bits(),
            unopened(node),
            &check
        )
        .unwrap_err(),
        Errno::EEXIST
    );
    assert_eq!(fx.outside_names(), ["17"]);
    assert_eq!(
        fs::read_to_string(fx.backing().join("config.yaml")).unwrap(),
        "old"
    );
    assert_eq!(fx.journal().len(), before);
    assert_eq!(
        fx.ops.lookup(ROOT, b"config.yaml").unwrap().id,
        fx.id("config.yaml")
    );
    assert_ne!(fx.id("config.yaml"), node);
}

// ---- a request's check, under the names ordering (sandbox overlay slice 4) ----

/// A [`NameCheck`] that refuses everything, after asserting the
/// operations hold the names ordering, and records what it was shown.
struct Refuses<'a> {
    ops: &'a Ops,
    seen: std::sync::Mutex<Vec<String>>,
}

impl Refuses<'_> {
    fn saw(&self, what: String) -> OpResult<()> {
        assert!(
            self.ops.names.try_write().is_err(),
            "{what}: checked without the names ordering held"
        );
        self.seen.lock().unwrap().push(what);
        Err(Errno::EACCES)
    }
}

fn found(at: Found<'_>) -> String {
    format!("{}={:?}", String::from_utf8_lossy(at.path), at.kind)
}

impl crate::session::NameCheck for Refuses<'_> {
    fn create(&self, at: Found<'_>, kind: Kind) -> OpResult<()> {
        self.saw(format!("create {} {kind:?}", found(at)))
    }
    fn link(&self, at: Found<'_>) -> OpResult<()> {
        self.saw(format!("link {}", found(at)))
    }
    fn remove(&self, at: Found<'_>) -> OpResult<()> {
        self.saw(format!("remove {}", found(at)))
    }
    fn rename(&self, from: Found<'_>, to: Found<'_>, flags: u32) -> OpResult<()> {
        self.saw(format!("rename {} {} {flags}", found(from), found(to)))
    }
    fn move_in(&self, to: Found<'_>) -> OpResult<()> {
        self.saw(format!("move_in {}", found(to)))
    }
}

/// Every file and directory under `root`, with its kind and content.
fn snapshot(root: &std::path::Path) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut dirs = vec![root.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        for e in fs::read_dir(&dir).unwrap() {
            let e = e.unwrap();
            let path = e.path();
            let rel = path.strip_prefix(root).unwrap().display().to_string();
            let t = e.file_type().unwrap();
            let what = if t.is_dir() {
                dirs.push(path.clone());
                "dir".to_owned()
            } else if t.is_file() {
                fs::read_to_string(&path).unwrap()
            } else {
                format!("{t:?}")
            };
            out.insert(rel, what);
        }
    }
    out
}

/// Each name operation asks the request's check while it holds the names
/// ordering, after statting the name, with what it found; a refusal is
/// the answer and nothing changes.
#[test]
fn a_check_decides_under_the_names_ordering_and_a_refusal_changes_nothing() {
    let fx = Fx::new();
    fx.put("a", "1");
    fx.put("b", "2");
    fs::create_dir(fx.backing().join("d")).unwrap();
    rustix::fs::mknodat(
        rustix::fs::CWD,
        fx.backing().join("pipe"),
        rustix::fs::FileType::Fifo,
        Mode::from_raw_mode(0o644),
        0,
    )
    .unwrap();
    let before = snapshot(&fx.backing());
    let history = fx.history();
    let backlog = fx.backlog();
    let check = Refuses {
        ops: &fx.ops,
        seen: std::sync::Mutex::new(Vec::new()),
    };
    let actor = Actor {
        caller: C,
        groups: Supplementary::OfProcess,
        check: Some(&check),
    };
    let a = fx.id("a");
    let eacces = Some(Errno::EACCES);
    assert_eq!(
        fx.ops.create(ROOT, b"new", 0o644, O_WRONLY, actor).err(),
        eacces
    );
    assert_eq!(
        fx.ops.create(ROOT, b"pipe", 0o644, O_WRONLY, actor).err(),
        eacces
    );
    assert_eq!(fx.ops.mknod(ROOT, b"n", 0o100644, 0, actor).err(), eacces);
    assert_eq!(fx.ops.mknod(ROOT, b"f", 0o010644, 0, actor).err(), eacces);
    assert_eq!(fx.ops.mkdir(ROOT, b"e", 0o755, actor).err(), eacces);
    assert_eq!(fx.ops.symlink(ROOT, b"s", b"a", actor).err(), eacces);
    assert_eq!(fx.ops.link(a, ROOT, b"l", actor).err(), eacces);
    assert_eq!(fx.ops.unlink(ROOT, b"a", actor).err(), eacces);
    assert_eq!(fx.ops.rmdir(ROOT, b"d", actor).err(), eacces);
    assert_eq!(
        fx.ops.rename(ROOT, b"a", ROOT, b"b", 0, actor).err(),
        eacces
    );
    assert_eq!(
        fx.ops.rename(ROOT, b"a", ROOT, b"c", 0, actor).err(),
        eacces
    );
    assert_eq!(
        fx.ops.rename(ROOT, b"d", ROOT, b"pipe", 0, actor).err(),
        eacces
    );
    assert_eq!(
        *check.seen.lock().unwrap(),
        [
            "create new=None File",
            "create pipe=Some(Other) File",
            "create n=None File",
            "create f=None Other",
            "create e=None Dir",
            "create s=None Symlink",
            "link l=None",
            "remove a=Some(File)",
            "remove d=Some(Dir)",
            "rename a=Some(File) b=Some(File) 0",
            "rename a=Some(File) c=None 0",
            "rename d=Some(Dir) pipe=Some(Other) 0",
        ]
    );
    assert_eq!(snapshot(&fx.backing()), before);
    assert_eq!(fx.backlog(), backlog, "nothing recorded");
    assert_eq!(fx.history(), history);
    // Without a check, the main mount's requests go on as before.
    fx.ops
        .rename(ROOT, b"a", ROOT, b"c", 0, Actor::of_process(C))
        .unwrap();
}

/// The access check of an existing file opened by `create` counts the
/// supplementary groups only of a request that acts as its own process.
#[test]
fn an_existing_file_is_opened_by_create_only_with_access() {
    let fx = Fx::new();
    let path = fx.backing().join("shared");
    fs::write(&path, "x").unwrap();
    // A process in group `group` that neither owns the file nor has the
    // file's group as its own: as root, a child given that group; else
    // this process, through one of its supplementary groups.
    let me = rustix::process::geteuid().as_raw();
    let (group, mut child, pid) = if me == 0 {
        let child = std::process::Command::new("setpriv")
            .args(["--groups=4242", "--", "sleep", "60"])
            .spawn()
            .unwrap();
        let pid = child.id();
        // Its groups are set before it runs sleep.
        while !creds::caller_groups(pid).contains(&4242) {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        std::os::unix::fs::chown(&path, Some(54321), Some(4242)).unwrap();
        (4242, Some(child), pid)
    } else {
        let primary = rustix::process::getegid().as_raw();
        let Some(group) = creds::caller_groups(std::process::id())
            .into_iter()
            .find(|g| *g != primary)
        else {
            eprintln!("skipped: needs root or a supplementary group");
            return;
        };
        std::os::unix::fs::chown(&path, None, Some(group)).unwrap();
        (group, None, std::process::id())
    };
    // The owner may read and write: as non-root, Ops opens the file as
    // this process, its owner. The caller is not the owner, so only the
    // group bits can let it in.
    fs::set_permissions(&path, fs::Permissions::from_mode(0o670)).unwrap();
    let caller = Caller {
        uid: me.wrapping_add(12345),
        gid: me.wrapping_add(12345),
        pid,
    };
    assert_ne!(caller.gid, group);
    let (_, fh, _) = fx
        .ops
        .create(ROOT, b"shared", 0o644, O_RDWR, Actor::of_process(caller))
        .unwrap();
    fx.ops.release(fh).unwrap();
    let substituted = Actor {
        caller,
        groups: Supplementary::None,
        check: None,
    };
    assert_eq!(
        fx.ops
            .create(ROOT, b"shared", 0o644, O_RDWR, substituted)
            .err(),
        Some(Errno::EACCES)
    );
    if let Some(child) = &mut child {
        let _ = child.kill();
        let _ = child.wait();
    }
}

/// An open to execute is an open to read: the kernel's `__FMODE_EXEC` is
/// not an open flag, and `openat2(2)` would refuse it.
#[test]
fn an_open_to_execute_opens_the_file() {
    let fx = Fx::new();
    fx.put("run.sh", "#!/bin/sh\n");
    let id = fx.id("run.sh");
    let (fh, handle) = fx
        .ops
        .open(id, O_RDONLY | FMODE_EXEC, Actor::of_process(C))
        .unwrap();
    assert!(!handle.write);
    assert_eq!(fx.ops.read(fh, 0, 64).unwrap(), b"#!/bin/sh\n");
    fx.ops.release(fh).unwrap();
}
