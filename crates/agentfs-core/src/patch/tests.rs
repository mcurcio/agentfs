//! The patch queue against real volumes (design §12, §18 test 15): a state
//! area, a backing tree, the recorder and committer, the `git` program
//! merging, no FUSE.
//!
//! The tests that run the queue skip, with a message, when `git` is not
//! installed, as `crates/agentfs/tests/cli_patch.rs` does.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use git2::{Oid, Repository, Signature, Time};

use super::accept::{AcceptanceRule, CleanMerge};
use super::integrate::{GitMergeTree, Integrated, Integrator, PatchBranch};
use super::queue::{Queue, QueueContext, QueueReport};
use super::status::{PatchState, read_status};
use super::*;
use crate::confined::Dir;
use crate::events::{Event, Events, MemorySink};
use crate::git_cli;
use crate::identity::Context;
use crate::lfs::Pointer;
use crate::lfs::fetch::LfsObjects;
use crate::paths::RepoPath;
use crate::record::{Caller, OperationKind};
use crate::restore::{self, Applied, NotLive, RecordMode};
use crate::telemetry::Telemetry;
use crate::volume::{self, Committer, OperationSpec, Recorder, StateDir};

const CALLER: Caller = Caller {
    uid: 10000,
    gid: 10000,
    pid: 1,
};

/// Whether `git` is installed; says so when it is not.
fn have_git() -> bool {
    let found = git_cli::find_git().is_some();
    if !found {
        eprintln!("skipping: git is not installed");
    }
    found
}

struct NoLfs;

impl LfsObjects for NoLfs {
    fn ensure(&self, pointer: &Pointer) -> Result<PathBuf, crate::lfs::LfsError> {
        Err(crate::lfs::LfsError::local(format!(
            "no object {}",
            pointer.oid
        )))
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    state: StateDir,
    root: PathBuf,
    sink: Arc<MemorySink>,
    recorder: Recorder,
    committer: Committer,
}

fn context() -> Context {
    Context {
        volume: "vol".into(),
        replica: "pod-0".into(),
        pod_uid: "u".into(),
        ..Context::default()
    }
}

impl Fixture {
    /// A volume whose branch holds `files`.
    fn new(files: &[(&str, &str)]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(dir.path().join("state"));
        volume::init(&state, "vol", None).unwrap();
        let root = dir.path().join("tree");
        fs::create_dir_all(&root).unwrap();
        let sink = Arc::new(MemorySink::new());
        let (recorder, committer) = open(&state, &root, &sink);
        let f = Self {
            _dir: dir,
            state,
            root,
            sink,
            recorder,
            committer,
        };
        f.change(files, &[]);
        f
    }

    /// Writes and removes files in the tree and records them, committed.
    fn change(&self, files: &[(&str, &str)], removes: &[&str]) {
        for (path, content) in files {
            let full = self.root.join(path);
            fs::create_dir_all(full.parent().unwrap()).unwrap();
            fs::write(full, content).unwrap();
        }
        for path in removes {
            fs::remove_file(self.root.join(path)).unwrap();
        }
        let applied = Applied {
            written: files
                .iter()
                .map(|(p, _)| RepoPath::new(*p).unwrap())
                .collect(),
            removed: removes.iter().map(|p| RepoPath::new(*p).unwrap()).collect(),
        };
        restore::record_applied(
            &self.recorder,
            &self.dir(),
            &applied,
            CALLER,
            RecordMode::Chunked(100),
        )
        .unwrap();
        self.committer.commit_pending().unwrap();
    }

    /// Closes and reopens the volume (journal replay).
    fn reopen(&mut self) {
        let (recorder, committer) = open(&self.state, &self.root, &self.sink);
        self.recorder = recorder;
        self.committer = committer;
    }

    fn dir(&self) -> Dir {
        Dir::open(&self.root).unwrap()
    }

    fn repo(&self) -> &Repository {
        self.committer.history().repo()
    }

    fn head(&self) -> Oid {
        self.committer.history().head().unwrap().unwrap().id()
    }

    fn read(&self, path: &str) -> Option<String> {
        fs::read_to_string(self.root.join(path)).ok()
    }

    /// The content of `path` in the branch head.
    fn at_head(&self, path: &str) -> Option<String> {
        let tree = self
            .committer
            .history()
            .head()
            .unwrap()
            .unwrap()
            .tree()
            .unwrap();
        let entry = tree.get_path(Path::new(path)).ok()?;
        let blob = self.repo().find_blob(entry.id()).unwrap();
        Some(String::from_utf8_lossy(blob.content()).into_owned())
    }

    /// A patch on `parent`: `files` written (`None` removes), authored by
    /// Ada.
    fn patch(&self, parent: Oid, files: &[(&str, Option<&str>)], message: &str) -> Oid {
        let repo = self.repo();
        let parent = repo.find_commit(parent).unwrap();
        let tree = edit_tree(repo, &parent.tree().unwrap(), files);
        let ada = Signature::new(
            "Ada Lovelace",
            "ada@example.com",
            &Time::new(1_700_000_000, 60),
        )
        .unwrap();
        repo.commit(None, &ada, &ada, message, &tree, &[&parent])
            .unwrap()
    }

    fn queue(&self, n: u64, oid: Oid) {
        self.repo()
            .reference(&inbox_ref("vol", n), oid, true, "test")
            .unwrap();
    }

    fn reference(&self, name: &str) -> Option<Oid> {
        self.repo().refname_to_id(name).ok()
    }

    fn process(&self) -> QueueReport {
        let tree = self.dir();
        let telemetry = Telemetry::noop();
        let integrator = GitMergeTree::new(self.state.git());
        let mut queue = Queue::new(QueueContext {
            state: &self.state,
            recorder: &self.recorder,
            committer: &self.committer,
            tree: &tree,
            objects: &NoLfs,
            telemetry: &telemetry,
            caller: CALLER,
            integrator: &integrator,
            rule: &CleanMerge,
        });
        queue.process(None).unwrap()
    }

    /// The commits applying patch `n` on first-parent history.
    fn inbox_commits(&self, n: u64) -> usize {
        let repo = self.repo();
        let mut walk = repo.revwalk().unwrap();
        walk.push(self.head()).unwrap();
        walk.simplify_first_parent().unwrap();
        walk.filter(|oid| {
            let c = repo.find_commit(*oid.as_ref().unwrap()).unwrap();
            inbox_from_message(c.message().unwrap()).map(|(i, _)| i) == Some(n)
        })
        .count()
    }

