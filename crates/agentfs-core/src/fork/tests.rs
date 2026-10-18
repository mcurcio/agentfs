//! Forked volumes against real state areas and bare remotes (design
//! §11.5): create, finish, publish and status, no FUSE.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use git2::{Oid, Repository};

use std::sync::Mutex;

use super::create::{self, CreateError, CreateRequest};
use super::finish::{
    self, AbandonError, FinishError, Finisher, ForkState, Met, PublishOutcome, RefusedReason,
    StatusError, refused_reason,
};
use super::identity::{ForkIdentity, ForkLink, PushTarget, change_ref, finish_ref};
use crate::confined::Dir;
use crate::events::{Event, Events, NullSink, PushFailureKind};
use crate::identity::Context;
use crate::lock::StateLock;
use crate::paths::RepoPath;
use crate::record::{Caller, OperationKind, Origin, Trigger};
use crate::restore::{self, Applied, RecordMode};
use crate::telemetry::Telemetry;
use crate::volume::{self, StateDir};

const CALLER: Caller = Caller {
    uid: 10000,
    gid: 10000,
    pid: 1,
};

fn context(volume: &str) -> Context {
    Context {
        volume: volume.into(),
        replica: "pod-0".into(),
        pod_uid: "u".into(),
        ..Context::default()
    }
}

/// A volume whose settings come from an included fragment, as a profile's
/// do, with a bare remote.
struct Vol {
    dir: tempfile::TempDir,
    state: StateDir,
    tree: PathBuf,
    fragment: PathBuf,
    remote: PathBuf,
}

impl Vol {
    fn new(name: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let remote = dir.path().join("remote.git");
        Repository::init_bare(&remote).unwrap();
        let fragment = dir.path().join("profile");
        fs::write(
            &fragment,
            format!("[remote \"origin\"]\n\turl = {}\n", remote.display()),
        )
        .unwrap();
        let state = StateDir::new(dir.path().join("state"));
        volume::init_with(&state, name, None, Some(std::slice::from_ref(&fragment))).unwrap();
        let tree = dir.path().join("tree");
        fs::create_dir_all(&tree).unwrap();
        Self {
            dir,
            state,
            tree,
            fragment,
            remote,
        }
    }

    fn name(&self) -> String {
        crate::config::Settings::load(&self.state.git())
            .unwrap()
            .volume
    }

    /// Writes `files` (content starting with `->`: a symlink to the rest)
    /// into the tree and records and commits them.
    fn change(&self, files: &[(&str, &str)]) {
        for (path, content) in files {
            let full = self.tree.join(path);
            fs::create_dir_all(full.parent().unwrap()).unwrap();
            let _ = fs::remove_file(&full);
            match content.strip_prefix("->") {
                Some(target) => std::os::unix::fs::symlink(target, full).unwrap(),
                None => fs::write(full, content).unwrap(),
            }
        }
        self.record(files.iter().map(|(p, _)| *p).collect());
    }

    fn record(&self, paths: Vec<&str>) {
        let name = self.name();
        let (recorder, committer, _) = volume::open(
            &self.state,
            &self.tree,
            context(&name),
            Arc::new(NullSink),
            &Telemetry::noop(),
        )
        .unwrap();
        let applied = Applied {
            written: paths.iter().map(|p| RepoPath::new(*p).unwrap()).collect(),
            removed: Vec::new(),
        };
        restore::record_applied(
            &recorder,
            &Dir::open(&self.tree).unwrap(),
            &applied,
            CALLER,
            RecordMode::Chunked(100),
        )
        .unwrap();
        committer.commit_pending().unwrap();
    }

    fn repo(&self) -> Repository {
        Repository::open_bare(self.state.git()).unwrap()
    }

    fn head(&self) -> Option<Oid> {
        let settings = crate::config::Settings::load(&self.state.git()).unwrap();
        self.repo().refname_to_id(&settings.branch_ref).ok()
    }

    /// Every ref and file of the state area, for "left untouched".
    fn snapshot(&self) -> BTreeMap<String, Vec<u8>> {
        let mut out = BTreeMap::new();
        for r in self.repo().references().unwrap() {
            let r = r.unwrap();
            out.insert(
                format!("ref {}", r.name().unwrap()),
                r.target()
                    .map(|o| o.to_string())
                    .unwrap_or_default()
                    .into_bytes(),
            );
        }
        let mut dirs = vec![self.state.root().to_path_buf()];
        while let Some(dir) = dirs.pop() {
            for entry in fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                // libgit2 writes nothing a fork reads; `objects` is
                // compared through the refs.
                if path.ends_with("objects") {
                    continue;
                }
                let meta = fs::symlink_metadata(&path).unwrap();
                if meta.is_dir() {
                    dirs.push(path);
                } else if meta.is_file() && !path.ends_with("lock") {
                    out.insert(path.display().to_string(), fs::read(path).unwrap());
                }
            }
        }
        out
    }
}

/// A fork of `source` named `name`, in a new directory.
struct Fork {
    _dir: tempfile::TempDir,
    state: StateDir,
    tree: PathBuf,
}

impl Fork {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        Self {
            state: StateDir::new(dir.path().join("state")),
            tree: dir.path().join("tree"),
            _dir: dir,
        }
    }

    fn create(&self, source: &Vol, name: &str) -> Result<ForkIdentity, CreateError> {
        self.create_with(source, name, ForkLink::Linked)
    }

    fn create_with(
        &self,
        source: &Vol,
        name: &str,
        link: ForkLink,
    ) -> Result<ForkIdentity, CreateError> {
        self.create_with_remotes(source, name, link, None)
    }

    fn create_with_remotes(
        &self,
        source: &Vol,
        name: &str,
        link: ForkLink,
        remotes: Option<&Path>,
    ) -> Result<ForkIdentity, CreateError> {
        create::create(&CreateRequest {
            source: &source.state,
            fork: &self.state,
            tree: &self.tree,
            name,
            includes: std::slice::from_ref(&source.fragment),
            push_target: PushTarget::Branch,
            link,
            remotes,
        })
    }

    fn finisher(&self, lock: &StateLock) -> Result<Finisher, FinishError> {
        self.finisher_with(lock, Arc::new(NullSink))
    }

    fn finisher_with(&self, lock: &StateLock, events: Events) -> Result<Finisher, FinishError> {
        Finisher::open(
            &self.state,
            &self.tree,
            context("fork"),
            events,
            &Telemetry::noop(),
            lock,
        )
    }

    fn lock(&self) -> StateLock {
        StateLock::acquire(&self.state, "test", Duration::ZERO).unwrap()
    }

    fn as_vol(&self) -> ForkVol<'_> {
        ForkVol(self)
    }
}

/// Recording in a fork, between create and finish.
struct ForkVol<'a>(&'a Fork);

impl ForkVol<'_> {
    fn write(&self, path: &str, content: &str) {
        let full = self.0.tree.join(path);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(full, content).unwrap();
        let (recorder, committer, _) = volume::open(
            &self.0.state,
            &self.0.tree,
            context("fork"),
            Arc::new(NullSink),
            &Telemetry::noop(),
        )
        .unwrap();
        restore::record_applied(
            &recorder,
            &Dir::open(&self.0.tree).unwrap(),
            &Applied {
                written: vec![RepoPath::new(path).unwrap()],
                removed: Vec::new(),
            },
            CALLER,
            RecordMode::Chunked(100),
        )
        .unwrap();
        committer.commit_pending().unwrap();
    }
}

fn read_link(path: &Path) -> String {
    fs::read_link(path).unwrap().display().to_string()
}

