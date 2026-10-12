//! End-to-end tests of `agentfs push`, `agentfs run` and the remote lines of
//! `agentfs status` (design §11, §17.1).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

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
        let out = v.run(&["init", "--volume", "demo"]);
        assert!(out.status.success(), "{}", stderr(&out));
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

    /// Sets keys in the history repository's own config.
    fn config(&self, pairs: &[(&str, &str)]) {
        let mut config = git2::Config::open(&self.state().join("git/config")).unwrap();
        for (k, v) in pairs {
            config.set_str(k, v).unwrap();
        }
    }

    fn command(&self, args: &[&str]) -> Command {
        let home = self.home();
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_agentfs"));
        cmd.arg("--state")
            .arg(self.state())
            .arg("--root")
            .arg(self.tree())
            .args(args)
            .env_remove("AGENTFS_STATE")
            .env_remove("AGENTFS_ROOT")
            .env("AGENTFS_REPLICA", "test-0")
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", home.join(".config"));
        // Hermetic: no telemetry from the environment running the tests.
        for (name, _) in std::env::vars_os() {
            if name.to_string_lossy().starts_with("OTEL_") {
                cmd.env_remove(name);
            }
        }
        cmd
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    fn record(&self, name: &str, content: &str) {
        fs::write(self.tree().join(name), content).unwrap();
        let out = self.run(&["record", name]);
        assert!(out.status.success(), "{}", stderr(&out));
    }

    fn head(&self) -> git2::Oid {
        branch(&self.state().join("git")).unwrap()
    }
}

fn branch(git_dir: &Path) -> Option<git2::Oid> {
    git2::Repository::open_bare(git_dir)
        .unwrap()
        .refname_to_id("refs/heads/agentfs/demo")
        .ok()
}

