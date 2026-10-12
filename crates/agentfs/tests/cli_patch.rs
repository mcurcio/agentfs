//! End-to-end tests of `agentfs patch` and the queue lines of `agentfs
//! status` and `agentfs push` (design §12, §18 test 15), against a
//! `file://` bare remote and the real `git`, which `patch submit` drives.
//!
//! Skipped, with a message, when `git` is not installed. The CI `test` job
//! runs them against the runner's git.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use git2::{Oid, Repository};

const BRANCH: &str = "refs/heads/agentfs/demo";

fn have_git() -> bool {
    let found = Command::new("git").arg("--version").output().is_ok();
    if !found {
        eprintln!("skipping: git is not installed");
    }
    found
}

/// A volume `demo` with a bare remote `origin`, and producer clones.
struct Setup {
    dir: tempfile::TempDir,
}

impl Setup {
    fn new() -> Self {
        let s = Self {
            dir: tempfile::tempdir().unwrap(),
        };
        fs::create_dir_all(s.tree()).unwrap();
        fs::create_dir_all(s.home()).unwrap();
        Repository::init_bare(s.remote()).unwrap();
        let out = s.agentfs(&["init", "--volume", "demo"]);
        assert!(out.status.success(), "{}", text(&out.stderr));
        let mut config = git2::Config::open(&s.state().join("git/config")).unwrap();
        config
            .set_str("remote.origin.url", &s.remote_url())
            .unwrap();
        s
    }

    fn state(&self) -> PathBuf {
        self.dir.path().join("state")
    }

    fn tree(&self) -> PathBuf {
        self.dir.path().join("tree")
    }

    fn home(&self) -> PathBuf {
        self.dir.path().join("home")
    }

    fn remote(&self) -> PathBuf {
        self.dir.path().join("remote.git")
    }

    fn remote_url(&self) -> String {
        format!("file://{}", self.remote().display())
    }

    fn hermetic(&self, cmd: &mut Command) {
        let home = self.home();
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

    fn agentfs_command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_agentfs"));
        cmd.arg("--state")
            .arg(self.state())
            .arg("--root")
            .arg(self.tree())
            .args(args);
        self.hermetic(&mut cmd);
        cmd
    }

    fn agentfs(&self, args: &[&str]) -> Output {
        self.agentfs_command(args).output().unwrap()
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.agentfs(args);
        assert!(
            out.status.success(),
            "agentfs {args:?}: {}{}",
            text(&out.stdout),
            text(&out.stderr)
        );
        text(&out.stdout)
    }

    fn record(&self, name: &str, content: &str) {
        fs::write(self.tree().join(name), content).unwrap();
        self.ok(&["record", name]);
    }

    fn git(&self, dir: &Path, args: &[&str]) -> Output {
        let mut cmd = Command::new("git");
        cmd.arg("-C").arg(dir).args(args);
        self.hermetic(&mut cmd);
        cmd.output().unwrap()
    }

    /// A clone of the remote: a producer's repository.
    fn producer(&self, name: &str) -> PathBuf {
        let dir = self.dir.path().join(name);
        let mut cmd = Command::new("git");
        cmd.args(["clone", "--quiet", &self.remote_url()]).arg(&dir);
        self.hermetic(&mut cmd);
        let out = cmd.output().unwrap();
        assert!(out.status.success(), "{}", text(&out.stderr));
        dir
    }

    /// `agentfs patch submit` in `producer`, without --state or --root.
    fn submit_command(&self, producer: &Path, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_agentfs"));
        cmd.args(["patch", "submit", "--volume", "demo", "--repo"])
            .arg(producer)
            .args(args);
        self.hermetic(&mut cmd);
        cmd
    }

    fn submit(&self, producer: &Path, args: &[&str]) -> Output {
        self.submit_command(producer, args).output().unwrap()
    }