    fn events(&self) -> Vec<Event> {
        self.sink.take()
    }
}

fn open(state: &StateDir, root: &Path, sink: &Arc<MemorySink>) -> (Recorder, Committer) {
    let events: Events = sink.clone();
    let (recorder, committer, _) =
        volume::open(state, root, context(), events, &Telemetry::noop()).unwrap();
    (recorder, committer)
}

/// `base` with `files` written or removed (top-level and one level of
/// directories).
fn edit_tree<'r>(
    repo: &'r Repository,
    base: &git2::Tree<'_>,
    files: &[(&str, Option<&str>)],
) -> git2::Tree<'r> {
    let mut index = git2::Index::new().unwrap();
    index.read_tree(base).unwrap();
    for (path, content) in files {
        match content {
            Some(content) => {
                let id = repo.blob(content.as_bytes()).unwrap();
                index.add(&entry(path, id, 0o100644)).unwrap();
            }
            None => index.remove_path(Path::new(path)).unwrap(),
        }
    }
    repo.find_tree(index.write_tree_to(repo).unwrap()).unwrap()
}

fn entry(path: &str, id: Oid, mode: u32) -> git2::IndexEntry {
    git2::IndexEntry {
        ctime: git2::IndexTime::new(0, 0),
        mtime: git2::IndexTime::new(0, 0),
        dev: 0,
        ino: 0,
        mode,
        uid: 0,
        gid: 0,
        file_size: 0,
        id,
        flags: 0,
        flags_extended: 0,
        path: path.as_bytes().to_vec(),
    }
}

#[test]
fn naming_and_parsing() {
    assert_eq!(inbox_ref("v", 1), "refs/agentfs/v/inbox/0000000001");
    assert_eq!(applied_ref("v", 42), "refs/agentfs/v/applied/0000000042");
    assert_eq!(status_ref("v"), "refs/agentfs/v/status");
    assert_eq!(format_seq(9_999_999_999), "9999999999");
    assert_eq!(parse_seq("0000000001"), Some(1));
    for bad in [
        "0000000000",
        "1",
        "000000001",
        "00000000001",
        "000000000a",
        "",
    ] {
        assert_eq!(parse_seq(bad), None, "{bad}");
    }
    assert_eq!(
        seq_of_ref("refs/agentfs/v/inbox/", "refs/agentfs/v/inbox/0000000003"),
        Some(3)
    );
    assert_eq!(
        seq_of_ref("refs/agentfs/v/inbox/", "refs/agentfs/w/inbox/0000000003"),
        None
    );
    let reasons = serde_json::to_string(&BlockReason::ParentNotInHistory).unwrap();
    assert_eq!(reasons, "\"parent_not_in_history\"");
    assert_eq!(BlockReason::TooLarge.to_string(), "too_large");
}

#[test]
fn inbox_trailers_only_count_from_patch_commits() {
    let oid = "1".repeat(40);
    let good = format!(
        "fix\n\nAgentfs-Seq: 3\nAgentfs-Trigger: patch\nAgentfs-Patch: {oid}\nAgentfs-Inbox: 4\n"
    );
    assert_eq!(
        inbox_from_message(&good),
        Some((4, Oid::from_str(&oid).unwrap()))
    );
    // Another trigger, a trailer in an earlier paragraph, no patch oid.
    let other = format!(
        "x\n\nAgentfs-Seq: 3\nAgentfs-Trigger: write\nAgentfs-Patch: {oid}\nAgentfs-Inbox: 4\n"
    );
    assert_eq!(inbox_from_message(&other), None);
    let earlier = format!(
        "x\n\nAgentfs-Trigger: patch\nAgentfs-Patch: {oid}\nAgentfs-Inbox: 4\n\nAgentfs-Seq: 3\n"
    );
    assert_eq!(inbox_from_message(&earlier), None);
    assert_eq!(
        inbox_from_message("x\n\nAgentfs-Trigger: patch\nAgentfs-Inbox: 4\n"),
        None
    );
    assert_eq!(
        inbox_from_message(&format!(
            "x\n\nAgentfs-Trigger: patch\nAgentfs-Patch: {oid}\nAgentfs-Inbox: 0\n"
        )),
        None
    );
}