fn bare(dir: &Path) {
    git2::Repository::init_bare(dir).unwrap();
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn push_to_two_remotes_one_failing() {
    let v = Volume::new();
    let good = v.dir.path().join("good.git");
    bare(&good);
    let bad = v.dir.path().join("missing.git");
    v.config(&[
        ("remote.good.url", &good.display().to_string()),
        ("remote.bad.url", &bad.display().to_string()),
    ]);
    v.record("a.txt", "a");

    let out = v.run(&["push"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(
        text.contains(&format!("good: pushed none..{}", v.head())),
        "{text}"
    );
    assert!(text.contains("bad: failed (unreachable): "), "{text}");
    assert!(text.contains("retry in 60s"), "{text}");
    assert_eq!(branch(&good), Some(v.head()));
    let err = stderr(&out);
    assert!(
        err.contains("alarm: push to bad failed (unreachable)"),
        "{err}"
    );
    assert!(!err.contains("push to good"), "{err}");

    // Only the selected remote, which now succeeds.
    let out = v.run(&["push", "--remote", "good"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(
        stdout(&out).trim(),
        format!("good: up to date at {}", v.head())
    );

    let out = v.run(&["push", "--remote", "nope"]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("no remote named \"nope\""),
        "{}",
        stderr(&out)
    );
}

#[test]
fn push_with_nothing_committed_or_configured() {
    let v = Volume::new();
    let out = v.run(&["push"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(stdout(&out).trim(), "no remotes configured");

    let remote = v.dir.path().join("r.git");
    bare(&remote);
    v.config(&[("remote.r.url", &remote.display().to_string())]);
    let out = v.run(&["push"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(stdout(&out).trim(), "r: nothing to push");
}

#[test]
fn global_config_cannot_redirect_a_push() {
    let v = Volume::new();
    let remote = v.dir.path().join("remote.git");
    bare(&remote);
    let decoy = v.dir.path().join("decoy.git");
    bare(&decoy);
    let config = format!(
        "[url \"{}\"]\n\tinsteadOf = {}\n[url \"{}\"]\n\tpushInsteadOf = {}\n",
        decoy.display(),
        remote.display(),
        decoy.display(),
        remote.display()
    );
    fs::write(v.home().join(".gitconfig"), &config).unwrap();
    let xdg = v.home().join(".config/git");
    fs::create_dir_all(&xdg).unwrap();
    fs::write(xdg.join("config"), &config).unwrap();
    v.config(&[("remote.origin.url", &remote.display().to_string())]);
    v.record("a.txt", "a");

    let out = v.run(&["push"]);
    assert!(out.status.success(), "{}{}", stdout(&out), stderr(&out));
    assert_eq!(branch(&remote), Some(v.head()));
    assert_eq!(branch(&decoy), None);
}

#[test]
fn status_shows_remote_state() {
    let v = Volume::new();
    let missing = v.dir.path().join("missing.git");
    v.config(&[(
        "remote.origin.url",
        &format!("file://user:hunter2@{}", missing.display()),
    )]);
    let out = v.run(&["status"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stdout(&out).contains("last pushed never, no branch yet, 0 consecutive failure(s)"),
        "{}",
        stdout(&out)
    );

    v.record("a.txt", "a");
    let out = v.run(&["push"]);
    assert_eq!(out.status.code(), Some(1));
    let out = v.run(&["status"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    let line = text
        .lines()
        .find(|l| l.starts_with("remote:"))
        .unwrap_or_else(|| panic!("{text}"));
    assert!(line.contains("origin file://***@"), "{line}");
    assert!(!text.contains("hunter2"), "{text}");
    assert!(line.contains("last pushed never, 1 unpushed"), "{line}");
    assert!(line.contains("1 consecutive failure(s)"), "{line}");
    assert!(line.contains("last error (unreachable): "), "{line}");
}

/// SSH remotes that are misconfigured, or that this build cannot push to,
/// are reported and never stop the other remotes (design §11.1).
#[test]
fn ssh_remotes_are_validated() {
    let v = Volume::new();
    let good = v.dir.path().join("good.git");
    bare(&good);
    let key = v.dir.path().join("id");
    let known_hosts = v.dir.path().join("known_hosts");
    let (key, known_hosts) = (key.display().to_string(), known_hosts.display().to_string());
    v.config(&[
        ("remote.good.url", &good.display().to_string()),
        ("remote.nokh.url", "ssh://git@localhost:1/x.git"),
        ("remote.nokh.agentfsSshKey", &key),
        ("remote.token.url", "git@localhost:x.git"),
        ("remote.token.agentfsSshKey", &key),
        ("remote.token.agentfsKnownHosts", &known_hosts),
        ("remote.token.agentfsCredentials", &key),
        ("remote.full.url", "ssh://git@localhost:1/x.git"),
        ("remote.full.agentfsSshKey", &key),
        ("remote.full.agentfsKnownHosts", &known_hosts),
    ]);
    v.record("a.txt", "a");

    let out = v.run(&["push"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    let (text, err) = (stdout(&out), stderr(&out));
    assert!(
        text.contains(&format!("good: pushed none..{}", v.head())),
        "{text}"
    );
    assert_eq!(branch(&good), Some(v.head()));
    assert!(
        err.contains("remote.nokh.agentfsKnownHosts must be set for an ssh remote"),
        "{err}"
    );
    assert!(
        err.contains("ssh remotes authenticate with agentfsSshKey, not agentfsCredentials"),
        "{err}"
    );
    if cfg!(feature = "ssh") {
        // Valid, and nothing listens on port 1.
        assert!(text.contains("full: failed (unreachable)"), "{text}");
    } else {
        assert!(
            err.contains(
                "remote.full.url: ssh remotes need agentfs built with the `ssh` cargo feature"
            ),
            "{err}"
        );
        assert!(!text.contains("full:"), "{text}");
    }

    let out = v.run(&["status"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    for name in ["nokh", "token"] {
        assert!(
            text.contains(&format!("remote:    {name}: misconfigured, not pushed")),
            "{text}"
        );
    }
    assert_eq!(
        text.contains("remote:    full: misconfigured"),
        !cfg!(feature = "ssh"),
        "{text}"
    );
}

fn spawn_run(v: &Volume) -> Child {
    v.command(&["run", "--push-grace", "1s"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
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

/// Reads the child's stderr until `agentfs run` says it is running, so its
/// signal handlers are installed. Returns everything read.
fn wait_until_running(child: &mut Child) -> String {
    use std::io::BufRead as _;
    let mut reader = std::io::BufReader::new(child.stderr.take().unwrap());
    let mut seen = String::new();
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).unwrap();
        assert!(n > 0, "agentfs run exited early: {seen}");
        seen.push_str(&line);
        if line.starts_with("running volume") {
            // Keep draining in the background so the child never blocks.
            std::thread::spawn(move || std::io::copy(&mut reader, &mut std::io::sink()));
            return seen;
        }
    }
}

fn signal(child: &Child, sig: rustix::process::Signal) {
    rustix::process::kill_process(rustix::process::Pid::from_child(child), sig).unwrap();
}

#[test]
fn run_pushes_until_sigterm() {
    let v = Volume::new();
    let remote = v.dir.path().join("remote.git");
    bare(&remote);
    v.config(&[
        ("agentfs.pushInterval", "100ms"),
        ("remote.origin.url", &remote.display().to_string()),
    ]);
    v.record("a.txt", "a");

    let mut child = spawn_run(&v);
    wait_until_running(&mut child);
    let deadline = Instant::now() + Duration::from_secs(10);
    while branch(&remote) != Some(v.head()) {
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("the remote never got the head");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    signal(&child, rustix::process::Signal::TERM);
    let status = wait_with_timeout(&mut child, Duration::from_secs(10));
    let out = child.wait_with_output().unwrap();
    assert!(status.success(), "{status:?}: {}", stderr(&out));
    assert!(
        stdout(&out).contains("final commit: 0 record(s)"),
        "{}",
        stdout(&out)
    );
}

#[test]
fn run_commits_journaled_records_and_stops_on_sigint() {
    let v = Volume::new();
    // Journal a record without committing it, as after a crash: record
    // commits, so write through the library instead.
    v.config(&[("agentfs.commitWindow", "1h")]);
    fs::write(v.tree().join("late.txt"), "late").unwrap();
    {
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
            agentfs_core::record::Caller {
                uid: 1,
                gid: 1,
                pid: 1,
            },
            vec![agentfs_core::volume::Input::Write {
                path: agentfs_core::paths::RepoPath::new("late.txt").unwrap(),
                backing: v.tree().join("late.txt"),
                executable: false,
            }],
        )
        .unwrap();
    }
    let mut child = spawn_run(&v);
    // The open group stays open for an hour, so only the shutdown commits
    // it.
    let started = wait_until_running(&mut child);
    assert!(
        started.contains("1 uncommitted record(s) recovered"),
        "{started}"
    );
    assert_eq!(branch(&v.state().join("git")), None);
    signal(&child, rustix::process::Signal::INT);
    let status = wait_with_timeout(&mut child, Duration::from_secs(10));
    let out = child.wait_with_output().unwrap();
    assert!(status.success(), "{status:?}: {}", stderr(&out));
    assert!(
        stdout(&out).contains("final commit: 1 record(s) into 1 commit(s)"),
        "{}",
        stdout(&out)
    );
    assert!(branch(&v.state().join("git")).is_some());
}
