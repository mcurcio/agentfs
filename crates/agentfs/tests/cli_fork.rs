//! End-to-end tests of `agentfs fork` (design §11.5, §17.1) against
//! `file://` bare remotes, no FUSE: init, record, fork create, record in
//! the fork, fork finish, then the change submitted twice with
//! `agentfs patch submit` (which drives the real `git`) and applied once.
//!
//! The git-driven parts are skipped, with a message, when `git` is not
//! installed. The CI `test` job runs them against the runner's git.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

use agentfs_core::lock::StateLock;
use agentfs_core::volume::StateDir;
use git2::{Oid, Repository};

fn have_git() -> bool {
    let found = Command::new("git").arg("--version").output().is_ok();
    if !found {
        eprintln!("skipping: git is not installed");
    }
    found
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// A source volume `src` whose settings come from a profile fragment, a
/// bare `origin`, and, when `fork_remote`, a bare `forks` remote with
/// `agentfsForkRemote = true`.
struct Setup {
    dir: tempfile::TempDir,
}

impl Setup {
    fn new(fork_remote: bool) -> Self {
        let s = Self {
            dir: tempfile::tempdir().unwrap(),
        };
        fs::create_dir_all(s.path("tree")).unwrap();
        fs::create_dir_all(s.path("home")).unwrap();
        Repository::init_bare(s.path("origin.git")).unwrap();
        Repository::init_bare(s.path("forks.git")).unwrap();
        let mut profile = format!(
            "[remote \"origin\"]\n\turl = {}\n",
            s.url("origin.git")
        );
        if fork_remote {
            profile.push_str(&format!(
                "[remote \"forks\"]\n\turl = {}\n\tagentfsForkRemote = true\n",
                s.url("forks.git")
            ));
        }
        fs::write(s.path("profile"), profile).unwrap();
        let profile = s.path("profile").display().to_string();
        s.ok("state", "tree", &["init", "--volume", "src", "--include", &profile]);
        s
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn url(&self, name: &str) -> String {
        format!("file://{}", self.path(name).display())
    }

    fn hermetic(&self, cmd: &mut Command) {
        let home = self.path("home");
        cmd.env("HOME", &home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "Producer")
            .env("GIT_AUTHOR_EMAIL", "producer@example.com")
            .env("GIT_COMMITTER_NAME", "Producer")
            .env("GIT_COMMITTER_EMAIL", "producer@example.com")
            .env_remove("AGENTFS_STATE")
            .env_remove("AGENTFS_ROOT")
            .env_remove("AGENTFS_VOLUME")
            .env("AGENTFS_REPLICA", "test-0");
        for (name, _) in std::env::vars_os() {
            if name.to_string_lossy().starts_with("OTEL_") {
                cmd.env_remove(name);
            }
        }
    }

    /// `agentfs --state <state> --root <root> args`.
    fn agentfs(&self, state: &str, root: &str, args: &[&str]) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_agentfs"));
        cmd.arg("--state")
            .arg(self.path(state))
            .arg("--root")
            .arg(self.path(root))
            .args(args);
        self.hermetic(&mut cmd);
        cmd.output().unwrap()
    }

    fn ok(&self, state: &str, root: &str, args: &[&str]) -> String {
        let out = self.agentfs(state, root, args);
        assert!(
            out.status.success(),
            "agentfs {args:?}: {}{}",
            text(&out.stdout),
            text(&out.stderr)
        );
        text(&out.stdout)
    }

    fn record(&self, state: &str, root: &str, name: &str, content: &str) {
        fs::write(self.path(root).join(name), content).unwrap();
        self.ok(state, root, &["record", name]);
    }

    /// `fork create` of the source into `fork` (state) and `fork-tree`;
    /// returns the fork's name.
    fn create_fork(&self) -> String {
        let source = self.path("state").display().to_string();
        let out = self.ok(
            "fork",
            "fork-tree",
            &["fork", "create", "--source-state", &source],
        );
        let line = out.lines().next().unwrap().to_owned();
        let name = line
            .strip_prefix("fork ")
            .and_then(|l| l.split(' ').next())
            .unwrap_or_else(|| panic!("{line}"))
            .to_owned();
        let head = self.source_head();
        assert_eq!(line, format!("fork {name} of src at {head}"));
        name
    }

    fn source_head(&self) -> Oid {
        Repository::open_bare(self.path("state/git"))
            .unwrap()
            .refname_to_id("refs/heads/agentfs/src")
            .unwrap()
    }

    fn refs(&self, remote: &str) -> Vec<(String, Oid)> {
        let repo = Repository::open_bare(self.path(remote)).unwrap();
        let mut out: Vec<(String, Oid)> = repo
            .references()
            .unwrap()
            .map(|r| {
                let r = r.unwrap();
                (r.name().unwrap().to_owned(), r.target().unwrap())
            })
            .collect();
        out.sort();
        out
    }

    fn remote_ref(&self, remote: &str, name: &str) -> Option<Oid> {
        Repository::open_bare(self.path(remote))
            .unwrap()
            .refname_to_id(name)
            .ok()
    }

    fn git(&self, dir: &Path, args: &[&str]) -> Output {
        let mut cmd = Command::new("git");
        cmd.arg("-C").arg(dir).args(args);
        self.hermetic(&mut cmd);
        cmd.output().unwrap()
    }

    fn submit_command(&self, producer: &Path, commit: &str) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_agentfs"));
        cmd.args(["patch", "submit", commit, "--volume", "src", "--repo"])
            .arg(producer)
            .args(["--attempts", "20"]);
        self.hermetic(&mut cmd);
        cmd
    }
}