/// Design §18 test 15: three patches, the second conflicts.
/// Keep going (bead agentfs-mia.2): 2 conflicts and is held whole; 1 and
/// 3 land; a rerun retries 2; rebased, it lands below the high-water.
#[test]
fn three_patches_the_second_conflicts() {
    if !have_git() {
        return;
    }
    let f = Fixture::new(&[("a.txt", "a0\n"), ("b.txt", "b0\n")]);
    let base = f.head();
    let p1 = f.patch(
        base,
        &[("a.txt", Some("a1\n"))],
        "edit a\n\nSigned-off-by: Ada\n",
    );
    let p2 = f.patch(base, &[("b.txt", Some("b-from-iac\n"))], "edit b");
    let p3 = f.patch(base, &[("c.txt", Some("c\n"))], "add c");
    // The workload changes b.txt, so 2 conflicts.
    f.change(&[("b.txt", "b-from-workload\n")], &[]);
    f.events();
    for (n, p) in [(1, p1), (2, p2), (3, p3)] {
        f.queue(n, p);
    }

    let report = f.process();
    assert_eq!(
        report.applied.iter().map(|(n, _)| *n).collect::<Vec<_>>(),
        [1, 3]
    );
    let commit = report.applied[0].1;
    let c = f.repo().find_commit(commit).unwrap();
    assert_eq!(c.author().name(), Ok("Ada Lovelace"));
    assert_eq!(c.author().email(), Ok("ada@example.com"));
    assert_eq!(c.author().when().seconds(), 1_700_000_000);
    assert_ne!(c.committer().name(), Ok("Ada Lovelace"));
    let message = c.message().unwrap();
    assert!(message.starts_with("edit a\n\n"), "{message}");
    assert!(!message.contains("Signed-off-by"), "{message}");
    assert_eq!(inbox_from_message(message), Some((1, p1)));
    assert!(message.contains("Agentfs-Trigger: patch\n"), "{message}");
    assert_eq!(f.inbox_commits(1), 1);
    assert_eq!(f.inbox_commits(3), 1);
    assert_eq!(f.reference(&applied_ref("vol", 1)), Some(p1));
    assert_eq!(f.reference(&inbox_ref("vol", 1)), None);

    assert_eq!(report.blocked, None);
    assert_eq!(report.set_aside.len(), 1);
    let (n, verdict) = &report.set_aside[0];
    assert_eq!(*n, 2);
    let Verdict::Held {
        reason,
        message,
        paths,
    } = verdict
    else {
        panic!("{verdict:?}");
    };
    assert_eq!(*reason, BlockReason::Conflict);
    assert_eq!(paths, &["b.txt"]);
    assert!(message.contains("conflicts in b.txt"), "{message}");
    assert_eq!(report.queue_depth, 1);
    assert_eq!(report.last_applied, 3);
    assert_eq!(f.reference(&inbox_ref("vol", 2)), Some(p2));
    assert_eq!(f.reference(&held_ref("vol", 2)), Some(p2));
    assert_eq!(f.reference(&inbox_ref("vol", 3)), None);

    // The tree has 1's and 3's changes; b.txt is the workload's.
    assert_eq!(f.read("a.txt").as_deref(), Some("a1\n"));
    assert_eq!(f.read("b.txt").as_deref(), Some("b-from-workload\n"));
    assert_eq!(f.read("c.txt").as_deref(), Some("c\n"));
    assert_eq!(f.at_head("c.txt").as_deref(), Some("c\n"));

    let state = PatchState::load(&f.state.patch_state());
    assert_eq!(state.queue_depth, 1);
    assert_eq!(state.held, 1);
    assert_eq!(state.last_applied, 3);
    assert_eq!(state.blocked, None);
    assert_eq!(
        state.summary(),
        "last applied 3, 1 queued (1 set aside), ok"
    );

    let status = read_status(f.repo(), report.status).unwrap();
    assert_eq!(f.reference(&status_ref("vol")), Some(report.status));
    assert_eq!(status.applied_now, [1, 3]);
    assert_eq!(status.last_applied.as_ref().unwrap().inbox, 3);
    assert_eq!(status.last_applied.as_ref().unwrap().patch, p3.to_string());
    assert_eq!(status.queue.len(), 1);
    assert_eq!(status.queue[0].inbox, 2);
    assert_eq!(status.queue[0].verdict.as_ref(), Some(verdict));
    assert_eq!(status.blocked, None);
    assert_eq!(status.head, Some(f.head().to_string()));

    let events = f.events();
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, Event::PatchBlocked { .. })),
        "{events:?}"
    );
    let held = events
        .iter()
        .find(|e| matches!(e, Event::PatchHeld { .. }))
        .unwrap();
    assert!(held.is_alarm());
    assert!(matches!(
        held,
        Event::PatchHeld {
            inbox: 2,
            verdict: "held",
            reason: "conflict",
            ..
        }
    ));

    // A rerun retries 2: still held, nothing else changes, and inbox/2 is
    // not stale although 3 is applied.
    let head = f.head();
    let again = f.process();
    assert!(again.applied.is_empty());
    assert!(again.ignored.is_empty(), "{:?}", again.ignored);
    assert_eq!(again.set_aside.len(), 1);
    assert_eq!(f.head(), head);

    // Rebased onto the head, 2 lands, after 3.
    let p2b = f.patch(
        f.head(),
        &[("b.txt", Some("b-merged\n"))],
        "edit b, rebased",
    );
    f.queue(2, p2b);
    let report = f.process();
    assert_eq!(
        report.applied.iter().map(|(n, _)| *n).collect::<Vec<_>>(),
        [2]
    );
    assert_eq!(report.blocked, None);
    assert!(report.set_aside.is_empty());
    assert_eq!(report.queue_depth, 0);
    assert_eq!(report.last_applied, 3);
    assert_eq!(f.read("b.txt").as_deref(), Some("b-merged\n"));
    for n in 1..=3 {
        assert_eq!(f.inbox_commits(n), 1, "patch {n}");
        assert_eq!(f.reference(&inbox_ref("vol", n)), None);
    }
    assert_eq!(f.reference(&held_ref("vol", 2)), None);
    assert_eq!(f.reference(&applied_ref("vol", 2)), Some(p2b));
    let state = PatchState::load(&f.state.patch_state());
    assert_eq!(state.queue_depth, 0);
    assert_eq!(state.held, 0);
    assert_eq!(state.blocked, None);
    assert_eq!(state.last_applied, 3);
    let status = read_status(f.repo(), report.status).unwrap();
    assert_eq!(status.last_applied.as_ref().unwrap().inbox, 3);
    assert!(status.queue.is_empty());
}

/// Without its held mark, a set-aside number below the high-water is
/// stale: the mark is what keeps it queued (P2).
#[test]
fn a_set_aside_number_is_judged_again_only_with_its_mark() {
    if !have_git() {
        return;
    }
    let f = Fixture::new(&[("b.txt", "b0\n")]);
    let base = f.head();
    let p1 = f.patch(base, &[("b.txt", Some("iac\n"))], "edit b");
    let p2 = f.patch(base, &[("c.txt", Some("c\n"))], "add c");
    f.change(&[("b.txt", "workload\n")], &[]);
    f.queue(1, p1);
    f.queue(2, p2);
    assert_eq!(f.process().set_aside.len(), 1);
    f.repo()
        .find_reference(&held_ref("vol", 1))
        .unwrap()
        .delete()
        .unwrap();
    let report = f.process();
    assert!(report.set_aside.is_empty());
    assert_eq!(report.ignored.len(), 1);
    assert_eq!(report.ignored[0].why, "stale");
}

/// Crash (b) on a set-aside number below the high-water, then the
/// producer skips it: the interrupted write is put back (the branch has
/// not applied that number, whatever the high-water says), and the skip
/// lands once (P3).
#[test]
fn crash_after_the_tree_write_below_the_high_water() {
    if !have_git() {
        return;
    }
    let f = Fixture::new(&[("b.txt", "b0\n")]);
    let base = f.head();
    let p1 = f.patch(base, &[("b.txt", Some("iac\n"))], "edit b");
    let p2 = f.patch(base, &[("c.txt", Some("c\n"))], "add c");
    f.change(&[("b.txt", "workload\n")], &[]);
    f.queue(1, p1);
    f.queue(2, p2);
    let report = f.process();
    assert_eq!(report.last_applied, 2);
    // 1, rebased, was being written when the process died.
    let p1b = f.patch(f.head(), &[("b.txt", Some("merged\n"))], "edit b, rebased");
    f.queue(1, p1b);
    note_in_flight(&f, 1, p1b, &["b.txt"]);
    fs::write(f.root.join("b.txt"), "merged\n").unwrap();
    // The producer gives up on it.
    let skip = f.patch(f.head(), &[], "agentfs: skip patch 1");
    f.queue(1, skip);

    let report = f.process();
    assert_eq!(
        report.applied.iter().map(|(n, _)| *n).collect::<Vec<_>>(),
        [1]
    );
    assert_eq!(f.read("b.txt").as_deref(), Some("workload\n"));
    assert_eq!(f.at_head("b.txt").as_deref(), Some("workload\n"));
    assert_eq!(f.inbox_commits(1), 1);
    assert_eq!(f.reference(&held_ref("vol", 1)), None);
    assert_eq!(PatchState::load(&f.state.patch_state()).in_flight, None);
}

