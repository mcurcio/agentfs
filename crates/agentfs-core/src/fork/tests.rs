//! Forked volumes against real state areas and bare remotes (design
//! §11.5): create, finish, publish and status, no FUSE.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use git2::{Oid, Repository};

use super::create::{self, CreateError, CreateRequest};
use super::finish::{self, FinishError, ForkState, Finisher, PublishOutcome, StatusError};
use super::identity::{ForkIdentity, PushTarget, change_ref, finish_ref};
use crate::confined::Dir;
use crate::events::NullSink;
use crate::identity::Context;
use crate::lock::StateLock;
use crate::paths::RepoPath;
use crate::record::Caller;
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
                Some(target) => std::os::unix::fs::symlink(target, &full).unwrap(),
                None => fs::write(&full, content).unwrap(),
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
                r.target().map(|o| o.to_string()).unwrap_or_default().into_bytes(),
            );
        }
        let mut dirs = vec![self.state.root().to_path_buf()];
        while let Some(dir) = dirs.pop() {
            for entry in fs::read_dir(&dir).unwrap() {
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
                    out.insert(path.display().to_string(), fs::read(&path).unwrap());
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
        create::create(&CreateRequest {
            source: &source.state,
            fork: &self.state,
            tree: &self.tree,
            name,
            includes: std::slice::from_ref(&source.fragment),
            push_target: PushTarget::Branch,
        })
    }

    fn finisher(&self, lock: &StateLock) -> Result<Finisher, FinishError> {
        Finisher::open(
            &self.state,
            &self.tree,
            context("fork"),
            Arc::new(NullSink),
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
        fs::write(&full, content).unwrap();
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
    let mode = fs::metadata(fork.tree.join("bin/run")).unwrap().permissions().mode();
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
    assert_eq!(fs::read_dir(&outside).unwrap().count(), 0, "wrote outside");

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
    let mut two = fs::read_to_string(&source.fragment).unwrap();
    two.push_str("[remote \"other\"]\n\turl = /nowhere\n");
    fs::write(&source.fragment, two).unwrap();
    let third = Fork::new();
    assert!(matches!(
        third.create(&source, "src-2"),
        Err(CreateError::NoForkRemote { .. })
    ));
    assert!(!third.state.root().exists());

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
        [source.fragment.clone()]
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
    assert_eq!(repo.refname_to_id(&change_ref("src-1")).unwrap(), change.id());
    assert_eq!(repo.refname_to_id(&finish_ref("src-1")).unwrap(), head);
    // Once: a second finish is the same record, with no new commit.
    assert_eq!(finisher.finish().unwrap(), record);
    assert_eq!(repo.refname_to_id("refs/heads/agentfs/src-1").unwrap(), head);
    assert_eq!(
        finish::status(&fork.state).unwrap().unwrap().state,
        ForkState::Finished
    );

    // Published only once the remote holds both refs.
    assert_eq!(finisher.publish(None).unwrap(), PublishOutcome::Published);
    let remote = Repository::open_bare(&source.remote).unwrap();
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
    fs::write(&mark, "{not json").unwrap();
    assert!(matches!(
        finish::status(&fork.state),
        Err(StatusError::Mark(_))
    ));
    // A non-fork has no fork status.
    assert_eq!(finish::status(&source.state).unwrap(), None);
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
    let mut two = fs::read_to_string(&source.fragment).unwrap();
    two.push_str("[remote \"other\"]\n\turl = /nowhere\n");
    fs::write(&source.fragment, two).unwrap();
    let repo = Repository::open_bare(fork.state.git()).unwrap();
    let head = repo.refname_to_id("refs/heads/agentfs/src-1").unwrap();
    let lock = fork.lock();
    let result = fork.finisher(&lock).unwrap().finish();
    assert!(
        matches!(result, Err(FinishError::NoForkRemote { .. })),
        "{result:?}"
    );
    assert_eq!(repo.refname_to_id("refs/heads/agentfs/src-1").unwrap(), head);
    assert!(repo.find_reference(&finish_ref("src-1")).is_err());
    assert_eq!(
        finish::status(&fork.state).unwrap().unwrap().state,
        ForkState::Active
    );
}

/// A8: an unreachable remote, or a finish ref someone else set, never
/// yields Published or a mark.
#[test]
fn publish_waits_for_the_remote_and_never_moves_its_refs() {
    let source = Vol::new("src");
    source.change(&[("a.txt", "a\n")]);
    let fork = Fork::new();
    fork.create(&source, "src-1").unwrap();
    fork.as_vol().write("b.txt", "b\n");
    // Unreachable.
    let parked = source.dir.path().join("parked.git");
    fs::rename(&source.remote, &parked).unwrap();
    let lock = fork.lock();
    let mut finisher = fork.finisher(&lock).unwrap();
    let outcome = finisher
        .publish(Some(Instant::now() + Duration::from_millis(300)))
        .unwrap();
    assert!(
        matches!(outcome, PublishOutcome::Pending { .. }),
        "{outcome:?}"
    );
    assert!(!fork.state.fork_mark().exists());
    // Back, with the finish ref set by another writer.
    fs::rename(&parked, &source.remote).unwrap();
    let remote = Repository::open_bare(&source.remote).unwrap();
    let source_head = source.head().unwrap();
    let local = source.repo();
    // The remote needs the commit it names: push the source's branch.
    let mut push = local.remote_anonymous(&source.remote.display().to_string()).unwrap();
    push.push(&[format!("{source_head}:refs/heads/agentfs/src")], None)
        .unwrap();
    remote
        .reference(&finish_ref("src-1"), source_head, true, "test")
        .unwrap();
    let outcome = finisher
        .publish(Some(Instant::now() + Duration::from_millis(300)))
        .unwrap();
    assert!(
        matches!(outcome, PublishOutcome::Pending { .. }),
        "{outcome:?}"
    );
    assert_eq!(
        remote.refname_to_id(&finish_ref("src-1")).unwrap(),
        source_head
    );
    assert!(!fork.state.fork_mark().exists());
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
    let remote = Repository::open_bare(&source.remote).unwrap();
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