    fn submit_ok(&self, producer: &Path, args: &[&str]) -> String {
        let out = self.submit(producer, args);
        assert!(
            out.status.success(),
            "submit {args:?}: {}{}",
            text(&out.stdout),
            text(&out.stderr)
        );
        text(&out.stdout)
    }

    fn file(&self, name: &str, content: &str) -> String {
        let path = self.dir.path().join(name);
        fs::write(&path, content).unwrap();
        path.display().to_string()
    }

    fn remote_ref(&self, name: &str) -> Option<Oid> {
        Repository::open_bare(self.remote())
            .unwrap()
            .refname_to_id(name)
            .ok()
    }

    fn local(&self) -> Repository {
        Repository::open_bare(self.state().join("git")).unwrap()
    }

    /// Commits on the local branch's first-parent history that apply
    /// patch `n`.
    fn inbox_commits(&self, n: u64) -> usize {
        let repo = self.local();
        let mut walk = repo.revwalk().unwrap();
        walk.push(repo.refname_to_id(BRANCH).unwrap()).unwrap();
        walk.simplify_first_parent().unwrap();
        let line = format!("Agentfs-Inbox: {n}\n");
        walk.filter(|oid| {
            repo.find_commit(*oid.as_ref().unwrap())
                .unwrap()
                .message()
                .unwrap()
                .contains(&line)
        })
        .count()
    }
}

fn inbox(n: u64) -> String {
    format!("refs/agentfs/demo/inbox/{n:010}")
}

fn applied(n: u64) -> String {
    format!("refs/agentfs/demo/applied/{n:010}")
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[test]
fn concurrent_submitters_get_distinct_contiguous_numbers() {
    if !have_git() {
        return;
    }
    let s = Setup::new();
    s.record("a.txt", "a\n");
    s.ok(&["push"]);
    let producers: Vec<PathBuf> = (1..=8).map(|i| s.producer(&format!("p{i}"))).collect();
    let handles: Vec<_> = producers
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let file = s.file(&format!("g{i}"), &format!("again {i}\n"));
            let put = format!("g{i}.txt={file}");
            let mut cmd = s.submit_command(p, &["--put", &put, "--attempts", "20"]);
            std::thread::spawn(move || cmd.output().unwrap())
        })
        .collect();
    for h in handles {
        let out = h.join().unwrap();
        assert!(out.status.success(), "{}", text(&out.stderr));
    }
    // 1..8, all distinct.
    let mut oids = std::collections::BTreeSet::new();
    for n in 1..=8 {
        let oid = s
            .remote_ref(&inbox(n))
            .unwrap_or_else(|| panic!("no inbox {n}"));
        assert!(oids.insert(oid));
    }
    assert_eq!(s.remote_ref(&inbox(9)), None);
}