/// The same crash with the patch still queued: it lands once, and the
/// tree matches history.
#[test]
fn crash_after_the_tree_write_of_a_set_aside_patch_still_queued() {
    if !have_git() {
        return;
    }
    let f = Fixture::new(&[("b.txt", "b0\n")]);
    let base = f.head();
    let p1 = f.patch(base, &[("b.txt", Some("iac\n"))], "edit b");
    let p2 = f.patch(base, &[("c.txt", Some("c\n"))], "add c");
    f.change(&[("b.txt", "workload\n")], &[]);
    f.queue(1, p1);
    f.queue(2, p2);
    f.process();
    let p1b = f.patch(f.head(), &[("b.txt", Some("merged\n"))], "edit b, rebased");
    f.queue(1, p1b);
    note_in_flight(&f, 1, p1b, &["b.txt"]);
    fs::write(f.root.join("b.txt"), "merged\n").unwrap();
    let report = f.process();
    assert_eq!(report.applied.len(), 1, "{report:?}");
    assert_eq!(f.inbox_commits(1), 1);
    assert_eq!(f.at_head("b.txt").as_deref(), Some("merged\n"));
    assert_eq!(f.read("b.txt").as_deref(), Some("merged\n"));
}

/// Crash (a): the commit is made, the refs are not moved.
#[test]
fn crash_between_commit_and_ref_move() {
    if !have_git() {
        return;
    }
    let f = Fixture::new(&[("a.txt", "a0\n")]);
    let p1 = f.patch(f.head(), &[("a.txt", Some("a1\n"))], "edit a");
    f.queue(1, p1);
    f.process();
    let head = f.head();
    // Undo the ref moves and forget the cache.
    f.repo()
        .find_reference(&applied_ref("vol", 1))
        .unwrap()
        .delete()
        .unwrap();
    f.queue(1, p1);
    fs::remove_file(f.state.patch_state()).unwrap();

    let report = f.process();
    assert!(report.applied.is_empty());
    assert_eq!(report.already, [1]);
    assert_eq!(report.blocked, None);
    assert_eq!(report.last_applied, 1);
    assert_eq!(f.head(), head);
    assert_eq!(f.reference(&applied_ref("vol", 1)), Some(p1));
    assert_eq!(f.reference(&inbox_ref("vol", 1)), None);
    assert_eq!(f.inbox_commits(1), 1);
}

/// Crash (b): the tree is written, nothing is recorded.
#[test]
fn crash_after_the_tree_write() {
    if !have_git() {
        return;
    }
    let f = Fixture::new(&[("a.txt", "a0\n"), ("b.txt", "b0\n")]);
    let p1 = f.patch(
        f.head(),
        &[
            ("a.txt", Some("a1\n")),
            ("b.txt", None),
            ("d/e.txt", Some("e\n")),
        ],
        "patch",
    );
    f.queue(1, p1);
    // What apply wrote before the crash.
    fs::write(f.root.join("a.txt"), "a1\n").unwrap();
    fs::remove_file(f.root.join("b.txt")).unwrap();

    let report = f.process();
    assert_eq!(report.applied.len(), 1, "{report:?}");
    assert_eq!(f.inbox_commits(1), 1);
    assert_eq!(f.at_head("a.txt").as_deref(), Some("a1\n"));
    assert_eq!(f.at_head("b.txt"), None);
    assert_eq!(f.at_head("d/e.txt").as_deref(), Some("e\n"));
    assert_eq!(f.read("d/e.txt").as_deref(), Some("e\n"));
    // One commit for the patch, right after the seed.
    let head = f.repo().find_commit(f.head()).unwrap();
    assert!(inbox_from_message(head.parent(0).unwrap().message().unwrap()).is_none());
    assert_eq!(f.process().applied.len(), 0);
    assert_eq!(f.inbox_commits(1), 1);
}

/// Crash (c): the record is journaled, not committed.
#[test]
fn crash_before_the_commit() {
    if !have_git() {
        return;
    }
    let mut f = Fixture::new(&[("a.txt", "a0\n")]);
    let p1 = f.patch(f.head(), &[("a.txt", Some("a1\n"))], "edit a");
    f.queue(1, p1);
    // What Queue::apply_inner does, up to the commit.
    {
        let repo = f.repo();
        let head = repo.find_commit(f.head()).unwrap();
        let patch = repo.find_commit(p1).unwrap();
        let branch = PatchBranch::read(repo, head.id(), p1).unwrap();
        let Integrated::Clean { tree: merged } = GitMergeTree::new(f.state.git())
            .integrate(head.id(), &branch)
            .unwrap()
        else {
            panic!("the patch conflicts");
        };
        repo.odb().unwrap().refresh().unwrap();
        let plan = restore::plan_tree(repo, head.id(), p1, merged, "edit a".into()).unwrap();
        let tree = f.dir();
        let plan = restore::check(plan, repo, &f.recorder, &NoLfs, &tree, &NotLive, false).unwrap();
        let spec = OperationSpec {
            inbox: Some(1),
            author: Some(queue::author_of(&patch)),
            ..OperationSpec::new(
                OperationKind::Patch,
                p1.to_string(),
                "edit a".into(),
                CALLER,
            )
        };
        let guard = f
            .recorder
            .begin_operation_spec(spec, &[CALLER.pid])
            .unwrap();
        let applied = restore::apply(&plan, repo, &NoLfs, &tree, &NotLive, false).unwrap();
        restore::record_applied(&f.recorder, &tree, &applied, CALLER, RecordMode::Atomic).unwrap();
        guard.finish();
    }
    f.reopen();
    let report = f.process();
    assert!(report.applied.is_empty(), "{report:?}");
    assert_eq!(report.already, [1]);
    assert_eq!(f.inbox_commits(1), 1);
    assert_eq!(f.at_head("a.txt").as_deref(), Some("a1\n"));
    let head = f.repo().find_commit(f.head()).unwrap();
    assert_eq!(head.author().name(), Ok("Ada Lovelace"));
    assert_eq!(f.reference(&applied_ref("vol", 1)), Some(p1));
}

