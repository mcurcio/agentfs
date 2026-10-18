//! Crash injection through the `agentfs` binary (design §18 test 3; the
//! crash model of docs/architecture.md): `record`, `push`, `run`,
//! `serve-history` with `record --history-service` fronts, and `patch
//! apply`, each killed (SIGKILL to itself) at a failpoint, then recovered
//! by the next command and checked against the oracle of
//! `agentfs-core/tests/support/crash_oracle.rs`. The engine's own matrix
//! is `agentfs-core/tests/crash_points.rs`; this file covers what only a
//! process shows: the state lock released by the kernel, the history
//! service and its fronts, the commit and push loops of `run`, the patch
//! queue.
//!
//! Built only with the `failpoints` feature: `cargo test -p agentfs
//! --features failpoints --test crash_daemon`. No FUSE and no root (the
//! mount points are in `crash_mount.rs`). Skipped, with a message, when
//! `git` is not installed (the patch queue and fsck need it).

#![cfg(feature = "failpoints")]

#[path = "../../agentfs-core/tests/support/crash_oracle.rs"]
mod crash_oracle;

use std::fs;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use agentfs_core::failpoints::ENV_VAR;
use agentfs_core::volume::StateDir;
use git2::{Oid, Repository, Signature};

const BRANCH: &str = "refs/heads/agentfs/demo";
const SIGKILL: i32 = 9;

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