/// A4, A6: create reads the source as it is, leaves it alone, and writes
/// base's tree as stored, never through a symlink.
#[test]
fn create_reads_the_source_live_and_leaves_it_untouched() {
    let source = Vol::new("src");
    let outside = source.dir.path().join("outside");
    fs::create_dir_all(&outside).unwrap();
    source.change(&[
        ("a.txt", "a\n"),
        ("bin/run", "#!/bin/sh\n"),
        ("link", "->a.txt"),
        ("out", &format!("->{}", outside.display())),
        ("d/out/f.txt", "beneath\n"),
    ]);
    fs::set_permissions(
        source.tree.join("bin/run"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    source.record(vec!["bin/run"]);
    let base = source.head().unwrap();
    let before = source.snapshot();
    // Another process holds the source: create takes no lock on it.
    let held = StateLock::acquire(&source.state, "test", Duration::ZERO).unwrap();
    let fork = Fork::new();
    let identity = fork.create(&source, "src-1").unwrap();
    drop(held);
    assert_eq!(identity.base, base);
    assert_eq!(identity.source, "src");
    assert_eq!(identity.fork, "src-1");
    assert_eq!(source.snapshot(), before, "the source changed");

    let settings = crate::config::Settings::load(&fork.state.git()).unwrap();
    assert_eq!(settings.fork.as_ref(), Some(&identity));
    assert_eq!(settings.branch_ref, "refs/heads/agentfs/src-1");
    let repo = Repository::open_bare(fork.state.git()).unwrap();
    assert_eq!(repo.refname_to_id(&settings.branch_ref).unwrap(), base);
    // The objects are the fork's own: no alternates.
    assert!(!fork.state.git().join("objects/info/alternates").exists());
    // The tree is base's, byte for byte.
    assert_eq!(fs::read_to_string(fork.tree.join("a.txt")).unwrap(), "a\n");
    let mode = fs::metadata(fork.tree.join("bin/run"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o111, 0o111, "{mode:o}");
    assert_eq!(read_link(&fork.tree.join("link")), "a.txt");
    assert_eq!(
        read_link(&fork.tree.join("out")),
        outside.display().to_string()
    );
    assert_eq!(
        fs::read_to_string(fork.tree.join("d/out/f.txt")).unwrap(),
        "beneath\n"
    );
    assert_eq!(fs::read_dir(outside).unwrap().count(), 0, "wrote outside");

    // A second create after the source moved returns the original base.
    source.change(&[("a.txt", "moved\n")]);
    let again = fork.create(&source, "src-1").unwrap();
    assert_eq!(again, identity);
    assert_eq!(fs::read_to_string(fork.tree.join("a.txt")).unwrap(), "a\n");
}

/// A5: what create refuses, changing nothing.
#[test]
fn create_refuses_and_changes_nothing() {
    // No history.
    let empty = Vol::new("empty");
    let fork = Fork::new();
    assert!(matches!(
        fork.create(&empty, "empty-1"),
        Err(CreateError::SourceHasNoHistory { .. })
    ));
    assert!(!fork.state.root().exists());

    // A fork of a fork.
    let source = Vol::new("src");
    source.change(&[("a.txt", "a\n")]);
    let first = Fork::new();
    first.create(&source, "src-1").unwrap();
    let as_source = Vol {
        dir: tempfile::tempdir().unwrap(),
        state: first.state.clone(),
        tree: first.tree.clone(),
        fragment: source.fragment.clone(),
        remote: source.remote.clone(),
    };
    let second = Fork::new();
    assert!(matches!(
        second.create(&as_source, "src-1-1"),
        Err(CreateError::SourceIsFork { .. })
    ));
    assert!(!second.state.root().exists());

    // No fork remote: two remotes and no patch remote.
    let mut two = fs::read_to_string(source.fragment.as_path()).unwrap();
    two.push_str("[remote \"other\"]\n\turl = /nowhere\n");
    fs::write(source.fragment.as_path(), two).unwrap();
    let third = Fork::new();
    assert!(matches!(
        third.create(&source, "src-2"),
        Err(CreateError::NoForkRemote { .. })
    ));
    assert!(!third.state.root().exists());
    // An unlinked fork needs no fork remote: the same source forks, and
    // the fork is local-only (it never pushes to its source's remotes).
    let unlinked = Fork::new();
    unlinked
        .create_with(&source, "src-3", ForkLink::Unlinked)
        .unwrap();
    let settings = crate::config::Settings::load(&unlinked.state.git()).unwrap();
    assert!(settings.is_unlinked_fork());
    assert_eq!(settings.remotes, crate::config::Remotes::LocalOnly);

    // A state area of another volume.
    let source = Vol::new("src");
    source.change(&[("a.txt", "a\n")]);
    let taken = Fork::new();
    volume::init(&taken.state, "other", None).unwrap();
    assert!(matches!(
        taken.create(&source, "src-3"),
        Err(CreateError::Exists { found }) if found == "other"
    ));
    // A bad name.
    assert!(matches!(
        Fork::new().create(&source, "a/b"),
        Err(CreateError::BadName { .. })
    ));
}

/// A5: a CLI source whose settings live in its own config cannot be
/// forked from its includes alone.
#[test]
fn settings_files_need_the_settings_included() {
    let source = Vol::new("src");
    assert_eq!(
        create::settings_files(&source.state).unwrap(),
        std::slice::from_ref(&source.fragment)
    );
    let mut config = git2::Config::open(&source.state.git().join("config")).unwrap();
    config.set_str("remote.own.url", "/own").unwrap();
    match create::settings_files(&source.state) {
        Err(CreateError::SourceSettingsNotIncluded { keys, .. }) => {
            assert_eq!(keys, ["remote.own.url"]);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn unique_names_are_valid_and_differ() {
    let a = create::unique_name("src").unwrap();
    let b = create::unique_name("src").unwrap();
    assert!(a.starts_with("src-") && a.len() == 12, "{a}");
    assert_ne!(a, b);
    assert!(matches!(
        create::unique_name("-x"),
        Err(CreateError::BadName { .. })
    ));
}

/// A7, A8, A9: finish once, publish to the fork remote, and the status
/// DeleteVolume reads.
#[test]
fn finish_and_publish_a_fork() {
    let source = Vol::new("src");
    source.change(&[("a.txt", "a\n")]);
    let fork = Fork::new();
    let identity = fork.create(&source, "src-1").unwrap();
    let status = finish::status(&fork.state).unwrap().unwrap();
    assert_eq!(status.state, ForkState::Active);
    assert_eq!(status.identity, identity);
    fork.as_vol().write("b.txt", "b\n");

    let lock = fork.lock();
    let mut finisher = fork.finisher(&lock).unwrap();
    let record = finisher.finish().unwrap();
    let repo = Repository::open_bare(fork.state.git()).unwrap();
    let head = repo.refname_to_id("refs/heads/agentfs/src-1").unwrap();
    assert_eq!(record.finish, head);
    assert_eq!(record.base, identity.base);
    assert_eq!(record.remote, "origin");
    assert_eq!(record.unrecorded, 0);
    let finish_commit = repo.find_commit(head).unwrap();
    let message = finish_commit.message().unwrap();
    for trailer in [
        "Agentfs-Seq: ",
        "Agentfs-Trigger: finish\n",
        "Agentfs-Fork: src-1\n",
        "Agentfs-Source: src\n",
        "Agentfs-Fork-Remote: origin\n",
        "Agentfs-Unrecorded: 0\n",
    ] {
        assert!(message.contains(trailer), "{trailer}: {message}");
    }
    assert!(crate::history::seq_from_message(message).is_some());
    let change = repo.find_commit(record.change.unwrap()).unwrap();
    assert_eq!(change.parent_ids().collect::<Vec<_>>(), [identity.base]);
    assert_eq!(change.tree_id(), finish_commit.tree_id());
    assert_eq!(
        repo.refname_to_id(&change_ref("src-1")).unwrap(),
        change.id()
    );
    assert_eq!(repo.refname_to_id(&finish_ref("src-1")).unwrap(), head);
    // Once: a second finish is the same record, with no new commit.
    assert_eq!(finisher.finish().unwrap(), record);
    assert_eq!(
        repo.refname_to_id("refs/heads/agentfs/src-1").unwrap(),
        head
    );
    assert_eq!(
        finish::status(&fork.state).unwrap().unwrap().state,
        ForkState::Finished
    );

    // Published only once the remote holds both refs.
    assert_eq!(finisher.publish(None).unwrap(), PublishOutcome::Published);
    let remote = Repository::open_bare(source.remote.as_path()).unwrap();
    assert_eq!(
        remote.refname_to_id("refs/heads/agentfs/src-1").unwrap(),
        head
    );
    assert_eq!(
        remote.refname_to_id(&change_ref("src-1")).unwrap(),
        change.id()
    );
    assert_eq!(remote.refname_to_id(&finish_ref("src-1")).unwrap(), head);
    let status = finish::status(&fork.state).unwrap().unwrap();
    assert_eq!(status.state, ForkState::Published);
    assert!(status.state.may_delete());
    // The source's branch is untouched by the fork's pushes.
    assert!(remote.refname_to_id("refs/heads/agentfs/src").is_err());

    // A mark naming another finish commit is not published.
    let mark = fork.state.fork_mark();
    let text = fs::read_to_string(&mark).unwrap();
    fs::write(&mark, text.replace(&head.to_string(), &"1".repeat(40))).unwrap();
    assert_eq!(
        finish::status(&fork.state).unwrap().unwrap().state,
        ForkState::Finished
    );
    // An unreadable mark is an error, never a state.
    fs::write(mark, "{not json").unwrap();
    assert!(matches!(
        finish::status(&fork.state),
        Err(StatusError::Mark(_))
    ));
    // A non-fork has no fork status.
    assert_eq!(finish::status(&source.state).unwrap(), None);
}

/// A repair queue's line for `path` (the FUSE layer's format).
fn repair_line(path: &str) -> String {
    let hex: String = path.bytes().map(|b| format!("{b:02x}")).collect();
    format!("{} {} {} {hex}\n", CALLER.uid, CALLER.gid, CALLER.pid)
}

/// Finish records what every repair queue left (design §11.5): the
/// single-process `meta/repairs`, a front slot's queue (CSI: no next front
/// comes for a deleted fork), and a slot's temporary file a crash left,
/// each entry as the pod that queued it; what it cannot record is counted.
#[test]
fn finish_records_the_paths_every_repair_queue_left() {
    let source = Vol::new("src");
    source.change(&[("a.txt", "a\n")]);
    let fork = Fork::new();
    fork.create(&source, "src-1").unwrap();
    for (path, content) in [("x.txt", "x\n"), ("y.txt", "y\n"), ("z.txt", "z\n")] {
        fs::write(fork.tree.join(path), content).unwrap();
    }
    fs::create_dir_all(fork.state.meta()).unwrap();
    fs::write(fork.state.repairs(), repair_line("y.txt")).unwrap();
    fs::create_dir_all(fork.state.fronts_dir()).unwrap();
    let origin = serde_json::to_string(&Origin {
        namespace: "ns".into(),
        pod: "pod-a".into(),
        pod_uid: "pod-a-uid".into(),
        node: "n".into(),
        replica: "pod-a".into(),
    })
    .unwrap();
    fs::write(
        fork.state.front_repairs(0),
        format!("@origin {origin}\n{}", repair_line("x.txt")),
    )
    .unwrap();
    // Slot 3: only the temporary file of a rewrite, with a torn last line
    // and a path that can never be recorded.
    fs::write(
        fork.state.fronts_dir().join("slot-3.tmp"),
        format!(
            "{}{}1 2 3 7a2e",
            repair_line("z.txt"),
            repair_line("a/../b")
        ),
    )
    .unwrap();

    let lock = fork.lock();
    let record = fork.finisher(&lock).unwrap().finish().unwrap();
    assert_eq!(record.unrecorded, 1, "a/../b is counted");
    let repo = Repository::open_bare(fork.state.git()).unwrap();
    let change = repo.find_commit(record.change.unwrap()).unwrap();
    let tree = change.tree().unwrap();
    for (path, content) in [("x.txt", "x\n"), ("y.txt", "y\n"), ("z.txt", "z\n")] {
        let blob = tree
            .get_path(Path::new(path))
            .unwrap()
            .to_object(&repo)
            .unwrap();
        assert_eq!(
            blob.as_blob().unwrap().content(),
            content.as_bytes(),
            "{path}"
        );
    }
    let message = repo
        .find_commit(record.finish)
        .unwrap()
        .message()
        .unwrap()
        .to_owned();
    assert!(message.contains("Agentfs-Unrecorded: 1\n"), "{message}");
    // x.txt, from slot 0, is committed as the pod that queued it.
    let mut walk = repo.revwalk().unwrap();
    walk.push(record.finish).unwrap();
    let by_pod_a = walk
        .map(|id| repo.find_commit(id.unwrap()).unwrap())
        .filter(|c| {
            c.message()
                .unwrap_or_default()
                .contains("Agentfs-Pod: pod-a\n")
        })
        .count();
    assert_eq!(by_pod_a, 1, "one commit records slot 0's repair as pod-a");
}

/// A volume that is not a fork has no fork status even when its settings
/// cannot be loaded (its profile include removed): DeleteVolume of it
/// never waits on them.
#[test]
fn a_volume_that_is_not_a_fork_has_no_status_whatever_its_settings() {
    let source = Vol::new("src");
    fs::remove_file(&source.fragment).unwrap();
    assert!(crate::config::Settings::load(&source.state.git()).is_err());
    assert_eq!(finish::status(&source.state).unwrap(), None);
    // A fork whose settings fail to load fails closed.
    let source = Vol::new("src");
    source.change(&[("a.txt", "a\n")]);
    let fork = Fork::new();
    fork.create(&source, "src-1").unwrap();
    fs::remove_file(&source.fragment).unwrap();
    assert!(finish::status(&fork.state).is_err());
}

#[test]
fn a_fork_without_edits_has_no_change_and_a_non_fork_cannot_finish() {
    let source = Vol::new("src");
    source.change(&[("a.txt", "a\n")]);
    let fork = Fork::new();
    fork.create(&source, "src-1").unwrap();
    let lock = fork.lock();
    let record = fork.finisher(&lock).unwrap().finish().unwrap();
    assert_eq!(record.change, None);
    let repo = Repository::open_bare(fork.state.git()).unwrap();
    assert!(repo.find_reference(&change_ref("src-1")).is_err());
    let message = repo
        .find_commit(record.finish)
        .unwrap()
        .message()
        .unwrap()
        .to_owned();
    assert!(!message.contains("Agentfs-Change"), "{message}");

    let lock = StateLock::acquire(&source.state, "test", Duration::ZERO).unwrap();
    let opened = Finisher::open(
        &source.state,
        &source.tree,
        context("src"),
        Arc::new(NullSink),
        &Telemetry::noop(),
        &lock,
    );
    assert!(matches!(opened, Err(FinishError::NotAFork)));
}

#[test]
fn finish_with_no_fork_remote_writes_nothing() {
    let source = Vol::new("src");
    source.change(&[("a.txt", "a\n")]);
    let fork = Fork::new();
    fork.create(&source, "src-1").unwrap();
    // The profile now has two remotes and no patch remote.
    let mut two = fs::read_to_string(source.fragment.as_path()).unwrap();
    two.push_str("[remote \"other\"]\n\turl = /nowhere\n");
    fs::write(source.fragment.as_path(), two).unwrap();
    let repo = Repository::open_bare(fork.state.git()).unwrap();
    let head = repo.refname_to_id("refs/heads/agentfs/src-1").unwrap();
    let lock = fork.lock();
    let result = fork.finisher(&lock).unwrap().finish();
    assert!(
        matches!(result, Err(FinishError::NoForkRemote { .. })),
        "{result:?}"
    );
    assert_eq!(
        repo.refname_to_id("refs/heads/agentfs/src-1").unwrap(),
        head
    );
    assert!(repo.find_reference(&finish_ref("src-1")).is_err());
    assert_eq!(
        finish::status(&fork.state).unwrap().unwrap().state,
        ForkState::Active
    );
}

/// A8, B2: an unreachable remote never yields Published or a mark: it is
/// pending, and retried.
#[test]
fn publish_waits_for_an_unreachable_remote() {
    let source = Vol::new("src");
    source.change(&[("a.txt", "a\n")]);
    let fork = Fork::new();
    fork.create(&source, "src-1").unwrap();
    fork.as_vol().write("b.txt", "b\n");
    let parked = source.dir.path().join("parked.git");
    fs::rename(source.remote.as_path(), &parked).unwrap();
    let lock = fork.lock();
    let (events, seen) = recording();
    let mut finisher = fork.finisher_with(&lock, events).unwrap();
    let outcome = finisher
        .publish(Some(Instant::now() + Duration::from_millis(300)))
        .unwrap();
    assert!(
        matches!(outcome, PublishOutcome::Pending { .. }),
        "{outcome:?}"
    );
    // A linked fork that cannot reach its source alarms, and its change
    // reached nowhere else (design §11.5 "Fork link").
    assert!(
        seen.lock()
            .unwrap()
            .iter()
            .any(|e| matches!(e, Event::PushFailed { .. })),
        "no alarm while the fork remote is unreachable"
    );
    assert!(!fork.state.fork_mark().exists());
    assert_eq!(
        finish::status(&fork.state).unwrap().unwrap().state,
        ForkState::Finished
    );
    fs::rename(parked, source.remote.as_path()).unwrap();
    assert_eq!(finisher.publish(None).unwrap(), PublishOutcome::Published);
}

/// The source's branch on its remote, so the remote holds its head.
fn push_source_branch(source: &Vol) {
    let repo = source.repo();
    let head = source.head().unwrap();
    let mut push = repo
        .remote_anonymous(&source.remote.display().to_string())
        .unwrap();
    push.push(&[format!("{head}:refs/heads/agentfs/src")], None)
        .unwrap();
}

/// Every event raised, for counting.
fn recording() -> (Events, Arc<Mutex<Vec<Event>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    let events: Events = Arc::new(move |event: Event| sink.lock().unwrap().push(event));
    (events, seen)
}

fn refusals(seen: &Mutex<Vec<Event>>) -> Vec<Event> {
    seen.lock()
        .unwrap()
        .iter()
        .filter(|e| matches!(e, Event::ForkRefused { .. }))
        .cloned()
        .collect()
}

/// B1: a finish ref another writer holds is refused for good: the refused
/// mark, one ForkRefused, the ref never moved, nothing retried; `--retry`
/// once the ref is gone publishes.
#[test]
fn another_writers_finish_ref_is_refused_until_retried() {
    let source = Vol::new("src");
    source.change(&[("a.txt", "a\n")]);
    let fork = Fork::new();
    fork.create(&source, "src-1").unwrap();
    fork.as_vol().write("b.txt", "b\n");
    let remote = Repository::open_bare(source.remote.as_path()).unwrap();
    let source_head = source.head().unwrap();
    // The remote needs the commit it names: push the source's branch.
    push_source_branch(&source);
    remote
        .reference(&finish_ref("src-1"), source_head, true, "test")
        .unwrap();
    let (events, seen) = recording();
    let lock = fork.lock();
    let mut finisher = fork.finisher_with(&lock, events).unwrap();
    let outcome = finisher.publish(None).unwrap();
    assert!(
        matches!(
            outcome,
            PublishOutcome::Refused {
                reason: RefusedReason::AnotherWriter,
                ..
            }
        ),
        "{outcome:?}"
    );
    assert_eq!(
        remote.refname_to_id(&finish_ref("src-1")).unwrap(),
        source_head
    );
    let status = finish::status(&fork.state).unwrap().unwrap();
    assert_eq!(status.state, ForkState::Refused);
    let mark = status.mark.unwrap();
    let refused = mark.refused.unwrap();
    assert_eq!(refused.reason, RefusedReason::AnotherWriter);
    assert_eq!(refused.remote, "origin");
    assert_eq!(refused.finish, status.record.unwrap().finish.to_string());
    assert!(mark.published.is_none());
    assert_eq!(refusals(&seen).len(), 1);
    // Publishing again returns the refusal, with no push and no alarm.
    let again = finisher.publish(None).unwrap();
    assert!(matches!(again, PublishOutcome::Refused { .. }), "{again:?}");
    assert_eq!(refusals(&seen).len(), 1, "ForkRefused is raised once");
    // The operator removes the ref and retries.
    remote
        .find_reference(&finish_ref("src-1"))
        .unwrap()
        .delete()
        .unwrap();
    finisher.retry_refused().unwrap();
    assert_eq!(finisher.publish(None).unwrap(), PublishOutcome::Published);
    let status = finish::status(&fork.state).unwrap().unwrap();
    assert_eq!(status.state, ForkState::Published);
    assert!(status.mark.unwrap().refused.is_none());
}

/// B2: the single decider of "permanent".
#[test]
fn refused_reasons_are_decided_in_one_place() {
    use PushFailureKind::*;
    assert_eq!(
        refused_reason(Met::AnotherWriter),
        Some(RefusedReason::AnotherWriter)
    );
    assert_eq!(
        refused_reason(Met::RemoteGone),
        Some(RefusedReason::RemoteGone)
    );
    assert_eq!(refused_reason(Met::Unsafe), Some(RefusedReason::Unsafe));
    assert_eq!(
        refused_reason(Met::CreateOnly(Refused)),
        Some(RefusedReason::Rejected)
    );
    for met in [Met::Branch(TooLarge), Met::CreateOnly(TooLarge)] {
        assert_eq!(
            refused_reason(met),
            Some(RefusedReason::TooLarge),
            "{met:?}"
        );
    }
    for kind in [Unreachable, Auth, UntrustedHost, NonFastForward, Local] {
        assert_eq!(refused_reason(Met::Branch(kind)), None, "{kind:?}");
        assert_eq!(refused_reason(Met::CreateOnly(kind)), None, "{kind:?}");
    }
    // A branch push the remote refuses (a hook on the branch, a lock) is
    // retried, as for any volume (§11.3).
    assert_eq!(refused_reason(Met::Branch(Refused)), None);
    for (reason, text) in [
        (RefusedReason::AnotherWriter, "another_writer"),
        (RefusedReason::TooLarge, "too_large"),
        (RefusedReason::Rejected, "rejected"),
        (RefusedReason::RemoteGone, "remote_gone"),
        (RefusedReason::Unsafe, "unsafe"),
    ] {
        assert_eq!(reason.as_str(), text);
        assert_eq!(
            serde_json::to_string(&reason).unwrap(),
            format!("\"{text}\"")
        );
    }
}

/// Pseudo-random hex text, which compresses to about half its size.
fn noise(bytes: usize) -> String {
    let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
    (0..bytes / 16)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            format!("{x:016x}")
        })
        .collect()
}

/// B2: one commit over the fork remote's agentfsMaxPushSize is refused
/// as too large.
#[test]
fn a_commit_over_the_push_size_is_refused_as_too_large() {
    let source = Vol::new("src");
    source.change(&[("a.txt", "a\n")]);
    let mut profile = fs::read_to_string(source.fragment.as_path()).unwrap();
    profile.push_str("\tagentfsMaxPushSize = 8192\n");
    fs::write(source.fragment.as_path(), profile).unwrap();
    let fork = Fork::new();
    fork.create(&source, "src-1").unwrap();
    fork.as_vol().write("big.txt", &noise(256 * 1024));
    let lock = fork.lock();
    let mut finisher = fork.finisher(&lock).unwrap();
    let outcome = finisher.publish(None).unwrap();
    assert!(
        matches!(
            outcome,
            PublishOutcome::Refused {
                reason: RefusedReason::TooLarge,
                ..
            }
        ),
        "{outcome:?}"
    );
    assert_eq!(
        finish::status(&fork.state).unwrap().unwrap().state,
        ForkState::Refused
    );
    let remote = Repository::open_bare(source.remote.as_path()).unwrap();
    assert!(remote.find_reference(&finish_ref("src-1")).is_err());
}

/// B3: the fork remote removed from the profile after finish is refused
/// as gone at the next publish, not retried until a deadline.
#[test]
fn a_fork_remote_removed_after_finish_is_refused_as_gone() {
    let source = Vol::new("src");
    source.change(&[("a.txt", "a\n")]);
    let fork = Fork::new();
    fork.create(&source, "src-1").unwrap();
    fork.as_vol().write("b.txt", "b\n");
    let lock = fork.lock();
    let mut finisher = fork.finisher(&lock).unwrap();
    finisher.finish().unwrap();
    fs::write(source.fragment.as_path(), "").unwrap();
    let started = Instant::now();
    let outcome = finisher
        .publish(Some(Instant::now() + Duration::from_secs(30)))
        .unwrap();
    assert!(
        matches!(
            outcome,
            PublishOutcome::Refused {
                reason: RefusedReason::RemoteGone,
                ..
            }
        ),
        "{outcome:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "it was retried"
    );
    assert_eq!(
        finish::status(&fork.state).unwrap().unwrap().state,
        ForkState::Refused
    );
}

/// The remote's refs, for "unchanged".
fn remote_refs(path: &Path) -> BTreeMap<String, Oid> {
    let repo = Repository::open_bare(path).unwrap();
    repo.references()
        .unwrap()
        .map(|r| {
            let r = r.unwrap();
            (r.name().unwrap().to_owned(), r.target().unwrap())
        })
        .collect()
}

const OPERATOR: Caller = Caller {
    uid: 0,
    gid: 0,
    pid: 2,
};

/// B4: abandon records the discard in any state but published; after it
/// nothing the finisher runs pushes, even between two attempts.
#[test]
fn an_abandoned_fork_is_never_pushed_again() {
    let source = Vol::new("src");
    source.change(&[("a.txt", "a\n")]);
    // Active.
    let active = Fork::new();
    active.create(&source, "src-1").unwrap();
    let lock = active.lock();
    assert!(matches!(
        finish::abandon(&active.state, &lock, "  ", OPERATOR),
        Err(AbandonError::EmptyReason)
    ));
    let status = finish::abandon(&active.state, &lock, "not needed", OPERATOR).unwrap();
    assert_eq!(status.state, ForkState::Abandoned);
    let mark = status.mark.unwrap().abandoned.unwrap();
    assert_eq!(mark.reason, "not needed");
    assert_eq!(mark.state, "active");
    assert_eq!(mark.uid, 0);
    assert!(status.state.may_delete());
    assert!(matches!(
        active.finisher(&lock),
        Err(FinishError::Abandoned { .. })
    ));
    // Again: unchanged.
    let again = finish::abandon(&active.state, &lock, "twice", OPERATOR).unwrap();
    assert_eq!(again.mark.unwrap().abandoned.unwrap().reason, "not needed");
    drop(lock);

    // Finished and pending: abandoned between two attempts.
    let fork = Fork::new();
    fork.create(&source, "src-2").unwrap();
    fork.as_vol().write("b.txt", "b\n");
    let parked = source.dir.path().join("parked.git");
    fs::rename(source.remote.as_path(), &parked).unwrap();
    let lock = fork.lock();
    let mut finisher = fork.finisher(&lock).unwrap();
    let outcome = finisher
        .publish(Some(Instant::now() + Duration::from_millis(200)))
        .unwrap();
    assert!(
        matches!(outcome, PublishOutcome::Pending { .. }),
        "{outcome:?}"
    );
    let status = finish::abandon(&fork.state, &lock, "stuck", OPERATOR).unwrap();
    assert_eq!(status.mark.unwrap().abandoned.unwrap().state, "finished");
    fs::rename(parked, source.remote.as_path()).unwrap();
    let before = remote_refs(source.remote.as_path());
    assert!(matches!(
        finisher.publish(None),
        Err(FinishError::Abandoned { .. })
    ));
    assert!(matches!(
        finisher.finish(),
        Err(FinishError::Abandoned { .. })
    ));
    assert_eq!(
        remote_refs(source.remote.as_path()),
        before,
        "a push reached the remote"
    );
    drop(finisher);
    drop(lock);

    // Refused, then abandoned.
    let refused = Fork::new();
    refused.create(&source, "src-3").unwrap();
    refused.as_vol().write("c.txt", "c\n");
    let remote = Repository::open_bare(source.remote.as_path()).unwrap();
    let source_head = source.head().unwrap();
    push_source_branch(&source);
    remote
        .reference(&finish_ref("src-3"), source_head, true, "test")
        .unwrap();
    let lock = refused.lock();
    let outcome = refused.finisher(&lock).unwrap().publish(None).unwrap();
    assert!(
        matches!(outcome, PublishOutcome::Refused { .. }),
        "{outcome:?}"
    );
    let status = finish::abandon(&refused.state, &lock, "another writer", OPERATOR).unwrap();
    assert_eq!(status.state, ForkState::Abandoned);
    assert_eq!(status.mark.unwrap().abandoned.unwrap().state, "refused");
    drop(lock);

    // Published: nothing to discard.
    let done = Fork::new();
    done.create(&source, "src-4").unwrap();
    let lock = done.lock();
    assert_eq!(
        done.finisher(&lock).unwrap().publish(None).unwrap(),
        PublishOutcome::Published
    );
    assert!(matches!(
        finish::abandon(&done.state, &lock, "late", OPERATOR),
        Err(AbandonError::Published { .. })
    ));
    // A volume that is not a fork.
    let held = StateLock::acquire(&source.state, "test", Duration::ZERO).unwrap();
    assert!(matches!(
        finish::abandon(&source.state, &held, "x", OPERATOR),
        Err(AbandonError::NotAFork)
    ));
}

/// B5: a fork whose included profile file is missing fails to open; a
/// flagged fork remote with a global lfs.url and no endpoint of its own
/// is misconfigured, and finish has no fork remote.
#[test]
fn settings_that_cannot_be_trusted_fail_closed() {
    let source = Vol::new("src");
    source.change(&[("a.txt", "a\n")]);
    let forks = source.dir.path().join("forks.git");
    Repository::init_bare(&forks).unwrap();
    let base_profile = fs::read_to_string(source.fragment.as_path()).unwrap();
    let flagged = format!(
        "{base_profile}[remote \"forks\"]\n\turl = {}\n\tagentfsForkRemote = true\n",
        forks.display()
    );
    fs::write(source.fragment.as_path(), &flagged).unwrap();
    let fork = Fork::new();
    fork.create(&source, "src-1").unwrap();

    // A global LFS endpoint, and none of the fork remote's own.
    fs::write(
        source.fragment.as_path(),
        format!("{flagged}[lfs]\n\turl = http://lfs.example/source\n"),
    )
    .unwrap();
    let (events, seen) = recording();
    let lock = fork.lock();
    let mut finisher = fork.finisher_with(&lock, events).unwrap();
    let misconfigured: Vec<String> = seen
        .lock()
        .unwrap()
        .iter()
        .filter_map(|e| match e {
            Event::RemoteMisconfigured { remote, error } => Some(format!("{remote}: {error}")),
            _ => None,
        })
        .collect();
    assert_eq!(misconfigured.len(), 1, "{misconfigured:?}");
    assert!(
        misconfigured[0].starts_with("forks: ")
            && misconfigured[0].contains("must name its own LFS endpoint"),
        "{misconfigured:?}"
    );
    assert!(matches!(
        finisher.finish(),
        Err(FinishError::NoForkRemote { .. })
    ));
    drop(finisher);
    // The source is unaffected: its own remote is valid.
    let settings = crate::config::Settings::load(&source.state.git()).unwrap();
    assert_eq!(settings.remotes.pushed().len(), 1);
    assert_eq!(settings.remotes.pushed()[0].name, "origin");

    // The profile file gone: nothing opens, nothing reads a status.
    fs::remove_file(source.fragment.as_path()).unwrap();
    let opened = fork.finisher(&lock);
    let error = format!("{}", opened.err().unwrap());
    assert!(error.contains("cannot be read"), "{error}");
    let status = finish::status(&fork.state);
    assert!(status.is_err(), "{status:?}");
    let opened = volume::open(
        &fork.state,
        &fork.tree,
        context("fork"),
        Arc::new(NullSink),
        &Telemetry::noop(),
    );
    assert!(opened.is_err());
}

/// B6: a Recorder opened on a finished fork refuses every record.
#[test]
fn a_finished_fork_records_nothing() {
    let source = Vol::new("src");
    source.change(&[("a.txt", "a\n")]);
    let fork = Fork::new();
    fork.create(&source, "src-1").unwrap();
    fork.as_vol().write("b.txt", "b\n");
    volume::refuse_finished(&fork.state).unwrap();
    let lock = fork.lock();
    fork.finisher(&lock).unwrap().finish().unwrap();
    assert!(matches!(
        volume::refuse_finished(&fork.state),
        Err(volume::VolumeError::ForkFinished { fork }) if fork == "src-1"
    ));
    fs::write(fork.tree.join("c.txt"), "c\n").unwrap();
    let (recorder, _committer, _) = volume::open(
        &fork.state,
        &fork.tree,
        context("fork"),
        Arc::new(NullSink),
        &Telemetry::noop(),
    )
    .unwrap();
    let finished = |e: &volume::VolumeError| matches!(e, volume::VolumeError::ForkFinished { .. });
    let path = RepoPath::new("c.txt").unwrap();
    let input = || vec![volume::Input::Tree { path: path.clone() }];
    assert!(finished(
        &recorder
            .record(Trigger::Close, CALLER, input())
            .unwrap_err()
    ));
    assert!(finished(
        &recorder
            .record_group(Trigger::Close, CALLER, input())
            .unwrap_err()
    ));
    assert!(finished(
        &recorder
            .record_as(Trigger::Sweep, CALLER, input(), None)
            .unwrap_err()
    ));
    assert!(finished(
        &recorder
            .record_move(
                Trigger::Rename,
                CALLER,
                RepoPath::new("b.txt").unwrap(),
                RepoPath::new("d.txt").unwrap()
            )
            .unwrap_err()
    ));
    let op = recorder.begin_operation(
        OperationKind::Restore,
        "HEAD".into(),
        "restore".into(),
        CALLER,
        &[CALLER.pid],
    );
    assert!(finished(&op.err().unwrap()));
    // An ordinary volume is not refused.
    volume::refuse_finished(&source.state).unwrap();
}

/// A8: a forge that moved the fork's branch does not block publishing once
/// the head is saved as diverged.
#[test]
fn a_moved_fork_branch_is_divergence_not_a_block() {
    let source = Vol::new("src");
    source.change(&[("a.txt", "a\n")]);
    let fork = Fork::new();
    fork.create(&source, "src-1").unwrap();
    // Someone else's commit on the fork's branch on the remote.
    let remote = Repository::open_bare(source.remote.as_path()).unwrap();
    let sig = git2::Signature::now("forge", "forge@example.com").unwrap();
    let tree = remote.treebuilder(None).unwrap().write().unwrap();
    let tree = remote.find_tree(tree).unwrap();
    let foreign = remote
        .commit(None, &sig, &sig, "forge", &tree, &[])
        .unwrap();
    remote
        .reference("refs/heads/agentfs/src-1", foreign, true, "test")
        .unwrap();
    fork.as_vol().write("b.txt", "b\n");
    let lock = fork.lock();
    let mut finisher = fork.finisher(&lock).unwrap();
    let outcome = finisher
        .publish(Some(Instant::now() + Duration::from_secs(5)))
        .unwrap();
    assert_eq!(outcome, PublishOutcome::Published);
    let record = finisher.finish().unwrap();
    assert_eq!(
        remote.refname_to_id(&finish_ref("src-1")).unwrap(),
        record.finish
    );
    let diverged: Vec<String> = remote
        .references_glob("refs/agentfs/src-1/diverged/*")
        .unwrap()
        .map(|r| r.unwrap().name().unwrap().to_owned())
        .collect();
    assert_eq!(diverged.len(), 1, "{diverged:?}");
}

/// The finish record is read only from the finish commit's last
/// paragraph, every field checked.
#[test]
fn finish_records_parse_only_from_agentfs_trailers() {
    let base = "a".repeat(40);
    let change = "b".repeat(40);
    let finish = Oid::from_str(&"c".repeat(40)).unwrap();
    let good = format!(
        "finish fork f\n\nAgentfs-Seq: 3\nAgentfs-Trigger: finish\nAgentfs-Fork: f\n\
         Agentfs-Source: s\nAgentfs-Base: {base}\nAgentfs-Change: {change}\n\
         Agentfs-Fork-Remote: origin\nAgentfs-Unrecorded: 2\n"
    );
    let record = crate::history::finish_from_message(&good, finish).unwrap();
    assert_eq!(record.fork, "f");
    assert_eq!(record.change, Some(Oid::from_str(&change).unwrap()));
    assert_eq!(record.unrecorded, 2);
    for bad in [
        good.replace("Agentfs-Trigger: finish\n", ""),
        good.replace("Agentfs-Fork: f\n", "Agentfs-Fork: a/b\n"),
        good.replace(&base, "abc"),
        good.replace("Unrecorded: 2", "Unrecorded: -1"),
        good.replace("Agentfs-Fork-Remote: origin\n", ""),
        format!("{good}Agentfs-Fork: g\n"),
        // In an earlier paragraph.
        format!("{good}\nAgentfs-Seq: 4\n"),
    ] {
        assert_eq!(
            crate::history::finish_from_message(&bad, finish),
            None,
            "{bad}"
        );
    }
}

/// A local-only volume: its profile has no remote section at all.
fn local_only(name: &str) -> Vol {
    let vol = Vol::new(name);
    fs::write(&vol.fragment, "").unwrap();
    vol
}

/// F8 S3 (R4): a fork of a local-only source publishes into the source's
/// own history, through no remote: its branch and its change and finish
/// refs, never the source's branch or tree; a second publish changes
/// nothing.
#[test]
fn a_local_only_sources_fork_publishes_into_its_history() {
    let source = local_only("src");
    source.change(&[("a.txt", "a\n")]);
    let base = source.head().unwrap();
    let fork = Fork::new();
    let identity = fork.create(&source, "src-1").unwrap();
    let settings = crate::config::Settings::load(&fork.state.git()).unwrap();
    assert_eq!(
        settings.fork_remote,
        Some(crate::config::ForkRemote::SourceHistory)
    );
    assert_eq!(
        settings.source_state.as_deref(),
        Some(std::path::absolute(source.state.root()).unwrap().as_path())
    );
    fork.as_vol().write("b.txt", "b\n");
    let source_tree = fs::read_dir(&source.tree).unwrap().count();

    let lock = fork.lock();
    let mut finisher = fork.finisher(&lock).unwrap();
    let record = finisher.finish().unwrap();
    assert_eq!(record.remote, super::receive::SOURCE_HISTORY);
    assert_eq!(
        finisher.publish(Some(soon())).unwrap(),
        PublishOutcome::Published
    );
    let repo = source.repo();
    assert_eq!(
        repo.refname_to_id("refs/heads/agentfs/src-1").unwrap(),
        record.finish
    );
    let change = record.change.unwrap();
    assert_eq!(repo.refname_to_id(&change_ref("src-1")).unwrap(), change);
    assert_eq!(
        repo.refname_to_id(&finish_ref("src-1")).unwrap(),
        record.finish
    );
    let received = repo.find_commit(change).unwrap();
    assert_eq!(received.parent_ids().collect::<Vec<_>>(), [base]);
    assert!(received.tree().unwrap().get_name("b.txt").is_some());
    // Never applied: the source's branch and tree are as they were.
    assert_eq!(source.head(), Some(base));
    assert_eq!(fs::read_dir(&source.tree).unwrap().count(), source_tree);
    assert!(!source.tree.join("b.txt").exists());
    assert_eq!(
        finish::status(&fork.state).unwrap().unwrap().state,
        ForkState::Published
    );

    // Idempotent: received again, the same refs.
    let refs = source.snapshot();
    super::receive::receive_fork_change(&source.state, &identity, &fork.state).unwrap();
    assert_eq!(source.snapshot(), refs);
    assert_eq!(
        finisher.publish(Some(soon())).unwrap(),
        PublishOutcome::Published
    );
}

/// What a local-only source refuses or is not there for: a branch another
/// writer holds is refused, nothing written; a source in use stops the
/// publish (exit 75 from the CLI); a source gone is refused.
#[test]
fn receiving_refuses_another_writer_and_waits_for_a_busy_source() {
    let source = local_only("src");
    source.change(&[("a.txt", "a\n")]);
    let fork = Fork::new();
    let identity = fork.create(&source, "src-2").unwrap();
    fork.as_vol().write("b.txt", "b\n");
    let lock = fork.lock();
    let mut finisher = fork.finisher(&lock).unwrap();
    finisher.finish().unwrap();

    let held = StateLock::acquire(&source.state, "test", Duration::ZERO).unwrap();
    assert!(matches!(
        finisher.publish(Some(soon())),
        Err(FinishError::SourceBusy(_))
    ));
    drop(held);

    // The fork's branch name taken in the source, at a commit off the
    // fork's history.
    let repo = source.repo();
    let other = source.head().unwrap();
    let tree = repo.find_commit(other).unwrap().tree().unwrap();
    let sig = git2::Signature::now("x", "x@e").unwrap();
    let stray = repo
        .commit(None, &sig, &sig, "another writer", &tree, &[])
        .unwrap();
    repo.reference("refs/heads/agentfs/src-2", stray, false, "test")
        .unwrap();
    let before = source.snapshot();
    assert!(matches!(
        super::receive::receive_fork_change(&source.state, &identity, &fork.state),
        Err(super::receive::ReceiveError::Diverged { ref refname })
            if refname == "refs/heads/agentfs/src-2"
    ));
    assert_eq!(source.snapshot(), before, "nothing is written");
    assert!(matches!(
        finisher.publish(Some(soon())).unwrap(),
        PublishOutcome::Refused {
            reason: RefusedReason::AnotherWriter,
            ..
        }
    ));

    // Gone: refused, never retried by the release scan.
    finisher.retry_refused().unwrap();
    fs::remove_dir_all(source.state.root()).unwrap();
    assert!(matches!(
        finisher.publish(Some(soon())).unwrap(),
        PublishOutcome::Refused {
            reason: RefusedReason::RemoteGone,
            ..
        }
    ));
    assert!(!source.state.root().exists(), "nothing made where it was");
}

/// A fork not finished has nothing to receive.
#[test]
fn an_active_fork_is_not_received() {
    let source = local_only("src");
    source.change(&[("a.txt", "a\n")]);
    let fork = Fork::new();
    let identity = fork.create(&source, "src-3").unwrap();
    assert!(matches!(
        super::receive::receive_fork_change(&source.state, &identity, &fork.state),
        Err(super::receive::ReceiveError::NotPublished)
    ));
}

/// A local-only source with one commit, and a finished fork of it named
/// `name` that added `b.txt`.
fn finished_fork_of_local_only(name: &str) -> (Vol, Fork, ForkIdentity) {
    let source = local_only("src");
    source.change(&[("a.txt", "a\n")]);
    let fork = Fork::new();
    let identity = fork.create(&source, name).unwrap();
    fork.as_vol().write("b.txt", "b\n");
    let lock = fork.lock();
    fork.finisher(&lock).unwrap().finish().unwrap();
    (source, fork, identity)
}

/// The fork's output is untrusted (forked-volume gate answer Q3,
/// 2026-10-15: "potentially hostile"): a fork whose refs are the
/// source's own (its branch, or `refs/agentfs/<source>/`) is refused and
/// nothing is written, however its name or the source's branch came to
/// collide.
#[test]
fn a_fork_never_writes_the_sources_own_refs() {
    // Named as its source: its branch is the source's, its change and
    // finish refs in the source's namespace.
    let (source, fork, identity) = finished_fork_of_local_only("src");
    let head = source.head();
    let before = source.snapshot();
    let received = super::receive::receive_fork_change(&source.state, &identity, &fork.state);
    assert!(
        matches!(
            received,
            Err(super::receive::ReceiveError::SourcesRef { ref refname })
                if refname == "refs/heads/agentfs/src"
        ),
        "received into the source's own refs: {received:?}"
    );
    assert_eq!(source.head(), head, "the source's branch never moves");
    assert_eq!(source.snapshot(), before, "nothing is written");
    // Hidden, its history ref is in the source's namespace.
    let hidden = ForkIdentity {
        push_target: PushTarget::Hidden,
        ..identity.clone()
    };
    let forked = Repository::open_bare(fork.state.git()).unwrap();
    let finish = forked.refname_to_id(&identity.history_ref()).unwrap();
    forked
        .reference(&hidden.history_ref(), finish, false, "test")
        .unwrap();
    assert!(matches!(
        super::receive::receive_fork_change(&source.state, &hidden, &fork.state),
        Err(super::receive::ReceiveError::SourcesRef { ref refname })
            if refname == "refs/agentfs/src/history"
    ));
    assert_eq!(source.snapshot(), before, "nothing is written");
    // Through the finisher: refused for good, as `unsafe`.
    let lock = fork.lock();
    assert!(matches!(
        fork.finisher(&lock).unwrap().publish(Some(soon())).unwrap(),
        PublishOutcome::Refused {
            reason: RefusedReason::Unsafe,
            ..
        }
    ));
    assert_eq!(source.head(), head, "the source's branch never moves");

    // The source's branch renamed (later) to the fork's, at base: base is
    // on it, and the fork's history fast-forwards from it.
    let (source, fork, identity) = finished_fork_of_local_only("src-4");
    git2::Config::open(&source.state.git().join("config"))
        .unwrap()
        .set_str("agentfs.branch", "agentfs/src-4")
        .unwrap();
    source
        .repo()
        .reference(&identity.history_ref(), identity.base, false, "test")
        .unwrap();
    let before = source.snapshot();
    let received = super::receive::receive_fork_change(&source.state, &identity, &fork.state);
    assert!(
        matches!(
            received,
            Err(super::receive::ReceiveError::SourcesRef { ref refname })
                if refname == "refs/heads/agentfs/src-4"
        ),
        "received into the source's branch: {received:?}"
    );
    assert_eq!(
        source.head(),
        Some(identity.base),
        "the source's branch never moves"
    );
    assert_eq!(source.snapshot(), before, "nothing is written");
}

/// A fork's base must be on the source's branch: one the source never
/// had, or no longer has (its history rewritten, design §15), is refused
/// and nothing is written, so a fork never brings history the source
/// does not hold.
#[test]
fn a_fork_whose_base_is_not_on_the_sources_branch_is_refused() {
    // A base the source never had: the fork's own finish commit.
    let (source, fork, identity) = finished_fork_of_local_only("src-5");
    let finish = Repository::open_bare(fork.state.git())
        .unwrap()
        .refname_to_id(&identity.history_ref())
        .unwrap();
    let foreign = ForkIdentity {
        base: finish,
        ..identity.clone()
    };
    let before = source.snapshot();
    let received = super::receive::receive_fork_change(&source.state, &foreign, &fork.state);
    assert!(
        matches!(
            received,
            Err(super::receive::ReceiveError::BaseNotOnBranch { base }) if base == finish
        ),
        "received off a foreign base: {received:?}"
    );
    assert_eq!(source.snapshot(), before, "nothing is written");

    // The source's branch rewritten to a new root: base is still in its
    // object store, but no longer on its branch.
    let repo = source.repo();
    let branch = crate::config::Settings::load(&source.state.git())
        .unwrap()
        .branch_ref;
    let tree = repo.find_commit(identity.base).unwrap().tree().unwrap();
    let sig = git2::Signature::now("x", "x@e").unwrap();
    let root = repo
        .commit(None, &sig, &sig, "rewritten", &tree, &[])
        .unwrap();
    repo.reference(&branch, root, true, "test").unwrap();
    let before = source.snapshot();
    let received = super::receive::receive_fork_change(&source.state, &identity, &fork.state);
    assert!(
        matches!(
            received,
            Err(super::receive::ReceiveError::BaseNotOnBranch { base }) if base == identity.base
        ),
        "received off a rewritten base: {received:?}"
    );
    assert_eq!(source.snapshot(), before, "nothing is written");
    let lock = fork.lock();
    assert!(matches!(
        fork.finisher(&lock).unwrap().publish(Some(soon())).unwrap(),
        PublishOutcome::Refused {
            reason: RefusedReason::Unsafe,
            ..
        }
    ));
}

/// The fork's LFS objects are taken into the source's cache only as
/// verified: the bytes kept are the bytes hashed, under the name they
/// hash to. An object whose content is not its name is refused and
/// nothing of it is kept, under that name or any other.
#[test]
fn a_forks_lfs_objects_are_kept_only_as_verified() {
    use crate::lfs::cache::ObjectCache;
    use crate::lfs::pointer::Pointer;
    let plant = |fork: &Fork, name: &Pointer, content: &[u8]| {
        let path = ObjectCache::at(fork.state.lfs()).unwrap().path(&name.oid);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    };
    let source_objects = |source: &Vol| {
        let mut list: Vec<_> = ObjectCache::at(source.state.lfs())
            .unwrap()
            .list()
            .unwrap()
            .into_iter()
            .map(|(p, _)| p.oid.to_string())
            .collect();
        list.sort();
        list
    };

    // A whole object is copied, byte for byte.
    let (source, fork, identity) = finished_fork_of_local_only("src-6");
    let good = Pointer::of_reader(&b"real object\n"[..]).unwrap();
    plant(&fork, &good, b"real object\n");
    super::receive::receive_fork_change(&source.state, &identity, &fork.state).unwrap();
    let cache = ObjectCache::at(source.state.lfs()).unwrap();
    assert_eq!(fs::read(cache.path(&good.oid)).unwrap(), b"real object\n");

    // One whose content is not its name, of the name's size.
    let (source, fork, identity) = finished_fork_of_local_only("src-7");
    let named = Pointer::of_reader(&b"real object\n"[..]).unwrap();
    plant(&fork, &named, b"fake object\n");
    let objects = source_objects(&source);
    let refs = source.snapshot();
    let received = super::receive::receive_fork_change(&source.state, &identity, &fork.state);
    assert!(
        matches!(
            received,
            Err(super::receive::ReceiveError::LfsMismatch { .. })
        ),
        "kept an object that is not its name: {received:?}"
    );
    assert_eq!(source_objects(&source), objects, "nothing of it is kept");
    assert_eq!(source.snapshot(), refs, "no ref is written");
}

/// Every refusal a receive makes crosses the source's control socket as
/// itself (`receive_fork`), and nothing else is read as one.
#[test]
fn receive_refusals_round_trip_the_control_socket() {
    use super::receive::ReceiveError;
    let base = Oid::from_str("0123456789abcdef0123456789abcdef01234567").unwrap();
    for refused in [
        ReceiveError::SourceRemoved,
        ReceiveError::NotPublished,
        ReceiveError::Diverged {
            refname: "refs/heads/agentfs/f".into(),
        },
        ReceiveError::SourcesRef {
            refname: "refs/heads/agentfs/src".into(),
        },
        ReceiveError::BaseNotOnBranch { base },
        ReceiveError::LfsMismatch {
            message: "LFS object x arrived as y".into(),
        },
    ] {
        let (kind, message) = refused.refusal().unwrap();
        let back = ReceiveError::from_refusal(kind, message).unwrap();
        assert_eq!(back.to_string(), refused.to_string(), "{kind}");
        assert_eq!(back.refusal(), refused.refusal(), "{kind}");
    }
    assert!(
        ReceiveError::Io(std::io::Error::other("x"))
            .refusal()
            .is_none()
    );
    assert!(ReceiveError::from_refusal("base_not_on_branch", "abc".into()).is_none());
    assert!(ReceiveError::from_refusal("not_a_fork", "x".into()).is_none());
}

/// A deadline for a publish that should not wait.
fn soon() -> Instant {
    Instant::now() + Duration::from_secs(5)
}

/// Design §11.5 "Fork link" (owner, 2026-10-17: "A fork can be created
/// with or without a remote link back to its parent. Configurable."; on
/// where an unlinked fork pushes: "B. And if no remote is configured, the
/// volume is lost, and that's a choice the user is allowed to make."): an
/// unlinked fork pushes only to its own remotes, never to its source's,
/// and nothing is ever delivered to its source: no finish, no change or
/// finish ref, no fork status, no abandon. Its link is its identity's,
/// fixed at create.
#[test]
fn an_unlinked_fork_pushes_only_to_its_own_remotes_and_delivers_nothing() {
    let source = Vol::new("src");
    // The source's fork remote, which a linked fork would publish to.
    let forks = source.dir.path().join("forks.git");
    Repository::init_bare(&forks).unwrap();
    fs::write(
        &source.fragment,
        format!(
            "[remote \"origin\"]\n\turl = {}\n[remote \"forks\"]\n\turl = {}\n\
             \tagentfsForkRemote = true\n",
            source.remote.display(),
            forks.display()
        ),
    )
    .unwrap();
    source.change(&[("a.txt", "a\n")]);
    let own = source.dir.path().join("own.git");
    Repository::init_bare(&own).unwrap();
    let remotes = source.dir.path().join("fork-remotes.config");
    fs::write(
        &remotes,
        format!("[remote \"mine\"]\n\turl = {}\n", own.display()),
    )
    .unwrap();
    // Remotes of its own are an unlinked fork's.
    assert!(matches!(
        Fork::new().create_with_remotes(&source, "src-l", ForkLink::Linked, Some(&remotes)),
        Err(CreateError::RemotesNeedUnlinked)
    ));
    let fork = Fork::new();
    let identity = fork
        .create_with_remotes(&source, "src-u", ForkLink::Unlinked, Some(&remotes))
        .unwrap();
    assert_eq!(identity.link, ForkLink::Unlinked);
    // A second create asking linked keeps the identity it has.
    assert_eq!(
        fork.create(&source, "src-u").unwrap().link,
        ForkLink::Unlinked
    );
    let settings = crate::config::Settings::load(&fork.state.git()).unwrap();
    assert_eq!(settings.fork.as_ref(), Some(&identity));
    assert_eq!(settings.fork_remote, None);
    assert_eq!(settings.source_state, None);
    assert_eq!(settings.fork_remotes.as_deref(), Some(remotes.as_path()));
    fork.as_vol().write("b.txt", "b\n");

    // Pushed as any volume's branch, to its own remote only.
    let mut pushers = crate::push::open_all(
        &fork.state,
        &settings,
        Arc::new(NullSink),
        &Telemetry::noop(),
    )
    .unwrap();
    assert_eq!(pushers.len(), 1);
    assert!(pushers[0].push_once().is_success());
    let head = Repository::open_bare(fork.state.git())
        .unwrap()
        .refname_to_id("refs/heads/agentfs/src-u")
        .unwrap();
    let on_own = remote_refs(&own);
    assert_eq!(on_own.get("refs/heads/agentfs/src-u"), Some(&head));
    assert!(
        on_own.keys().all(|r| !r.starts_with("refs/agentfs/")),
        "nothing delivered: {on_own:?}"
    );
    for parents in [&source.remote, &forks] {
        let refs = remote_refs(parents);
        assert!(
            refs.keys().all(|r| !r.contains("src-u")),
            "nothing reaches the source's remotes: {refs:?}"
        );
    }

    // Without remotes of its own it is local-only, even beside the
    // source's remotes.
    let alone = Fork::new();
    alone
        .create_with(&source, "src-v", ForkLink::Unlinked)
        .unwrap();
    let settings = crate::config::Settings::load(&alone.state.git()).unwrap();
    assert_eq!(settings.remotes, crate::config::Remotes::LocalOnly);
    assert!(settings.invalid_remotes.is_empty());

    // Never finished, so never read-only, and no fork status.
    let lock = fork.lock();
    assert!(matches!(
        fork.finisher(&lock),
        Err(FinishError::Unlinked { ref fork }) if fork == "src-u"
    ));
    assert_eq!(finish::status(&fork.state).unwrap(), None);
    assert!(matches!(
        finish::abandon(&fork.state, &lock, "no", CALLER),
        Err(AbandonError::NotAFork)
    ));
    volume::refuse_finished(&fork.state).unwrap();
    let repo = Repository::open_bare(fork.state.git()).unwrap();
    assert!(repo.find_reference(&change_ref("src-u")).is_err());
    assert!(repo.find_reference(&finish_ref("src-u")).is_err());
}

/// An unlinked fork of a local-only source records no path to it and is
/// local-only itself: nothing it does reaches the source's state area. A
/// linked one beside it still records the path (F8 S3).
#[test]
fn an_unlinked_fork_of_a_local_only_source_records_no_path() {
    let source = local_only("src");
    source.change(&[("a.txt", "a\n")]);
    let fork = Fork::new();
    fork.create_with(&source, "src-u", ForkLink::Unlinked)
        .unwrap();
    let before = source.snapshot();
    let settings = crate::config::Settings::load(&fork.state.git()).unwrap();
    assert_eq!(settings.source_state, None);
    assert_eq!(settings.fork_remote, None);
    assert_eq!(settings.remotes, crate::config::Remotes::LocalOnly);
    let config = fs::read_to_string(fork.state.git().join("config")).unwrap();
    assert!(!config.contains("sourceState"), "{config}");
    assert!(config.contains("link = unlinked"), "{config}");
    fork.as_vol().write("b.txt", "b\n");
    let lock = fork.lock();
    assert!(matches!(
        fork.finisher(&lock),
        Err(FinishError::Unlinked { .. })
    ));
    assert_eq!(source.snapshot(), before, "nothing reaches the source");

    let linked = Fork::new();
    linked.create(&source, "src-l").unwrap();
    let settings = crate::config::Settings::load(&linked.state.git()).unwrap();
    assert!(settings.source_state.is_some());
    assert_eq!(
        settings.fork_remote,
        Some(crate::config::ForkRemote::SourceHistory)
    );
}