#[test]
fn a_patch_that_changes_nothing_gets_a_marker_commit() {
    if !have_git() {
        return;
    }
    let f = Fixture::new(&[("a.txt", "a0\n")]);
    let base = f.head();
    f.change(&[("a.txt", "a1\n")], &[]);
    let before = f.repo().find_commit(f.head()).unwrap();
    let seq = f.committer.history().head_seq().unwrap();
    let p1 = f.patch(base, &[("a.txt", Some("a1\n"))], "edit a");
    f.queue(1, p1);
    let report = f.process();
    assert_eq!(report.applied.len(), 1);
    let marker = f.repo().find_commit(f.head()).unwrap();
    assert_eq!(marker.parent_id(0).unwrap(), before.id());
    assert_eq!(marker.tree_id(), before.tree_id());
    assert_eq!(marker.author().name(), Ok("Ada Lovelace"));
    assert_eq!(inbox_from_message(marker.message().unwrap()), Some((1, p1)));
    assert_eq!(f.committer.history().head_seq().unwrap(), seq);
    // The next record still commits.
    f.change(&[("z.txt", "z\n")], &[]);
    assert_eq!(f.at_head("z.txt").as_deref(), Some("z\n"));
}

#[test]
fn a_gap_blocks() {
    if !have_git() {
        return;
    }
    let f = Fixture::new(&[("a.txt", "a0\n")]);
    let p1 = f.patch(f.head(), &[("a.txt", Some("a1\n"))], "1");
    f.queue(1, p1);
    f.process();
    let p3 = f.patch(f.head(), &[("a.txt", Some("a3\n"))], "3");
    f.queue(3, p3);
    let report = f.process();
    let blocked = report.blocked.unwrap();
    assert_eq!(blocked.reason, BlockReason::Gap);
    assert_eq!(blocked.inbox, 2);
    assert_eq!(blocked.patch, None);
    assert_eq!(report.queue_depth, 1);
    assert_eq!(f.read("a.txt").as_deref(), Some("a1\n"));
}

#[test]
fn merge_commits_root_commits_and_foreign_parents_are_held() {
    if !have_git() {
        return;
    }
    let f = Fixture::new(&[("a.txt", "a0\n")]);
    let head = f.head();
    let repo = f.repo();
    let sig = Signature::now("Ada", "ada@example.com").unwrap();
    let tree = repo.find_commit(head).unwrap().tree().unwrap();
    let other = repo.commit(None, &sig, &sig, "other", &tree, &[]).unwrap();
    let merge = {
        let parents = [
            &repo.find_commit(head).unwrap(),
            &repo.find_commit(other).unwrap(),
        ];
        repo.commit(None, &sig, &sig, "merge", &tree, &parents)
            .unwrap()
    };
    let cases = [
        (merge, BlockReason::MergeCommit),
        (other, BlockReason::Unsupported),
        (
            f.patch(other, &[("a.txt", Some("x\n"))], "foreign"),
            BlockReason::ParentNotInHistory,
        ),
    ];
    for (oid, reason) in cases {
        f.queue(1, oid);
        let report = f.process();
        assert_eq!(report.blocked, None);
        assert!(
            matches!(&report.set_aside[..], [(1, Verdict::Held { reason: r, .. })] if *r == reason),
            "{:?}",
            report.set_aside
        );
        assert_eq!(f.head(), head);
    }
}

#[test]
fn gitlinks_are_unsupported() {
    if !have_git() {
        return;
    }
    let f = Fixture::new(&[("a.txt", "a0\n")]);
    let repo = f.repo();
    let parent = repo.find_commit(f.head()).unwrap();
    let mut index = git2::Index::new().unwrap();
    index.read_tree(&parent.tree().unwrap()).unwrap();
    index.add(&entry("sub", parent.id(), 0o160000)).unwrap();
    let tree = repo.find_tree(index.write_tree_to(repo).unwrap()).unwrap();
    let sig = Signature::now("Ada", "ada@example.com").unwrap();
    let p = repo
        .commit(None, &sig, &sig, "submodule", &tree, &[&parent])
        .unwrap();
    f.queue(1, p);
    let report = f.process();
    let [(1, Verdict::Held { reason, paths, .. })] = &report.set_aside[..] else {
        panic!("{report:?}");
    };
    assert_eq!(*reason, BlockReason::Unsupported);
    assert_eq!(paths, &["sub"]);
}

#[test]
fn unrecorded_changes_block_and_leave_the_tree_alone() {
    if !have_git() {
        return;
    }
    let f = Fixture::new(&[("a.txt", "a0\n")]);
    let p1 = f.patch(f.head(), &[("a.txt", Some("a1\n"))], "edit a");
    f.queue(1, p1);
    // A change history does not have yet.
    fs::write(f.root.join("a.txt"), "live\n").unwrap();
    let head = f.head();
    let blocked = f.process().blocked.unwrap();
    assert_eq!(blocked.reason, BlockReason::Unrecorded, "{blocked:?}");
    assert_eq!(blocked.paths, ["a.txt"]);
    assert_eq!(f.read("a.txt").as_deref(), Some("live\n"));
    assert_eq!(f.head(), head);
}

#[test]
fn stale_entries_are_ignored_and_do_not_block() {
    if !have_git() {
        return;
    }
    let f = Fixture::new(&[("a.txt", "a0\n")]);
    let p1 = f.patch(f.head(), &[("a.txt", Some("a1\n"))], "1");
    f.queue(1, p1);
    f.process();
    // Someone re-queues 1 with another commit, and queues 2.
    let other = f.patch(f.head(), &[("a.txt", Some("other\n"))], "1 again");
    f.queue(1, other);
    let p2 = f.patch(f.head(), &[("b.txt", Some("b\n"))], "2");
    f.queue(2, p2);
    // A malformed name.
    f.repo()
        .reference("refs/agentfs/vol/inbox/3", p2, true, "test")
        .unwrap();
    let report = f.process();
    assert_eq!(
        report.applied.iter().map(|(n, _)| *n).collect::<Vec<_>>(),
        [2]
    );
    assert_eq!(report.blocked, None);
    let why: Vec<(&str, &str)> = report
        .ignored
        .iter()
        .map(|i| (i.ref_name.as_str(), i.why.as_str()))
        .collect();
    assert!(
        why.contains(&("refs/agentfs/vol/inbox/0000000001", "stale")),
        "{why:?}"
    );
    assert!(
        why.contains(&("refs/agentfs/vol/inbox/3", "malformed")),
        "{why:?}"
    );
    assert_eq!(f.read("a.txt").as_deref(), Some("a1\n"));
    assert_eq!(f.inbox_commits(1), 1);
}