/// A1, A11: a flagged fork remote is never the source's, and a fork
/// publishes there.
#[test]
fn a_source_never_pushes_to_its_fork_remote() {
    let s = Setup::new(true);
    s.record("state", "tree", "a.txt", "a\n");
    let out = s.ok("state", "tree", &["push"]);
    assert!(!out.contains("forks"), "{out}");
    assert!(s.refs("forks.git").is_empty(), "{:?}", s.refs("forks.git"));
    assert_eq!(
        s.remote_ref("origin.git", "refs/heads/agentfs/src"),
        Some(s.source_head())
    );

    let fork = s.create_fork();
    assert!(fork.starts_with("src-"), "{fork}");
    // The fork's tree is base's.
    assert_eq!(fs::read_to_string(s.path("fork-tree/a.txt")).unwrap(), "a\n");
    s.record("fork", "fork-tree", "b.txt", "b\n");
    let out = s.ok("fork", "fork-tree", &["fork", "finish"]);
    assert!(out.contains("published to forks"), "{out}");
    let branch = format!("refs/heads/agentfs/{fork}");
    let finish = s.remote_ref("forks.git", &format!("refs/agentfs/{fork}/finish"));
    assert!(finish.is_some());
    assert_eq!(s.remote_ref("forks.git", &branch), finish);
    assert!(
        s.remote_ref("forks.git", &format!("refs/agentfs/{fork}/change"))
            .is_some()
    );
    // Nothing of the fork reached the source's remote.
    assert!(s.remote_ref("origin.git", &branch).is_none());
    // A second create in the same state area is the same fork.
    let again = s.create_fork();
    assert_eq!(again, fork);
}