#[test]
fn submit_builds_replaces_skips_and_refuses() {
    if !have_git() {
        return;
    }
    let s = Setup::new();
    s.record("a.txt", "a\n");
    s.record("gone.txt", "bye\n");
    s.ok(&["push"]);
    let head = s.remote_ref(BRANCH).unwrap();
    let p = s.producer("p");
    let file = s.file("new", "new content\n");
    let out = s.submit_ok(
        &p,
        &[
            "--put",
            &format!("dir/new.txt={file}"),
            "--delete",
            "gone.txt",
            "-m",
            "add new, drop gone",
            "--author",
            "Ada Lovelace <ada@example.com>",
        ],
    );
    assert!(
        out.contains("queued patch 1 as refs/agentfs/demo/inbox/0000000001"),
        "{out}"
    );
    let remote = Repository::open_bare(s.remote()).unwrap();
    let c = remote
        .find_commit(s.remote_ref(&inbox(1)).unwrap())
        .unwrap();
    assert_eq!(c.parent_id(0).unwrap(), head);
    assert_eq!(c.message(), Ok("add new, drop gone\n"));
    assert_eq!(c.author().name(), Ok("Ada Lovelace"));
    assert_eq!(c.author().email(), Ok("ada@example.com"));
    let tree = c.tree().unwrap();
    assert!(tree.get_path(Path::new("gone.txt")).is_err());
    assert!(tree.get_path(Path::new("a.txt")).is_ok());
    let blob = remote
        .find_blob(tree.get_path(Path::new("dir/new.txt")).unwrap().id())
        .unwrap();
    assert_eq!(blob.content(), b"new content\n");

    // A commit made in the producer, replacing 1.
    let out = s.git(&p, &["checkout", "--quiet", "agentfs/demo"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let file_path = p.join("a.txt");
    fs::write(&file_path, "edited\n").unwrap();
    let out = s.git(&p, &["commit", "--quiet", "-am", "edit a"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let out = s.submit_ok(&p, &["HEAD", "--replace", "1"]);
    assert!(out.contains("queued patch 1"), "{out}");
    let replaced = s.remote_ref(&inbox(1)).unwrap();
    assert_ne!(replaced, c.id());
    assert_eq!(
        remote.find_commit(replaced).unwrap().summary(),
        Ok(Some("edit a"))
    );
    // Not queued: nothing to replace.
    let out = s.submit(&p, &["HEAD", "--replace", "7"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        text(&out.stderr).contains("not queued"),
        "{}",
        text(&out.stderr)
    );

    // Skip: an empty commit on the head.
    s.submit_ok(&p, &["--skip", "1"]);
    let skip = remote
        .find_commit(s.remote_ref(&inbox(1)).unwrap())
        .unwrap();
    assert_eq!(skip.parent_id(0).unwrap(), head);
    assert_eq!(skip.tree_id(), remote.find_commit(head).unwrap().tree_id());

    // A parent the branch does not have.
    let out = s.git(&p, &["checkout", "--quiet", "--orphan", "elsewhere"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let out = s.git(&p, &["commit", "--quiet", "-m", "root"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    fs::write(p.join("x.txt"), "x\n").unwrap();
    s.git(&p, &["add", "x.txt"]);
    let out = s.git(&p, &["commit", "--quiet", "-m", "on a foreign root"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let out = s.submit(&p, &["HEAD"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        text(&out.stderr).contains("is not on the volume's branch"),
        "{}",
        text(&out.stderr)
    );
    assert_eq!(s.remote_ref(&inbox(2)), None);
}

/// Design §18 test 15 at the CLI.
#[test]
fn three_patches_the_second_conflicts() {
    if !have_git() {
        return;
    }
    let s = Setup::new();
    s.record("a.txt", "a0\n");
    s.record("b.txt", "b0\n");
    s.ok(&["push"]);
    let p = s.producer("p");
    let a1 = s.file("a1", "a1\n");
    let b1 = s.file("b1", "b-from-iac\n");
    let c1 = s.file("c1", "c\n");
    s.submit_ok(&p, &["--put", &format!("a.txt={a1}"), "-m", "one"]);
    s.submit_ok(&p, &["--put", &format!("b.txt={b1}"), "-m", "two"]);
    s.submit_ok(&p, &["--put", &format!("c.txt={c1}"), "-m", "three"]);
    // The workload changes b.txt meanwhile.
    s.record("b.txt", "b-from-workload\n");

    let out = s.agentfs(&["patch", "apply"]);
    let err = text(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(err.contains("patch 1 applied as "), "{err}");
    assert!(err.contains("patches: blocked at 2 (conflict)"), "{err}");
    assert!(err.contains("alarm: "), "{err}");
    assert_eq!(fs::read_to_string(s.tree().join("a.txt")).unwrap(), "a1\n");
    assert_eq!(
        fs::read_to_string(s.tree().join("b.txt")).unwrap(),
        "b-from-workload\n"
    );
    assert!(!s.tree().join("c.txt").exists());
    assert_eq!(s.inbox_commits(1), 1);

    let status = s.ok(&["status"]);
    assert!(
        status
            .contains("patches:   last applied 1, 2 queued, blocked at 2 (conflict) (from origin)"),
        "{status}"
    );
    let json = s.ok(&["patch", "status", "--json"]);
    let doc: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(doc["status"]["blocked"]["reason"], "conflict");
    assert_eq!(doc["status"]["blocked"]["paths"][0], "b.txt");
    assert_eq!(doc["cache"]["queue_depth"], 2);
    let human = s.ok(&["patch", "status"]);
    assert!(human.contains("blocked:   2 (conflict)"), "{human}");

    // The push carries the branch, then the queue.
    let out = s.ok(&["push"]);
    assert!(
        out.contains(
            "origin: queue: status pushed, 1 applied ref(s) created, 1 inbox ref(s) deleted"
        ),
        "{out}"
    );
    let local_head = s.local().refname_to_id(BRANCH).unwrap();
    assert_eq!(s.remote_ref(BRANCH), Some(local_head));
    assert!(s.remote_ref(&applied(1)).is_some());
    assert_eq!(s.remote_ref(&inbox(1)), None);
    assert!(s.remote_ref(&inbox(2)).is_some());
    assert!(s.remote_ref(&inbox(3)).is_some());
    let remote = Repository::open_bare(s.remote()).unwrap();
    let status = remote
        .find_commit(s.remote_ref("refs/agentfs/demo/status").unwrap())
        .unwrap();
    let blob = remote
        .find_blob(
            status
                .tree()
                .unwrap()
                .get_path(Path::new("status.json"))
                .unwrap()
                .id(),
        )
        .unwrap();
    let doc: serde_json::Value = serde_json::from_slice(blob.content()).unwrap();
    assert_eq!(doc["blocked"]["inbox"], 2);
    assert_eq!(doc["blocked"]["reason"], "conflict");

    // A crash between the commit and the ref moves.
    let local = s.local();
    let p1 = local.refname_to_id(&applied(1)).unwrap();
    local.find_reference(&applied(1)).unwrap().delete().unwrap();
    local.reference(&inbox(1), p1, true, "test").unwrap();
    fs::remove_file(s.state().join("meta/patch.json")).unwrap();
    let out = s.agentfs(&["patch", "apply", "--no-fetch"]);
    let err = text(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(err.contains("were already applied"), "{err}");
    assert_eq!(s.inbox_commits(1), 1);
    assert_eq!(s.local().refname_to_id(BRANCH).unwrap(), local_head);

    // Resolved: 2 rebased onto the pushed head.
    let b2 = s.file("b2", "b-merged\n");
    s.submit_ok(
        &p,
        &[
            "--put",
            &format!("b.txt={b2}"),
            "--replace",
            "2",
            "-m",
            "two, rebased",
        ],
    );
    let out = s.agentfs(&["patch", "apply"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(
        fs::read_to_string(s.tree().join("b.txt")).unwrap(),
        "b-merged\n"
    );
    assert_eq!(fs::read_to_string(s.tree().join("c.txt")).unwrap(), "c\n");
    for n in 1..=3 {
        assert_eq!(s.inbox_commits(n), 1, "patch {n}");
    }
    let out = s.ok(&["push"]);
    assert!(out.contains("2 inbox ref(s) deleted"), "{out}");
    for n in 1..=3 {
        assert_eq!(s.remote_ref(&inbox(n)), None, "inbox {n}");
        assert!(s.remote_ref(&applied(n)).is_some(), "applied {n}");
    }
    let status = s.ok(&["status"]);
    assert!(
        status.contains("patches:   last applied 3, 0 queued, ok"),
        "{status}"
    );
    // A new submission numbers after the applied ones.
    let d = s.file("d", "d\n");
    let out = s.submit_ok(&p, &["--put", &format!("d.txt={d}")]);
    assert!(out.contains("queued patch 4"), "{out}");
}

#[test]
fn apply_needs_the_lock_and_a_root() {
    if !have_git() {
        return;
    }
    let s = Setup::new();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_agentfs"));
    cmd.arg("--state").arg(s.state()).args(["patch", "apply"]);
    s.hermetic(&mut cmd);
    let out = cmd.output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(
        text(&out.stderr).contains("--root"),
        "{}",
        text(&out.stderr)
    );
    // Nothing queued, no remote branch yet: nothing to do.
    let out = s.agentfs(&["patch", "apply"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let status = s.ok(&["status"]);
    assert!(
        status.contains("patches:   none applied, 0 queued, ok"),
        "{status}"
    );
}

/// A gap (a number missing below the highest one) is cleared with
/// `--skip N`, and the queue moves on.
#[test]
fn a_gap_is_cleared_with_skip() {
    if !have_git() {
        return;
    }
    let s = Setup::new();
    s.record("a.txt", "a0\n");
    s.ok(&["push"]);
    let p = s.producer("p");
    let a1 = s.file("a1", "a1\n");
    let b1 = s.file("b1", "b1\n");
    s.submit_ok(&p, &["--put", &format!("a.txt={a1}"), "-m", "one"]);
    s.submit_ok(&p, &["--put", &format!("b.txt={b1}"), "-m", "two"]);
    // Patch 1 vanishes from the remote.
    Repository::open_bare(s.remote())
        .unwrap()
        .find_reference(&inbox(1))
        .unwrap()
        .delete()
        .unwrap();
    let out = s.agentfs(&["patch", "apply"]);
    let err = text(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(err.contains("patches: blocked at 1 (gap)"), "{err}");
    assert!(err.contains("`agentfs patch submit --skip 1`"), "{err}");

    // As the hint says.
    let out = s.submit_ok(&p, &["--skip", "1"]);
    assert!(out.contains("queued patch 1"), "{out}");
    let out = s.agentfs(&["patch", "apply"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(fs::read_to_string(s.tree().join("a.txt")).unwrap(), "a0\n");
    assert_eq!(fs::read_to_string(s.tree().join("b.txt")).unwrap(), "b1\n");
    assert_eq!(s.inbox_commits(1), 1);
    assert_eq!(s.inbox_commits(2), 1);

    // Applied: skipping it now is refused, by the applied ref or, without
    // it (held back while the branch cannot push), by the remote status.
    s.ok(&["push"]);
    let out = s.submit(&p, &["--skip", "2"]);
    assert!(
        text(&out.stderr).contains("already applied"),
        "{}",
        text(&out.stderr)
    );
    Repository::open_bare(s.remote())
        .unwrap()
        .find_reference(&applied(2))
        .unwrap()
        .delete()
        .unwrap();
    let out = s.submit(&p, &["--skip", "2"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        text(&out.stderr).contains("already applied"),
        "{}",
        text(&out.stderr)
    );
}

/// A queue that cannot run at all is an alarm and reads as blocked
/// (`failed`) in `agentfs status`, not as the previous run's state.
#[test]
fn a_queue_that_cannot_run_reads_as_blocked() {
    if !have_git() {
        return;
    }
    let s = Setup::new();
    s.record("a.txt", "a0\n");
    s.ok(&["patch", "apply", "--no-fetch"]);
    // The status ref is locked.
    let refs = s.state().join("git/refs/agentfs/demo");
    fs::create_dir_all(&refs).unwrap();
    fs::write(refs.join("status.lock"), "").unwrap();
    let out = s.agentfs(&["patch", "apply", "--no-fetch"]);
    let err = text(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(err.contains("alarm: "), "{err}");
    let status = s.ok(&["status"]);
    assert!(status.contains("blocked at 0 (failed)"), "{status}");
    let json = s.ok(&["patch", "status", "--json"]);
    let doc: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(doc["cache"]["blocked"]["reason"], "failed");
}