#[test]
fn the_scan_cache_is_not_authoritative() {
    if !have_git() {
        return;
    }
    let f = Fixture::new(&[("a.txt", "a0\n")]);
    let p1 = f.patch(f.head(), &[("a.txt", Some("a1\n"))], "1");
    f.queue(1, p1);
    f.process();
    // A cache that lies about the scanned head (not on the branch).
    let mut state = PatchState::load(&f.state.patch_state());
    state.scanned_head = Some("1".repeat(40));
    state.last_applied = 0;
    state.save(&f.state.patch_state()).unwrap();
    f.queue(1, p1);
    let report = f.process();
    assert_eq!(report.already, [1]);
    assert_eq!(f.inbox_commits(1), 1);
    // An incremental scan from a good cache finds new ones too.
    let found = f
        .committer
        .history()
        .find_inbox_commits(Some(f.head()))
        .unwrap();
    assert!(found.is_empty());
    assert_eq!(
        f.committer.history().find_inbox_commits(None).unwrap()[&1].patch,
        p1
    );
}

/// What `apply_inner` notes before writing patch `n` (`p`) on the head.
fn note_in_flight(f: &Fixture, n: u64, p: Oid, paths: &[&str]) {
    let path = f.state.patch_state();
    let mut state = PatchState::load(&path);
    state.in_flight = Some(status::InFlight {
        inbox: n,
        patch: p.to_string(),
        head: f.head().to_string(),
        paths: paths.iter().map(|s| s.to_string()).collect(),
    });
    state.save(&path).unwrap();
}

/// Crash (b), then the producer skips the patch: what the interrupted
/// write left goes back to the head's state; nothing of it reaches the
/// tree or history.
#[test]
fn crash_after_the_tree_write_then_the_patch_is_skipped() {
    if !have_git() {
        return;
    }
    let f = Fixture::new(&[("a.txt", "a0\n"), ("b.txt", "b0\n")]);
    let base = f.head();
    let p1 = f.patch(
        base,
        &[
            ("a.txt", Some("a1\n")),
            ("b.txt", None),
            ("d/e.txt", Some("e\n")),
        ],
        "patch",
    );
    f.queue(1, p1);
    note_in_flight(&f, 1, p1, &["a.txt", "b.txt", "d/e.txt"]);
    fs::write(f.root.join("a.txt"), "a1\n").unwrap();
    fs::remove_file(f.root.join("b.txt")).unwrap();
    // The producer skips 1: an empty commit on the head.
    let skip = f.patch(base, &[], "agentfs: skip patch 1");
    f.queue(1, skip);

    let report = f.process();
    assert_eq!(report.applied.len(), 1, "{report:?}");
    assert_eq!(report.blocked, None);
    assert_eq!(f.read("a.txt").as_deref(), Some("a0\n"));
    assert_eq!(f.read("b.txt").as_deref(), Some("b0\n"));
    assert_eq!(f.read("d/e.txt"), None);
    assert_eq!(f.at_head("a.txt").as_deref(), Some("a0\n"));
    assert_eq!(f.at_head("b.txt").as_deref(), Some("b0\n"));
    // The marker commit for the skip, right on the seed.
    let head = f.repo().find_commit(f.head()).unwrap();
    assert_eq!(head.parent_id(0).unwrap(), base);
    assert_eq!(
        head.tree_id(),
        f.repo().find_commit(base).unwrap().tree_id()
    );
    assert_eq!(PatchState::load(&f.state.patch_state()).in_flight, None);
}

/// When the paths cannot be put back (here the head moved), they are
/// recorded as they stand, so the tree still matches history.
#[test]
fn an_interrupted_write_that_cannot_be_put_back_is_recorded() {
    if !have_git() {
        return;
    }
    let f = Fixture::new(&[("a.txt", "a0\n")]);
    let p1 = f.patch(f.head(), &[("a.txt", Some("a1\n"))], "patch");
    note_in_flight(&f, 1, p1, &["a.txt"]);
    let mut state = PatchState::load(&f.state.patch_state());
    state.in_flight.as_mut().unwrap().head = "1".repeat(40);
    state.save(&f.state.patch_state()).unwrap();
    fs::write(f.root.join("a.txt"), "a1\n").unwrap();
    // Patch 1 was withdrawn.
    let report = f.process();
    assert_eq!(report.applied.len(), 0, "{report:?}");
    assert_eq!(f.at_head("a.txt").as_deref(), Some("a1\n"));
    let head = f.repo().find_commit(f.head()).unwrap();
    assert!(inbox_from_message(head.message().unwrap()).is_none());
    assert_eq!(PatchState::load(&f.state.patch_state()).in_flight, None);
}

/// The same patch still queued: the run applies it as usual.
#[test]
fn an_interrupted_write_of_a_patch_still_queued_is_applied() {
    if !have_git() {
        return;
    }
    let f = Fixture::new(&[("a.txt", "a0\n")]);
    let p1 = f.patch(f.head(), &[("a.txt", Some("a1\n"))], "patch");
    f.queue(1, p1);
    note_in_flight(&f, 1, p1, &["a.txt"]);
    fs::write(f.root.join("a.txt"), "a1\n").unwrap();
    let report = f.process();
    assert_eq!(report.applied.len(), 1, "{report:?}");
    assert_eq!(f.inbox_commits(1), 1);
    assert_eq!(f.at_head("a.txt").as_deref(), Some("a1\n"));
}