/// A volume `demo` with a bare remote `origin`.
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
        s.ok(&["init", "--volume", "demo"]);
        let mut config = git2::Config::open(&s.state().join("git/config")).unwrap();
        config
            .set_str(
                "remote.origin.url",
                &format!("file://{}", s.remote().display()),
            )
            .unwrap();
        config.set_str("agentfs.commitWindow", "10ms").unwrap();
        config.set_str("agentfs.pushInterval", "1s").unwrap();
        s
    }

    fn state(&self) -> PathBuf {
        self.dir.path().join("state")
    }

    fn git_dir(&self) -> PathBuf {
        self.state().join("git")
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

    fn hermetic(&self, cmd: &mut Command) {
        let home = self.home();
        for (name, _) in std::env::vars_os() {
            let n = name.to_string_lossy();
            if n.starts_with("OTEL_") || n.starts_with("AGENTFS_") {
                cmd.env_remove(name);
            }
        }
        cmd.env("HOME", &home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "Producer")
            .env("GIT_AUTHOR_EMAIL", "producer@example.com")
            .env("GIT_COMMITTER_NAME", "Producer")
            .env("GIT_COMMITTER_EMAIL", "producer@example.com")
            .env("AGENTFS_REPLICA", "test-0");
    }

    /// `agentfs` with `--state`, `--root` and `args`; failpoints `spec`.
    fn command(&self, spec: &str, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_agentfs"));
        cmd.arg("--state")
            .arg(self.state())
            .arg("--root")
            .arg(self.tree())
            .args(args);
        self.hermetic(&mut cmd);
        if !spec.is_empty() {
            cmd.env(ENV_VAR, spec);
        }
        cmd
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.command("", args).output().unwrap();
        assert!(
            out.status.success(),
            "agentfs {args:?}: {}{}",
            text(&out.stdout),
            text(&out.stderr)
        );
        text(&out.stdout)
    }

    /// Runs `args` with `spec`, which must kill it at `point`.
    fn killed(&self, spec: &str, point: &str, args: &[&str]) -> Output {
        let out = self.command(spec, args).output().unwrap();
        assert_killed(&out, point, &format!("agentfs {args:?} with {spec}"));
        out
    }

    fn write(&self, name: &str, content: &str) {
        let path = self.tree().join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    fn head(&self) -> Option<Oid> {
        Repository::open_bare(self.git_dir())
            .unwrap()
            .refname_to_id(BRANCH)
            .ok()
    }

    /// `name` in the branch head, if there.
    fn committed(&self, name: &str) -> Option<String> {
        let repo = Repository::open_bare(self.git_dir()).unwrap();
        let head = repo.find_reference(BRANCH).ok()?.peel_to_commit().ok()?;
        let entry = head.tree().ok()?.get_path(Path::new(name)).ok()?;
        let blob = repo.find_blob(entry.id()).ok()?;
        Some(text(blob.content()))
    }

    fn remote_ref(&self, name: &str) -> Option<Oid> {
        Repository::open_bare(self.remote())
            .unwrap()
            .refname_to_id(name)
            .ok()
    }

    /// The state area is clean and the history consistent: the oracle's
    /// checks, plus no empty commit on the first-parent history.
    fn check(&self) {
        crash_oracle::check_clean(&self.state(), BRANCH).unwrap();
        let repo = Repository::open_bare(self.git_dir()).unwrap();
        for oid in crash_oracle::first_parent(&repo, BRANCH) {
            let commit = repo.find_commit(oid).unwrap();
            if let Ok(parent) = commit.parent(0) {
                let is_patch_marker = commit
                    .message()
                    .unwrap_or("")
                    .lines()
                    .any(|l| l == "Agentfs-Trigger: patch");
                assert!(
                    commit.tree_id() != parent.tree_id() || is_patch_marker,
                    "empty commit {oid}: {:?}",
                    commit.message()
                );
            }
        }
    }

    /// The remote's branch is the head, with no diverged ref, and passes
    /// fsck.
    fn check_remote(&self) {
        crash_oracle::check_remote(&self.remote(), BRANCH, "demo", self.head().unwrap(), false)
            .unwrap();
    }
}

fn assert_killed(out: &Output, point: &str, what: &str) {
    let err = text(&out.stderr);
    assert_eq!(
        out.status.signal(),
        Some(SIGKILL),
        "{what}: expected SIGKILL, got {:?}\n{}{err}",
        out.status,
        text(&out.stdout)
    );
    assert!(
        err.contains(&format!("agentfs failpoint {point} triggered (kill)")),
        "{what}: {err}"
    );
}

/// Every step of `record` (freeze, journal, commit, truncation): killed
/// at each, the next `flush` commits what was journaled (a record whose
/// write reached the journal before its fsync may land too: SIGKILL keeps
/// the page cache), recording again lands the file exactly once, and the
/// state area is clean. Power loss, which drops the page cache, is the
/// power-loss workflow's.
#[test]
fn record_killed_at_every_step() {
    if !have_git() {
        return;
    }
    // (point, whether the record is durable when it fires)
    let points = [
        ("pending.after_freeze", false),
        ("journal.before_write", false),
        ("journal.before_fsync", false),
        ("journal.after_fsync", true),
        ("record.after_journal", true),
        ("commit.after_tree", true),
        ("commit.sync_objects", true),
        ("commit.after_object", true),
        ("commit.after_ref", true),
        ("commit.after_sync", true),
        ("journal.truncate.after_tmp", true),
        ("journal.truncate.after_rename", true),
        ("commit.before_pending_remove", true),
        ("pending.remove.between", true),
    ];
    for (point, durable) in points {
        let s = Setup::new();
        s.write("base.txt", "base\n");
        s.ok(&["record", "base.txt"]);
        s.write("a.txt", "one\n");
        s.killed(&format!("{point}=kill@1"), point, &["record", "a.txt"]);
        s.ok(&["flush"]);
        let a = s.committed("a.txt");
        if durable {
            assert_eq!(
                a.as_deref(),
                Some("one\n"),
                "{point}: a journaled record is lost"
            );
        }
        s.check();
        let before = s.head();
        s.ok(&["record", "a.txt"]);
        assert_eq!(s.committed("a.txt").as_deref(), Some("one\n"), "{point}");
        if durable {
            assert_eq!(s.head(), before, "{point}: recorded twice");
        }
        s.check();
    }
}

/// A push killed after the remote took the branch but before the push
/// state was saved, or while saving it: the next push finds the remote up
/// to date and records it.
#[test]
fn push_killed_after_the_remote_updated() {
    if !have_git() {
        return;
    }
    for point in ["push.after_remote_update", "state.json.before_rename"] {
        let s = Setup::new();
        s.write("a.txt", "a\n");
        s.ok(&["record", "a.txt"]);
        s.killed(&format!("{point}=kill@1"), point, &["push"]);
        s.ok(&["push"]);
        s.check_remote();
        let state: serde_json::Value = serde_json::from_slice(
            &fs::read(StateDir::new(s.state()).push_state("origin")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            state["last_pushed"].as_str(),
            Some(s.head().unwrap().to_string().as_str()),
            "{point}: {state}"
        );
    }
}

/// Someone else wrote the remote branch: a push killed while saving the
/// local head under `diverged/`, before or after the ref reached the
/// remote, saves it exactly once across the restart and never forces.
#[test]
fn diverged_push_killed_saves_once() {
    if !have_git() {
        return;
    }
    for point in ["push.diverged.after_pending", "push.diverged.after_ref"] {
        let s = Setup::new();
        s.write("a.txt", "a\n");
        s.ok(&["record", "a.txt"]);
        s.ok(&["push"]);
        let remote = Repository::open_bare(s.remote()).unwrap();
        let theirs = {
            let parent = remote
                .find_reference(BRANCH)
                .unwrap()
                .peel_to_commit()
                .unwrap();
            let sig = Signature::now("someone else", "else@example.invalid").unwrap();
            let id = remote
                .commit(
                    None,
                    &sig,
                    &sig,
                    "out of band",
                    &parent.tree().unwrap(),
                    &[&parent],
                )
                .unwrap();
            remote.reference(BRANCH, id, true, "out of band").unwrap();
            id
        };
        s.write("a.txt", "a2\n");
        s.ok(&["record", "a.txt"]);
        s.killed(&format!("{point}=kill@1"), point, &["push"]);
        for _ in 0..2 {
            let out = s.command("", &["push"]).output().unwrap();
            assert_eq!(out.status.code(), Some(1), "{point}: {}", text(&out.stderr));
        }
        assert_eq!(s.remote_ref(BRANCH), Some(theirs), "{point}: forced");
        let saved: Vec<Oid> = remote
            .references_glob("refs/agentfs/demo/diverged/*")
            .unwrap()
            .filter_map(|r| r.ok()?.target())
            .collect();
        assert_eq!(saved.len(), 1, "{point}: saved {saved:?}");
        let head = s.head().unwrap();
        let repo = Repository::open_bare(s.git_dir()).unwrap();
        assert!(
            saved[0] == head || repo.graph_descendant_of(head, saved[0]).unwrap(),
            "{point}: the saved head is not ours"
        );
        crash_oracle::fsck(&s.remote()).unwrap();
        s.check();
    }
}

/// A long-running process: `run`, or `serve-history`.
struct Daemon {
    child: Option<Child>,
    log: PathBuf,
}

impl Daemon {
    fn start(s: &Setup, spec: &str, args: &[&str], name: &str) -> Self {
        let log = s.dir.path().join(format!("{name}.log"));
        let child = s
            .command(spec, args)
            .stdout(Stdio::null())
            .stderr(fs::File::create(&log).unwrap())
            .stdin(Stdio::null())
            .spawn()
            .unwrap();
        Self {
            child: Some(child),
            log,
        }
    }

    fn log(&self) -> String {
        fs::read_to_string(&self.log).unwrap_or_default()
    }

    fn wait_for(&self, what: &str, mut ready: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while !ready() {
            assert!(
                Instant::now() < deadline,
                "never {what}; log:\n{}",
                self.log()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Waits for the process to end; its exit status.
    fn wait(&mut self) -> std::process::ExitStatus {
        let deadline = Instant::now() + Duration::from_secs(60);
        let child = self.child.as_mut().unwrap();
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                self.child = None;
                return status;
            }
            assert!(Instant::now() < deadline, "never exited:\n{}", self.log());
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn stop(mut self) {
        let pid = rustix::process::Pid::from_child(self.child.as_ref().unwrap());
        rustix::process::kill_process(pid, rustix::process::Signal::TERM).unwrap();
        let status = self.wait();
        assert!(status.success(), "exit {status:?}:\n{}", self.log());
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// `run` with a backlog left by a killed `record`: its commit loop is
/// killed after the branch ref moved, then its push loop after the remote
/// took the branch; each restart carries on, and the remote ends at the
/// head.
#[test]
fn run_killed_in_its_commit_and_push_loops() {
    if !have_git() {
        return;
    }
    let s = Setup::new();
    s.write("a.txt", "a\n");
    s.killed(
        "commit.after_tree=kill@1",
        "commit.after_tree",
        &["record", "a.txt"],
    );
    assert_eq!(s.committed("a.txt"), None);

    let mut run = Daemon::start(&s, "commit.after_ref=kill@1", &["run"], "run-1");
    let status = run.wait();
    assert_eq!(status.signal(), Some(SIGKILL), "{}", run.log());
    assert_eq!(s.committed("a.txt").as_deref(), Some("a\n"));

    let mut run = Daemon::start(&s, "push.after_remote_update=kill@1", &["run"], "run-2");
    let status = run.wait();
    assert_eq!(status.signal(), Some(SIGKILL), "{}", run.log());
    assert_eq!(s.remote_ref(BRANCH), s.head());

    let run = Daemon::start(&s, "", &["run"], "run-3");
    let state = StateDir::new(s.state()).push_state("origin");
    let head = s.head().unwrap().to_string();
    run.wait_for("recorded the push", || {
        fs::read(&state)
            .ok()
            .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
            .is_some_and(|v| v["last_pushed"].as_str() == Some(head.as_str()))
    });
    run.stop();
    s.check();
    s.check_remote();
}

impl Setup {
    fn serve(&self, spec: &str, name: &str) -> Daemon {
        let d = Daemon::start(self, spec, &["serve-history"], name);
        d.wait_for("served", || {
            d.log().contains("serving history for volume demo")
        });
        d
    }

    /// `record --history-service` of `paths`, with failpoints `spec`.
    fn front_record(&self, spec: &str, paths: &[&str]) -> Output {
        let socket = StateDir::new(self.state()).history_socket();
        let mut args = vec!["record", "--history-service"];
        let socket = socket.display().to_string();
        args.push(&socket);
        args.extend(["--history-wait", "10s"]);
        args.extend_from_slice(paths);
        self.command(spec, &args)
            .env("AGENTFS_POD", "front-0")
            .env("AGENTFS_POD_UID", "front-0-uid")
            .output()
            .unwrap()
    }
}

/// The history service killed after journaling a front's record but
/// before replying, and a front killed after the reply: the record is
/// committed once the service is back, exactly once, and the front that
/// lost its reply failed rather than claiming success.
#[test]
fn history_service_and_front_killed_around_the_reply() {
    if !have_git() {
        return;
    }
    let s = Setup::new();

    // The service dies between the journal and its reply.
    let mut service = s.serve("service.after_record=kill@1", "serve-1");
    s.write("a.txt", "a\n");
    let out = s.front_record("", &["a.txt"]);
    assert!(!out.status.success(), "the front claimed success");
    assert_eq!(service.wait().signal(), Some(SIGKILL), "{}", service.log());
    assert!(
        service
            .log()
            .contains("agentfs failpoint service.after_record triggered (kill)")
    );

    // The next service commits the journaled record.
    let service = s.serve("", "serve-2");
    service.wait_for("committed a.txt", || s.committed("a.txt").is_some());

    // The front dies after the reply: its record is the service's to keep.
    s.write("b.txt", "b\n");
    let out = s.front_record("front.after_reply=kill@1", &["b.txt"]);
    assert_killed(&out, "front.after_reply", "record --history-service");
    service.wait_for("committed b.txt", || s.committed("b.txt").is_some());

    // A front whose reply is lost reports the failure; the record stands.
    s.write("c.txt", "c\n");
    let out = s.front_record("front.after_reply=error@1", &["c.txt"]);
    assert!(!out.status.success(), "{}", text(&out.stderr));
    service.wait_for("committed c.txt", || s.committed("c.txt").is_some());
    service.stop();

    let repo = Repository::open_bare(s.git_dir()).unwrap();
    let commits = crash_oracle::first_parent(&repo, BRANCH);
    for name in ["a.txt", "b.txt", "c.txt"] {
        let changed = commits
            .iter()
            .filter(|oid| {
                let c = repo.find_commit(**oid).unwrap();
                let entry = |t: &git2::Tree<'_>| t.get_path(Path::new(name)).ok().map(|e| e.id());
                let ours = entry(&c.tree().unwrap());
                let parent = c.parent(0).ok().and_then(|p| entry(&p.tree().unwrap()));
                ours != parent
            })
            .count();
        assert_eq!(changed, 1, "{name} changed by {changed} commits");
    }
    s.check();
}

/// `patch apply` killed after writing the patched tree, after recording
/// it, after its commit, and after the queue status: the next apply
/// finishes the patch exactly once and the push carries the queue.
#[test]
fn patch_apply_killed_at_every_step() {
    if !have_git() {
        return;
    }
    for point in [
        "patch.after_tree_write",
        "patch.after_record",
        "patch.after_commit",
        "queue.after_status",
    ] {
        let s = Setup::new();
        s.write("a.txt", "a0\n");
        s.ok(&["record", "a.txt"]);
        s.ok(&["push"]);
        let producer = s.dir.path().join("producer");
        let mut clone = Command::new("git");
        clone
            .args(["clone", "--quiet"])
            .arg(format!("file://{}", s.remote().display()))
            .arg(&producer);
        s.hermetic(&mut clone);
        let out = clone.output().unwrap();
        assert!(out.status.success(), "{}", text(&out.stderr));
        let put = s.dir.path().join("a1");
        fs::write(&put, "a1\n").unwrap();
        let mut submit = Command::new(env!("CARGO_BIN_EXE_agentfs"));
        submit
            .args(["patch", "submit", "--volume", "demo", "--repo"])
            .arg(&producer)
            .arg("--put")
            .arg(format!("a.txt={}", put.display()))
            .args(["-m", "one"]);
        s.hermetic(&mut submit);
        let out = submit.output().unwrap();
        assert!(out.status.success(), "{point}: {}", text(&out.stderr));

        s.killed(&format!("{point}=kill@1"), point, &["patch", "apply"]);
        s.ok(&["patch", "apply"]);
        assert_eq!(fs::read_to_string(s.tree().join("a.txt")).unwrap(), "a1\n");
        assert_eq!(s.committed("a.txt").as_deref(), Some("a1\n"), "{point}");
        let repo = Repository::open_bare(s.git_dir()).unwrap();
        let applies = crash_oracle::first_parent(&repo, BRANCH)
            .into_iter()
            .filter(|oid| {
                repo.find_commit(*oid)
                    .unwrap()
                    .message()
                    .unwrap_or("")
                    .contains("Agentfs-Inbox: 1\n")
            })
            .count();
        assert_eq!(applies, 1, "{point}: patch 1 applied {applies} times");
        s.ok(&["push"]);
        s.check_remote();
        assert!(
            s.remote_ref("refs/agentfs/demo/applied/0000000001")
                .is_some()
        );
        assert_eq!(s.remote_ref("refs/agentfs/demo/inbox/0000000001"), None);
        s.check();
    }
}