/// A11: exit statuses of `fork finish`: 75 while the state area is held,
/// 1 at --timeout with the remote unreachable, 0 once published, with the
/// same record each time. A10: the change, submitted twice at once,
/// applies once.
#[test]
fn finish_publishes_to_the_patch_remote_and_the_change_applies_once() {
    let s = Setup::new(false);
    s.record("state", "tree", "a.txt", "a\n");
    s.ok("state", "tree", &["push"]);
    let fork = s.create_fork();
    s.record("fork", "fork-tree", "b.txt", "from the fork\n");

    {
        let held =
            StateLock::acquire(&StateDir::new(s.path("fork")), "test", Duration::ZERO).unwrap();
        let out = s.agentfs("fork", "fork-tree", &["fork", "finish"]);
        assert_eq!(out.status.code(), Some(75), "{}", text(&out.stderr));
        drop(held);
    }

    let parked = s.path("parked.git");
    fs::rename(s.path("origin.git"), &parked).unwrap();
    let out = s.agentfs("fork", "fork-tree", &["fork", "finish", "--timeout", "1s"]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    let first = text(&out.stdout).lines().next().unwrap().to_owned();
    assert!(first.starts_with(&format!("finished fork {fork} of src")), "{first}");
    fs::rename(&parked, s.path("origin.git")).unwrap();
    let out = s.ok("fork", "fork-tree", &["fork", "finish"]);
    assert_eq!(out.lines().next().unwrap(), first);
    assert!(out.contains("published to origin"), "{out}");
    let change = s
        .remote_ref("origin.git", &format!("refs/agentfs/{fork}/change"))
        .unwrap();
    // The source's branch is not moved by the fork.
    assert_eq!(
        s.remote_ref("origin.git", "refs/heads/agentfs/src"),
        Some(s.source_head())
    );

    if !have_git() {
        return;
    }
    // A receiver fetches the change and submits it, twice at once.
    let producer = s.path("producer");
    let mut clone = Command::new("git");
    clone
        .args(["clone", "--quiet", &s.url("origin.git")])
        .arg(&producer);
    s.hermetic(&mut clone);
    let out = clone.output().unwrap();
    assert!(out.status.success(), "{}", text(&out.stderr));
    let spec = format!("refs/agentfs/{fork}/change:refs/heads/change");
    let out = s.git(&producer, &["fetch", "--quiet", "origin", &spec]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let oid = change.to_string();
    let handles: Vec<_> = (0..2)
        .map(|_| {
            let mut cmd = s.submit_command(&producer, &oid);
            std::thread::spawn(move || cmd.output().unwrap())
        })
        .collect();
    for h in handles {
        let out = h.join().unwrap();
        assert!(out.status.success(), "{}", text(&out.stderr));
    }
    let out = s.ok("state", "tree", &["patch", "apply"]);
    let _ = out;
    assert_eq!(
        fs::read_to_string(s.path("tree/b.txt")).unwrap(),
        "from the fork\n"
    );
    // One commit applies it; the other is a duplicate marker.
    let repo = Repository::open_bare(s.path("state/git")).unwrap();
    let mut walk = repo.revwalk().unwrap();
    walk.push(s.source_head()).unwrap();
    walk.simplify_first_parent().unwrap();
    let patch_line = format!("Agentfs-Patch: {oid}\n");
    let messages: Vec<String> = walk
        .map(|o| {
            repo.find_commit(o.unwrap())
                .unwrap()
                .message()
                .unwrap()
                .to_owned()
        })
        .filter(|m| m.contains(&patch_line))
        .collect();
    assert_eq!(messages.len(), 2, "{messages:?}");
    assert_eq!(
        messages
            .iter()
            .filter(|m| m.starts_with("duplicate of patch "))
            .count(),
        1,
        "{messages:?}"
    );
}

/// A5: a CLI source whose remotes live in its own config is refused.
#[test]
fn a_source_with_settings_in_its_own_config_is_refused() {
    let s = Setup::new(false);
    s.record("state", "tree", "a.txt", "a\n");
    let mut config = git2::Config::open(&s.path("state/git/config")).unwrap();
    config.set_str("remote.own.url", "/nowhere").unwrap();
    let source = s.path("state").display().to_string();
    let out = s.agentfs(
        "fork",
        "fork-tree",
        &["fork", "create", "--source-state", &source],
    );
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stdout));
    assert!(text(&out.stderr).contains("remote.own.url"), "{}", text(&out.stderr));
}