/// A queue that cannot run is recorded as blocked, for the gauges.
#[test]
fn a_failed_run_is_recorded_as_blocked() {
    if !have_git() {
        return;
    }
    let f = Fixture::new(&[("a.txt", "a0\n")]);
    f.process();
    let before = PatchState::load(&f.state.patch_state());
    assert_eq!(before.blocked, None);
    // The status ref cannot be written: it is locked.
    fs::create_dir_all(f.state.git().join("refs/agentfs/vol")).unwrap();
    fs::write(f.state.git().join("refs/agentfs/vol/status.lock"), "").unwrap();
    let tree = f.dir();
    let telemetry = Telemetry::noop();
    let integrator = GitMergeTree::new(f.state.git());
    let mut queue = Queue::new(QueueContext {
        state: &f.state,
        recorder: &f.recorder,
        committer: &f.committer,
        tree: &tree,
        objects: &NoLfs,
        telemetry: &telemetry,
        caller: CALLER,
        integrator: &integrator,
        rule: &CleanMerge as &dyn AcceptanceRule,
    });
    assert!(queue.process(None).is_err());
    let after = PatchState::load(&f.state.patch_state());
    let blocked = after.blocked.unwrap();
    assert_eq!((blocked.inbox, blocked.reason), (0, BlockReason::Failed));
    assert!(
        blocked.message.contains("could not run"),
        "{}",
        blocked.message
    );
    assert!(after.updated_ms >= before.updated_ms);
}

/// A stale entry is a warning event.
#[test]
fn ignored_entries_raise_a_warning() {
    if !have_git() {
        return;
    }
    let f = Fixture::new(&[("a.txt", "a0\n")]);
    let p1 = f.patch(f.head(), &[("a.txt", Some("a1\n"))], "1");
    f.queue(1, p1);
    f.process();
    f.events();
    let other = f.patch(f.head(), &[("a.txt", Some("other\n"))], "1 again");
    f.queue(1, other);
    f.process();
    let warned: Vec<Event> = f
        .events()
        .into_iter()
        .filter(|e| matches!(e, Event::PatchIgnored { .. }))
        .collect();
    assert_eq!(
        warned,
        [Event::PatchIgnored {
            ref_name: inbox_ref("vol", 1),
            inbox: Some(1),
            patch: Some(other),
            why: "stale".into(),
        }]
    );
    assert!(warned[0].is_alarm());
}

/// A10: the same patch commit queued twice applies once; the second is a
/// marker commit carrying its inbox number, with nothing merged.
#[test]
fn a_patch_commit_queued_twice_applies_once() {
    if !have_git() {
        return;
    }
    let f = Fixture::new(&[("a.txt", "a0\n")]);
    let p = f.patch(f.head(), &[("a.txt", Some("a1\n"))], "edit a");
    f.queue(1, p);
    f.queue(2, p);
    f.events();
    let report = f.process();
    assert_eq!(
        report.applied.iter().map(|(n, _)| *n).collect::<Vec<_>>(),
        [1]
    );
    assert_eq!(report.duplicates, [(2, 1)]);
    assert_eq!(report.blocked, None);
    assert_eq!(report.last_applied, 2);
    assert_eq!(f.inbox_commits(1), 1);
    assert_eq!(f.inbox_commits(2), 1);
    let head = f.repo().find_commit(f.head()).unwrap();
    let message = head.message().unwrap();
    assert!(
        message.starts_with("duplicate of patch 1: edit a"),
        "{message}"
    );
    assert_eq!(inbox_from_message(message), Some((2, p)));
    // A marker: the tree of the commit that applied 1.
    assert_eq!(head.tree_id(), head.parent(0).unwrap().tree_id());
    assert_eq!(f.reference(&applied_ref("vol", 2)), Some(p));
    assert_eq!(f.reference(&inbox_ref("vol", 2)), None);
    let events = f.events();
    let duplicate = events
        .iter()
        .find(|e| matches!(e, Event::PatchDuplicate { .. }))
        .unwrap();
    assert!(!duplicate.is_alarm());
    assert!(matches!(
        duplicate,
        Event::PatchDuplicate {
            inbox: 2,
            applied_as: 1,
            ..
        }
    ));
    let status = read_status(f.repo(), report.status).unwrap();
    assert_eq!(status.applied_now, [1]);
    assert_eq!(status.duplicate_now.len(), 1);
    assert_eq!(status.duplicate_now[0].inbox, 2);
    assert_eq!(status.duplicate_now[0].patch, p.to_string());
    assert_eq!(status.duplicate_now[0].applied_as, 1);
    // A rerun converges: nothing new.
    let head = f.head();
    let again = f.process();
    assert!(again.applied.is_empty() && again.duplicates.is_empty());
    assert_eq!(f.head(), head);
}

/// A10: a duplicate in a later run, after the workload reverted the
/// patched file, leaves the file reverted.
#[test]
fn a_duplicate_never_reapplies_a_reverted_change() {
    if !have_git() {
        return;
    }
    let mut f = Fixture::new(&[("a.txt", "a0\n")]);
    let p = f.patch(f.head(), &[("a.txt", Some("a1\n"))], "edit a");
    f.queue(1, p);
    f.process();
    assert_eq!(f.read("a.txt").as_deref(), Some("a1\n"));
    f.change(&[("a.txt", "a0\n")], &[]);
    f.queue(2, p);
    // Across a reopen, so the scan cache plays no part.
    f.reopen();
    let report = f.process();
    assert_eq!(report.duplicates, [(2, 1)]);
    assert!(report.applied.is_empty());
    assert_eq!(f.read("a.txt").as_deref(), Some("a0\n"));
    assert_eq!(f.at_head("a.txt").as_deref(), Some("a0\n"));
}

impl Fixture {
    /// [`Queue::judge`] with the volume open, as a daemon judges it.
    fn judge(&self) -> super::queue::Judgement {
        let integrator = GitMergeTree::new(self.state.git());
        let events: Events = self.sink.clone();
        super::queue::Queue::judge(&super::queue::JudgeContext {
            state: &self.state,
            history: self.committer.history(),
            integrator: &integrator,
            rule: &CleanMerge,
            events: &events,
            telemetry: &Telemetry::noop(),
            committer: &crate::identity::Identity {
                name: "agentfs".into(),
                email: "agentfs@pod-0".into(),
            },
            fetch: None,
        })
        .unwrap()
    }

    /// Every ref but the status ref, with its target.
    fn refs_but_status(&self) -> Vec<(String, Oid)> {
        let mut out: Vec<(String, Oid)> = self
            .repo()
            .references()
            .unwrap()
            .map(|r| r.unwrap())
            .filter(|r| r.name().ok() != Some(status_ref("vol").as_str()))
            .map(|r| (r.name().unwrap().to_owned(), r.target().unwrap()))
            .collect();
        out.sort();
        out
    }
}

