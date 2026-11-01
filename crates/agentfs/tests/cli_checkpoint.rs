//! End-to-end tests of `agentfs checkpoint` (design §17.1, §19.2 item 11;
//! bead agentfs-mia.6): one change-line commit per intent, merged back
//! into the branch, with and without a daemon, and a remote that pushes
//! the change line instead of the branch. The patch tests drive the real
//! `git` through `patch submit`; they are skipped, with a message, when it
//! is not installed.

use std::fs;
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use git2::{Oid, Repository};

const BRANCH: &str = "refs/heads/agentfs/demo";
const CHANGE_LINE: &str = "refs/agentfs/demo/change-line";

fn have_git() -> bool {
    let found = Command::new("git").arg("--version").output().is_ok();
    if !found {
        eprintln!("skipping: git is not installed");
    }
    found
}

/// A volume `demo` in a temporary directory.
struct Volume {
    dir: tempfile::TempDir,
}

impl Volume {
    fn new() -> Self {
        let v = Self {
            dir: tempfile::tempdir().unwrap(),
        };
        fs::create_dir_all(v.tree()).unwrap();
        fs::create_dir_all(v.home()).unwrap();
        v.ok(&["init", "--volume", "demo"]);
        v
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

    /// A bare repository `name`.git beside the volume, as a `file://` URL.
    fn bare(&self, name: &str) -> String {
        let path = self.dir.path().join(format!("{name}.git"));
        Repository::init_bare(&path).unwrap();
        format!("file://{}", path.display())
    }

    fn config(&self, pairs: &[(&str, &str)]) {
        let mut config = git2::Config::open(&self.state().join("git/config")).unwrap();
        for (k, v) in pairs {
            config.set_str(k, v).unwrap();
        }
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

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_agentfs"));
        cmd.arg("--state")
            .arg(self.state())
            .arg("--root")
            .arg(self.tree())
            .args(args);
        self.hermetic(&mut cmd);
        cmd
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.run(args);
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

    fn local(&self) -> Repository {
        Repository::open_bare(self.state().join("git")).unwrap()
    }

    fn head(&self) -> Oid {
        self.local().refname_to_id(BRANCH).unwrap()
    }

    fn change_line(&self) -> Option<Oid> {
        self.local().refname_to_id(CHANGE_LINE).ok()
    }

    /// The commits on the change line, newest first.
    fn change_line_len(&self) -> usize {
        let repo = self.local();
        let mut walk = repo.revwalk().unwrap();
        walk.push(self.change_line().unwrap()).unwrap();
        walk.simplify_first_parent().unwrap();
        walk.count()
    }

    /// `agentfs checkpoint` with `args`: the checkpoint and marker it
    /// printed, `None` for nothing new.
    fn checkpoint(&self, args: &[&str]) -> Option<(Oid, Oid)> {
        let mut all = vec!["checkpoint"];
        all.extend_from_slice(args);
        let out = self.ok(&all);
        if out.contains("nothing new since the last checkpoint") {
            return None;
        }
        let rest = out
            .strip_prefix("checkpoint ")
            .unwrap_or_else(|| panic!("{out}"));
        let (change, rest) = rest.split_once(", merged into the branch by ").unwrap();
        Some((
            Oid::from_str(change).unwrap(),
            Oid::from_str(rest.trim()).unwrap(),
        ))
    }

    /// `agentfs patch submit --put NAME=<content>` in a fresh clone of
    /// `remote`.
    fn submit_put(&self, remote: &str, name: &str, content: &str, clone: &str) {
        let producer = self.dir.path().join(clone);
        let mut cmd = Command::new("git");
        cmd.args(["clone", "--quiet", remote]).arg(&producer);
        self.hermetic(&mut cmd);
        assert!(cmd.output().unwrap().status.success());
        let file = self.dir.path().join(format!("{clone}.content"));
        fs::write(&file, content).unwrap();
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_agentfs"));
        cmd.args(["patch", "submit", "--volume", "demo", "--repo"])
            .arg(&producer)
            .args([
                "--put",
                &format!("{name}={}", file.display()),
                "-m",
                &format!("patch {name}"),
            ]);
        self.hermetic(&mut cmd);
        let out = cmd.output().unwrap();
        assert!(out.status.success(), "{}", text(&out.stderr));
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn parents(repo: &Repository, oid: Oid) -> Vec<Oid> {
    repo.find_commit(oid).unwrap().parent_ids().collect()
}

/// Acceptance 1: three changes, one checkpoint: one change-line commit
/// with the head's tree and the message given, merged back by a
/// two-parent marker; no file changes.
#[test]
fn a_checkpoint_cuts_one_commit() {
    let v = Volume::new();
    v.record("a.txt", "a\n");
    v.record("b.txt", "b\n");
    v.record("c.txt", "c\n");
    let head = v.head();
    let (change, marker) = v.checkpoint(&["-m", "intent A"]).expect("nothing cut");
    let repo = v.local();
    assert_eq!(v.change_line(), Some(change));
    assert_eq!(v.change_line_len(), 1);
    let c = repo.find_commit(change).unwrap();
    assert_eq!(c.parent_count(), 0);
    assert_eq!(c.tree_id(), repo.find_commit(head).unwrap().tree_id());
    assert_eq!(c.summary(), Ok(Some("intent A")));
    assert_eq!(v.head(), marker);
    assert_eq!(parents(&repo, marker), [head, change]);
    assert_eq!(repo.find_commit(marker).unwrap().tree_id(), c.tree_id());
    for (name, content) in [("a.txt", "a\n"), ("b.txt", "b\n"), ("c.txt", "c\n")] {
        assert_eq!(fs::read_to_string(v.tree().join(name)).unwrap(), content);
    }
    assert_eq!(fs::read_dir(v.tree()).unwrap().count(), 3);
    // `status` reads the sequence from the marker.
    let status = v.ok(&["status"]);
    assert!(status.contains(&marker.to_string()), "{status}");
}

/// Acceptance 2: a second checkpoint at once is nothing new: exit 0, no
/// commit.
#[test]
fn nothing_new() {
    let v = Volume::new();
    v.record("a.txt", "a\n");
    v.checkpoint(&[]).expect("nothing cut");
    let head = v.head();
    let line = v.change_line();
    assert_eq!(v.checkpoint(&["-m", "again"]), None);
    assert_eq!(v.head(), head);
    assert_eq!(v.change_line(), line);
    // With no branch at all there is nothing to checkpoint.
    let empty = Volume::new();
    let out = empty.run(&["checkpoint"]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stdout));
    assert!(
        text(&out.stderr).contains("nothing to checkpoint"),
        "{}",
        text(&out.stderr)
    );
}

/// Acceptance 3 and 6: patch 1, landed between two checkpoints, is the
/// second's second parent; the queue still numbers, finds and lands
/// patches across the markers.
#[test]
fn patches_become_parents() {
    if !have_git() {
        return;
    }
    let v = Volume::new();
    let origin = v.bare("origin");
    v.config(&[("remote.origin.url", &origin)]);
    v.record("a.txt", "a\n");
    let (first, _) = v.checkpoint(&["-m", "first"]).expect("nothing cut");
    v.ok(&["push"]);
    v.submit_put(&origin, "p1.txt", "one\n", "producer1");
    let out = v.run(&["patch", "apply"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(text(&out.stderr).contains("patch 1 applied as "));
    let applied1 = v
        .local()
        .refname_to_id("refs/agentfs/demo/applied/0000000001")
        .unwrap();
    let (second, _) = v.checkpoint(&["-m", "second"]).expect("nothing cut");
    let repo = v.local();
    assert_eq!(parents(&repo, second), [first, applied1]);
    assert_eq!(
        repo.find_commit(second).unwrap().tree_id(),
        repo.find_commit(v.head()).unwrap().tree_id()
    );

    // The queue across a checkpoint marker: patch 2 is numbered 2 and
    // lands; the next checkpoint merges it alone.
    v.ok(&["push"]);
    v.submit_put(&origin, "p2.txt", "two\n", "producer2");
    let out = v.run(&["patch", "apply"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let err = text(&out.stderr);
    assert!(err.contains("patch 2 applied as "), "{err}");
    assert!(!err.contains("stale"), "{err}");
    assert_eq!(
        fs::read_to_string(v.tree().join("p2.txt")).unwrap(),
        "two\n"
    );
    let applied2 = repo
        .refname_to_id("refs/agentfs/demo/applied/0000000002")
        .unwrap();
    let (third, _) = v.checkpoint(&[]).expect("nothing cut");
    assert_eq!(parents(&repo, third), [second, applied2]);
    v.record("after.txt", "x\n");
}

fn wait_until_running(child: &mut Child) {
    wait_for(child, "running volume");
}

/// Reads `child`'s stderr until a line starts with `prefix`, then drains
/// the rest in the background.
fn wait_for(child: &mut Child, prefix: &str) {
    use std::io::BufRead as _;
    let mut reader = std::io::BufReader::new(child.stderr.take().unwrap());
    let mut seen = String::new();
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).unwrap();
        assert!(n > 0, "agentfs exited early: {seen}");
        seen.push_str(&line);
        if line.starts_with(prefix) {
            std::thread::spawn(move || std::io::copy(&mut reader, &mut std::io::sink()));
            return;
        }
    }
}

fn wait_with_timeout(child: &mut Child, timeout: Duration) -> std::process::ExitStatus {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("agentfs run did not exit within {timeout:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Journals a record of `name` without committing it, as `run`'s commit
/// window would leave it.
fn journal(v: &Volume, name: &str) {
    fs::write(v.tree().join(name), name).unwrap();
    let state = agentfs_core::volume::StateDir::new(v.state());
    let (rec, _, _) = agentfs_core::volume::open(
        &state,
        &v.tree(),
        agentfs_core::identity::Context {
            volume: "demo".into(),
            replica: "x".into(),
            pod_uid: "x".into(),
            ..Default::default()
        },
        std::sync::Arc::new(agentfs_core::events::NullSink),
        &agentfs_core::telemetry::Telemetry::noop(),
    )
    .unwrap();
    rec.record(
        agentfs_core::record::Trigger::Close,
        agentfs_core::record::Writer {
            caller: agentfs_core::record::Caller {
                uid: 1,
                gid: 1,
                pid: 1,
            },
            entry: agentfs_core::record::EntryPoint::Main,
        },
        vec![agentfs_core::volume::Input::Write {
            path: agentfs_core::paths::RepoPath::new(name).unwrap(),
            backing: v.tree().join(name),
            executable: false,
        }],
    )
    .unwrap();
}

/// Acceptance 4: under `agentfs run`, the checkpoint goes through the
/// control socket (the lock path would exit 75): the daemon's commit loop
/// commits the record its window still holds, then cuts; it keeps
/// committing and pushing on top of the marker.
#[test]
fn under_a_daemon() {
    let v = Volume::new();
    let origin = v.bare("origin");
    v.config(&[
        ("remote.origin.url", &origin),
        ("agentfs.commitWindow", "1h"),
        ("agentfs.pushInterval", "1h"),
    ]);
    v.record("a.txt", "a\n");
    journal(&v, "b.txt");
    let committed = v.head();
    let mut child = v
        .command(&["run", "--push-grace", "1s"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    wait_until_running(&mut child);
    let cut = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        v.checkpoint(&["-m", "under run"])
    }));
    let Ok(Some((change, marker))) = cut else {
        let _ = child.kill();
        panic!("the checkpoint through the daemon failed");
    };
    let repo = v.local();
    assert_eq!(v.head(), marker);
    let first = parents(&repo, marker)[0];
    assert_ne!(
        first, committed,
        "the record in the window was not committed first"
    );
    assert_eq!(parents(&repo, first), [committed]);
    assert!(
        repo.find_commit(change)
            .unwrap()
            .tree()
            .unwrap()
            .get_name("b.txt")
            .is_some()
    );
    assert_eq!(parents(&repo, marker)[1], change);
    // The author is the caller, through the socket's peer credentials.
    let uid = rustix::process::getuid().as_raw();
    assert_eq!(
        repo.find_commit(change).unwrap().author().name(),
        Ok(format!("uid-{uid}").as_str())
    );
    // The daemon serves push now after it: the marker reaches the remote.
    let pushed = v.run(&["push", "--timeout", "30s"]);
    let out = text(&pushed.stdout);
    let _ = rustix::process::kill_process(
        rustix::process::Pid::from_child(&child),
        rustix::process::Signal::TERM,
    );
    let status = wait_with_timeout(&mut child, Duration::from_secs(30));
    assert!(pushed.status.success(), "{out}{}", text(&pushed.stderr));
    assert!(out.contains("through the running daemon"), "{out}");
    assert!(status.success(), "{status:?}");
    assert_eq!(
        Repository::open_bare(origin.strip_prefix("file://").unwrap())
            .unwrap()
            .refname_to_id(BRANCH)
            .ok(),
        Some(marker)
    );
    // After it stops, the CLI's next record commits on top of the marker
    // (the daemon's own commit loop: `a_daemon_commits_on_top_of_its_marker`).
    v.record("c.txt", "c\n");
    assert_eq!(parents(&repo, v.head()), [marker]);
}

/// Acceptance 4, the commit loop: a record that reaches a running daemon
/// after its checkpoint is committed by that daemon on top of the marker.
/// `run` has no way in for a record while it is up (`agentfs record` takes
/// the state-area lock, exit 75), so this drives the same commit loop
/// (`agentfs_core::service`) through `serve-history`, recording with
/// `record --history-service` and committing with a push through the
/// control socket.
#[test]
fn a_daemon_commits_on_top_of_its_marker() {
    let v = Volume::new();
    let origin = v.bare("origin");
    v.config(&[
        ("remote.origin.url", &origin),
        ("agentfs.commitWindow", "1h"),
        ("agentfs.pushInterval", "1h"),
    ]);
    v.record("a.txt", "a\n");
    let mut child = v
        .command(&["serve-history", "--push-grace", "1s"])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    wait_for(&mut child, "serving history for volume");
    let served = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let (_, marker) = v
            .checkpoint(&["-m", "under serve-history"])
            .expect("nothing cut");
        assert_eq!(v.head(), marker);
        fs::write(v.tree().join("b.txt"), "b\n").unwrap();
        let socket = agentfs_core::volume::StateDir::new(v.state()).history_socket();
        let mut record = v.command(&["record", "--history-service"]);
        record.arg(&socket).args(["--history-wait", "10s", "b.txt"]);
        let out = record.output().unwrap();
        assert!(out.status.success(), "{}", text(&out.stderr));
        // Nothing is committed in the window until the push asks for it.
        assert_eq!(v.head(), marker);
        let pushed = v.run(&["push", "--timeout", "30s"]);
        assert!(
            pushed.status.success(),
            "{}{}",
            text(&pushed.stdout),
            text(&pushed.stderr)
        );
        // The push committed, before the daemon's own commit at stop.
        let head = v.head();
        assert_ne!(head, marker, "the push committed nothing");
        assert_eq!(parents(&v.local(), head), [marker]);
        marker
    }));
    let _ = rustix::process::kill_process(
        rustix::process::Pid::from_child(&child),
        rustix::process::Signal::TERM,
    );
    let status = wait_with_timeout(&mut child, Duration::from_secs(30));
    let marker = served.unwrap_or_else(|e| std::panic::resume_unwind(e));
    assert!(status.success(), "{status:?}");
    let repo = v.local();
    let head = v.head();
    assert_ne!(
        head, marker,
        "the daemon committed nothing after the marker"
    );
    assert_eq!(parents(&repo, head), [marker]);
    assert!(
        repo.find_commit(head)
            .unwrap()
            .tree()
            .unwrap()
            .get_name("b.txt")
            .is_some()
    );
}

/// Acceptance 5: with one remote set to `agentfsPush = changeLine`, a push
/// sends the change line to that remote's branch ref and the branch to the
/// other's.
#[test]
fn a_remote_gets_the_change_line() {
    let v = Volume::new();
    let lines = v.bare("lines");
    let all = v.bare("all");
    v.config(&[
        ("remote.lines.url", &lines),
        ("remote.lines.agentfsPush", "changeLine"),
        ("remote.all.url", &all),
    ]);
    v.record("a.txt", "a\n");
    let (change, _) = v.checkpoint(&["-m", "one"]).expect("nothing cut");
    v.record("b.txt", "b\n");
    let out = v.ok(&["push"]);
    let head = v.head();
    let remote_branch = |url: &str| {
        Repository::open_bare(url.strip_prefix("file://").unwrap())
            .unwrap()
            .refname_to_id(BRANCH)
            .ok()
    };
    assert_eq!(remote_branch(&lines), Some(change), "{out}");
    assert_eq!(remote_branch(&all), Some(head), "{out}");
    // A bad value leaves the remote out, as any misconfigured remote.
    v.config(&[("remote.lines.agentfsPush", "everything")]);
    let status = v.ok(&["status"]);
    assert!(
        status.contains("lines: misconfigured, not pushed") && status.contains("agentfsPush"),
        "{status}"
    );
}