/// Bead agentfs-mia.5: judging lands nothing, writes no ref but the status
/// and no file of the tree, and every patch that would land is ready,
/// judged against the same head; the status carries the verdicts.
#[test]
fn judging_writes_only_the_status() {
    if !have_git() {
        return;
    }
    let f = Fixture::new(&[("a.txt", "a0\n"), ("b.txt", "b0\n")]);
    let base = f.head();
    let p1 = f.patch(base, &[("a.txt", Some("a1\n"))], "edit a");
    let p2 = f.patch(base, &[("b.txt", Some("b-from-iac\n"))], "edit b");
    let p3 = f.patch(base, &[("a.txt", Some("a-again\n"))], "edit a again");
    f.change(&[("b.txt", "b-from-workload\n")], &[]);
    for (n, p) in [(1, p1), (2, p2), (3, p3)] {
        f.queue(n, p);
    }
    let head = f.head();
    let refs = f.refs_but_status();
    let cache_before = PatchState::load(&f.state.patch_state());

    let judgement = f.judge();
    assert_eq!(f.head(), head, "nothing landed");
    assert_eq!(f.refs_but_status(), refs, "no ref but the status moved");
    assert_eq!(f.read("a.txt").as_deref(), Some("a0\n"));
    assert_eq!(f.read("b.txt").as_deref(), Some("b-from-workload\n"));
    let kinds: Vec<(u64, &str)> = judgement
        .verdicts
        .iter()
        .map(|(n, v)| (*n, v.kind()))
        .collect();
    // 3 changes a.txt as 1 does: judged against the same head, it is ready
    // too (ready is advisory until a landing).
    assert_eq!(kinds, [(1, "ready"), (2, "held"), (3, "ready")]);
    assert_eq!(judgement.blocked, None);
    assert_eq!(
        f.reference(&status_ref("vol")),
        Some(judgement.status),
        "the status ref is written"
    );
    let doc = read_status(f.repo(), judgement.status).unwrap();
    assert_eq!(doc.queue.len(), 3);
    assert_eq!(doc.queue[0].verdict, Some(Verdict::Ready));
    assert!(doc.applied_now.is_empty());
    let cache = PatchState::load(&f.state.patch_state());
    assert_eq!(cache.queue_depth, 3);
    assert_eq!(cache.held, 1);
    assert_eq!(cache.status_oid, Some(judgement.status.to_string()));
    assert_eq!(cache.scanned_head, cache_before.scanned_head);
    assert_eq!(cache.last_applied, cache_before.last_applied);
    assert!(
        f.events()
            .iter()
            .any(|e| matches!(e, Event::PatchHeld { inbox: 2, .. })),
        "a held patch is alarmed as at a run"
    );

    // A run lands by the same rules: 1 lands, and 3, ready against the old
    // head, now conflicts with it.
    let report = f.process();
    assert_eq!(
        report.applied.iter().map(|(n, _)| *n).collect::<Vec<_>>(),
        [1]
    );
    let kinds: Vec<(u64, &str)> = report
        .set_aside
        .iter()
        .map(|(n, v)| (*n, v.kind()))
        .collect();
    assert_eq!(kinds, [(2, "held"), (3, "held")]);
}

/// A notifier that records each notice and fails while told to.
#[derive(Default)]
struct Recording {
    seen: std::sync::Mutex<Vec<notify::Notice>>,
    fail: std::sync::atomic::AtomicBool,
}

impl notify::Notifier for Arc<Recording> {
    fn name(&self) -> &'static str {
        "hook"
    }

    fn notify(&self, notice: &notify::Notice) -> Result<(), notify::NotifyError> {
        self.seen.lock().unwrap().push(notice.clone());
        if self.fail.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(notify::NotifyError::Hook("exited with 1".into()));
        }
        Ok(())
    }
}

/// Bead agentfs-mia.5, P1: the cursor moves only after every method
/// delivered, so a failed notice is sent again at the next run, and once
/// delivered, nothing more.
#[test]
fn a_failed_notice_is_sent_again() {
    if !have_git() {
        return;
    }
    let f = Fixture::new(&[("a.txt", "a0\n"), ("b.txt", "b0\n")]);
    let base = f.head();
    let p1 = f.patch(base, &[("a.txt", Some("a1\n"))], "edit a");
    let p2 = f.patch(base, &[("b.txt", Some("b-from-iac\n"))], "edit b");
    f.change(&[("b.txt", "b-from-workload\n")], &[]);
    f.queue(1, p1);
    f.queue(2, p2);
    let recording = Arc::new(Recording::default());
    let notifiers: Vec<Box<dyn notify::Notifier>> = vec![Box::new(recording.clone())];
    let events: Events = f.sink.clone();
    let deliver = |status: Oid, landed: &[(u64, Oid)]| {
        notify::deliver(
            &f.state,
            f.repo(),
            status,
            landed,
            None,
            &notifiers,
            &events,
        )
        .unwrap()
    };

    recording
        .fail
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let report = f.process();
    let first = deliver(report.status, &report.applied);
    assert_eq!(
        first,
        notify::Delivery {
            sent: true,
            failed: vec!["hook".into()]
        }
    );
    assert!(f.events().iter().any(|e| matches!(
        e,
        Event::NotifyFailed { method: "hook", message } if message.contains("exited with 1")
    )));
    assert_eq!(
        notify::NotifyCursor::load(&f.state.notify_state()).delivered_status,
        None
    );

    // The next run changes nothing, and sends the same changes again.
    recording
        .fail
        .store(false, std::sync::atomic::Ordering::SeqCst);
    let again = f.process();
    assert!(again.applied.is_empty());
    assert_eq!(
        deliver(again.status, &again.applied).failed,
        Vec::<String>::new()
    );
    let seen = recording.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 2);
    let changes = |n: &notify::Notice| {
        n.changes
            .iter()
            .map(|c| {
                (
                    c.inbox,
                    c.to.as_ref().map(Verdict::kind),
                    c.landed.is_some(),
                )
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(
        changes(&seen[0]),
        [(1, None, true), (2, Some("held"), false)]
    );
    assert_eq!(changes(&seen[1]), changes(&seen[0]));
    assert_eq!(seen[1].changes[0].patch, p1.to_string());

    // Delivered: nothing more.
    let third = f.process();
    assert!(!deliver(third.status, &third.applied).sent);
    assert_eq!(recording.seen.lock().unwrap().len(), 2);
}
