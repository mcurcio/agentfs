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

/// Owner, 2026-10-17: "Remotes can come and go. It's not a error to have
/// zero remotes". A volume made with no remote is local-only, with no
/// alarm, and `agentfs push` has nothing to do. A remote added later gets
/// the whole branch with nothing else changed; with its section removed
/// again, the volume is local-only again.
#[test]
fn remotes_come_and_go() {
    let v = Volume::new();
    v.record("a.txt", "one\n");
    let local_only = |v: &Volume| {
        let out = v.run(&["status"]);
        assert!(out.status.success(), "{}", stderr(&out));
        assert!(
            stdout(&out).contains("remotes:   local-only (no remote)"),
            "{}",
            stdout(&out)
        );
        assert!(!stderr(&out).contains("alarm"), "{}", stderr(&out));
        let out = v.run(&["push"]);
        assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
        assert_eq!(stdout(&out).trim(), "local-only: nothing to push");
        assert!(!stderr(&out).contains("alarm"), "{}", stderr(&out));
    };
    local_only(&v);

    let remote = v.dir.path().join("r.git");
    bare(&remote);
    v.config(&[("remote.r.url", &remote.display().to_string())]);
    let out = v.run(&["push"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(branch(&remote), Some(v.head()));
    let out = v.run(&["status"]);
    assert!(
        stdout(&out).contains("remotes:   1 pushed"),
        "{}",
        stdout(&out)
    );
    assert!(!stderr(&out).contains("alarm"), "{}", stderr(&out));

    let mut config = git2::Config::open(&v.state().join("git/config")).unwrap();
    config.remove("remote.r.url").unwrap();
    drop(config);
    v.record("b.txt", "two\n");
    local_only(&v);
}

#[test]
fn push_with_nothing_committed_or_configured() {
    let v = Volume::new();
    let out = v.run(&["push"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(stdout(&out).trim(), "local-only: nothing to push");

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
        // Valid; the key is read before connecting, and there is none.
        assert!(text.contains("full: failed (auth)"), "{text}");
        assert!(text.contains("reading credentials"), "{text}");
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
    spawn_run_with(v, "1s")
}

fn spawn_run_with(v: &Volume, push_grace: &str) -> Child {
    v.command(&["run", "--push-grace", push_grace])
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

/// Journals a record of `name` without committing it, as after a crash:
/// `record` commits, so write through the library instead.
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

#[test]
fn run_commits_journaled_records_and_stops_on_sigint() {
    let v = Volume::new();
    v.config(&[("agentfs.commitWindow", "1h")]);
    journal(&v, "late.txt");
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

/// A volume whose remote gets nothing during the run: intervals of an
/// hour, and a record journaled before the start, so only the final
/// commit and the final push at shutdown can bring it to the remote.
fn idle_remote() -> (Volume, PathBuf) {
    let v = Volume::new();
    let remote = v.dir.path().join("remote.git");
    bare(&remote);
    v.config(&[
        ("agentfs.commitWindow", "1h"),
        ("agentfs.pushInterval", "1h"),
        ("remote.origin.url", &remote.display().to_string()),
    ]);
    journal(&v, "late.txt");
    (v, remote)
}

/// Stopping `run` pushes the final commit once (design §11.2).
#[test]
fn run_pushes_once_at_shutdown() {
    let (v, remote) = idle_remote();
    let mut child = spawn_run_with(&v, "30s");
    wait_until_running(&mut child);
    // The first push found no branch; nothing is committed yet.
    assert_eq!(branch(&remote), None);
    signal(&child, rustix::process::Signal::INT);
    let status = wait_with_timeout(&mut child, Duration::from_secs(30));
    let out = child.wait_with_output().unwrap();
    assert!(status.success(), "{status:?}: {}", stderr(&out));
    let text = stdout(&out);
    assert!(
        text.contains("final commit: 1 record(s) into 1 commit(s)"),
        "{text}"
    );
    let head = v.head();
    assert!(
        text.contains(&format!("final push: origin: pushed none..{head}")),
        "{text}"
    );
    assert_eq!(branch(&remote), Some(head));
}

/// `--push-grace 0s` makes the final commit and skips the final push.
#[test]
fn run_push_grace_zero_skips_final_push() {
    let (v, remote) = idle_remote();
    let mut child = spawn_run_with(&v, "0s");
    wait_until_running(&mut child);
    signal(&child, rustix::process::Signal::TERM);
    let status = wait_with_timeout(&mut child, Duration::from_secs(30));
    let out = child.wait_with_output().unwrap();
    assert!(status.success(), "{status:?}: {}", stderr(&out));
    let text = stdout(&out);
    assert!(
        text.contains("final commit: 1 record(s) into 1 commit(s)"),
        "{text}"
    );
    // Reported as skipped (no attempt started, so nothing is abandoned);
    // never pushed.
    assert!(
        text.contains("final push: origin: skipped, push grace spent (1 commit(s)"),
        "{text}"
    );
    assert!(!text.contains("still running"), "{text}");
    assert_eq!(branch(&remote), None);
}

/// `agentfs push` asks a running `run` to commit and push now, past its
/// push interval (design §2.3, §11.2).
#[test]
fn push_now_reaches_a_running_run() {
    let (v, remote) = idle_remote();
    let mut child = spawn_run_with(&v, "1s");
    wait_until_running(&mut child);
    assert_eq!(branch(&remote), None);

    let out = v.run(&["push", "--timeout", "30s"]);
    let text = stdout(&out);
    assert!(out.status.success(), "{text}{}", stderr(&out));
    assert!(text.contains("through the running daemon (pid "), "{text}");
    assert!(text.contains("1 record(s) into 1 commit(s)"), "{text}");
    let head = v.head();
    assert!(
        text.contains(&format!("origin: pushed none..{head}")),
        "{text}"
    );
    assert_eq!(branch(&remote), Some(head));

    // A remote the daemon does not know is refused, and the daemon runs on.
    let out = v.run(&["push", "--remote", "nope"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    let err = stderr(&out);
    assert!(err.contains("push refused: "), "{err}");
    assert!(err.contains("nope"), "{err}");
    assert!(err.contains("configured remotes: origin"), "{err}");

    signal(&child, rustix::process::Signal::TERM);
    let status = wait_with_timeout(&mut child, Duration::from_secs(30));
    let out = child.wait_with_output().unwrap();
    assert!(status.success(), "{status:?}");
    let text = stdout(&out);
    assert!(text.contains("final push: origin: nothing new"), "{text}");
}

/// Without a daemon, `agentfs push` pushes directly, as before.
#[test]
fn push_without_a_daemon_pushes_directly() {
    let (v, remote) = idle_remote();
    let out = v.run(&["record", "late.txt"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let out = v.run(&["push", "--timeout", "5s"]);
    let text = stdout(&out);
    assert!(out.status.success(), "{text}{}", stderr(&out));
    assert!(!text.contains("through the running daemon"), "{text}");
    assert_eq!(branch(&remote), Some(v.head()));
}

/// Incompressible bytes: a xorshift stream.
fn random(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

/// A volume with a 1 MiB push size limit on `origin` and five commits of
/// 400 KiB of random data each.
fn limited_backlog() -> (Volume, PathBuf) {
    let v = Volume::new();
    let remote = v.dir.path().join("remote.git");
    bare(&remote);
    v.config(&[
        ("agentfs.pushInterval", "1h"),
        ("remote.origin.url", &remote.display().to_string()),
        ("remote.origin.agentfsMaxPushSize", "1m"),
    ]);
    for i in 0..5 {
        let name = format!("big{i}");
        fs::write(v.tree().join(&name), random(400 << 10, i + 1)).unwrap();
        let out = v.run(&["record", &name]);
        assert!(out.status.success(), "{}", stderr(&out));
    }
    (v, remote)
}

/// A backlog over `agentfsMaxPushSize` is pushed in steps until the remote
/// is up to date; a single commit over it fails as `too_large` (design
/// §11.3).
#[test]
fn push_steps_through_a_push_size_limit() {
    let (v, remote) = limited_backlog();
    let out = v.run(&["push"]);
    let text = stdout(&out);
    assert!(out.status.success(), "{text}{}", stderr(&out));
    let steps = text
        .lines()
        .filter(|l| l.contains("(push size limit: "))
        .count();
    assert_eq!(steps, 2, "{text}");
    assert!(text.contains("1 commit(s) left"), "{text}");
    let head = v.head();
    assert!(
        text.lines().last().unwrap().ends_with(&head.to_string()),
        "{text}"
    );
    assert_eq!(branch(&remote), Some(head));
    assert!(
        stderr(&out).contains("alarm: push to origin limited to 1048576 bytes"),
        "{}",
        stderr(&out)
    );

    // One commit alone over the limit: too_large, exit 1, nothing sent.
    fs::write(v.tree().join("huge"), random(2 << 20, 99)).unwrap();
    let out = v.run(&["record", "huge"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let out = v.run(&["push"]);
    assert_eq!(out.status.code(), Some(1), "{}", stdout(&out));
    let text = stdout(&out);
    assert!(
        text.contains("origin: failed (too_large): commit "),
        "{text}"
    );
    assert!(text.contains("agentfsMaxPushSize"), "{text}");
    assert_eq!(branch(&remote), Some(head));

    // Raising the limit lets it through.
    v.config(&[("remote.origin.agentfsMaxPushSize", "8m")]);
    let out = v.run(&["push"]);
    assert!(out.status.success(), "{}", stdout(&out));
    assert_eq!(branch(&remote), Some(v.head()));
}

/// Through a running daemon, `agentfs push` also ends up to date.
#[test]
fn push_now_steps_through_a_push_size_limit() {
    let (v, remote) = limited_backlog();
    let mut child = spawn_run_with(&v, "1s");
    wait_until_running(&mut child);
    let out = v.run(&["push", "--timeout", "60s"]);
    let text = stdout(&out);
    assert!(out.status.success(), "{text}{}", stderr(&out));
    assert!(text.contains("through the running daemon (pid "), "{text}");
    let head = v.head();
    assert!(
        text.lines()
            .last()
            .is_some_and(|l| l.contains(&head.to_string())),
        "{text}"
    );
    assert_eq!(branch(&remote), Some(head));
    signal(&child, rustix::process::Signal::TERM);
    let status = wait_with_timeout(&mut child, Duration::from_secs(30));
    assert!(status.success(), "{status:?}");
}

/// Another writer's commit on top of the remote branch, written in the
/// remote: the volume's state area never has it.
fn foreign_commit(remote: &Path) -> git2::Oid {
    let repo = git2::Repository::open_bare(remote).unwrap();
    let parent = repo
        .find_commit(repo.refname_to_id("refs/heads/agentfs/demo").unwrap())
        .unwrap();
    let sig = git2::Signature::now("intruder", "i@example.invalid").unwrap();
    let tree = parent.tree().unwrap();
    repo.commit(
        Some("refs/heads/agentfs/demo"),
        &sig,
        &sig,
        "a foreign commit",
        &tree,
        &[&parent],
    )
    .unwrap()
}

/// F8 S2, acceptance 3: `agentfs push --force-with-lease` (the divergence
/// and history-rewrite runbooks) replaces the volume's own branch only
/// with the right lease, fetches a lease it lacks, clears the divergence,
/// and never replaces an unrelated history.
#[test]
fn force_with_lease_replaces_only_the_leased_branch() {
    let v = Volume::new();
    let remote = v.dir.path().join("remote.git");
    bare(&remote);
    v.config(&[("remote.origin.url", &remote.display().to_string())]);
    v.record("a.txt", "a");
    assert!(v.run(&["push"]).status.success());
    let theirs = foreign_commit(&remote);
    v.record("b.txt", "b");
    let out = v.run(&["push"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stdout(&out).contains("diverged"), "{}", stdout(&out));
    let head = v.head();

    // Not a full commit id.
    let out = v.run(&["push", "--force-with-lease", "HEAD"]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("not a full commit id"),
        "{}",
        stderr(&out)
    );

    // A wrong lease: nothing written, exit 1, the remote's value printed.
    let out = v.run(&["push", "--force-with-lease", &head.to_string()]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(
        stdout(&out).contains(&format!("origin: lease broken: the remote holds {theirs}")),
        "{}",
        stdout(&out)
    );
    assert_eq!(branch(&remote), Some(theirs));

    // The right lease, which the state area lacks: fetched aside, then the
    // branch is the head again and the divergence is cleared.
    let out = v.run(&["push", "--force-with-lease", &theirs.to_string()]);
    assert!(out.status.success(), "{}{}", stdout(&out), stderr(&out));
    assert_eq!(
        stdout(&out).trim(),
        format!("origin: replaced {theirs} with {head} (leased on {theirs})")
    );
    assert_eq!(branch(&remote), Some(head));
    let local = git2::Repository::open_bare(v.state().join("git")).unwrap();
    assert!(
        local.find_commit(theirs).is_err(),
        "the lease was not taken in"
    );
    let status = stdout(&v.run(&["status"]));
    assert!(!status.contains("diverged"), "{status}");
    let out = v.run(&["push", "--force-with-lease", &theirs.to_string()]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(stdout(&out).trim(), format!("origin: up to date at {head}"));

    // An unrelated history: refused, exit 1.
    let other = v.dir.path().join("other.git");
    let repo = git2::Repository::init_bare(&other).unwrap();
    let sig = git2::Signature::now("o", "o@example.invalid").unwrap();
    let tree = repo.treebuilder(None).unwrap().write().unwrap();
    let tree = repo.find_tree(tree).unwrap();
    let unrelated = repo.commit(None, &sig, &sig, "x", &tree, &[]).unwrap();
    repo.remote_anonymous(&remote.display().to_string())
        .unwrap()
        .push(&[format!("+{unrelated}:refs/heads/agentfs/demo")], None)
        .unwrap();
    let out = v.run(&["push", "--force-with-lease", &unrelated.to_string()]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(
        stdout(&out).contains("shares no commit"),
        "{}",
        stdout(&out)
    );
    assert_eq!(branch(&remote), Some(unrelated));
}

/// Divergence case B (bead agentfs-4lb.2): the remote's branch holds this
/// volume's newer history, recorded from another copy of its state area.
/// `--force-with-lease` refuses it, exit 1, and the branch stays.
#[test]
fn force_with_lease_refuses_history_agentfs_recorded() {
    let v = Volume::new();
    let remote = v.dir.path().join("remote.git");
    bare(&remote);
    v.config(&[("remote.origin.url", &remote.display().to_string())]);
    v.record("a.txt", "a");
    assert!(v.run(&["push"]).status.success());
    let repo = git2::Repository::open_bare(&remote).unwrap();
    let parent = repo
        .find_commit(repo.refname_to_id("refs/heads/agentfs/demo").unwrap())
        .unwrap();
    let sig = git2::Signature::now("agent", "agent@example.invalid").unwrap();
    let newer = repo
        .commit(
            Some("refs/heads/agentfs/demo"),
            &sig,
            &sig,
            "close c.txt\n\nAgentfs-Seq: 99\nAgentfs-Trigger: close\n",
            &parent.tree().unwrap(),
            &[&parent],
        )
        .unwrap();
    v.record("b.txt", "b");
    assert_eq!(v.run(&["push"]).status.code(), Some(1));

    let out = v.run(&["push", "--force-with-lease", &newer.to_string()]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(
        stdout(&out).contains(&format!(
            "origin: refused: the remote's branch holds {newer}, which carries agentfs trailers"
        )),
        "{}",
        stdout(&out)
    );
    assert_eq!(branch(&remote), Some(newer));
}

/// The lease push needs exactly one remote, and never goes through a
/// running daemon.
#[test]
fn force_with_lease_needs_one_remote_and_bypasses_the_daemon() {
    let v = Volume::new();
    let one = v.dir.path().join("one.git");
    let two = v.dir.path().join("two.git");
    bare(&one);
    bare(&two);
    v.config(&[
        ("remote.one.url", &one.display().to_string()),
        ("remote.two.url", &two.display().to_string()),
    ]);
    v.record("a.txt", "a");
    let head = v.head().to_string();
    let out = v.run(&["push", "--force-with-lease", &head]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("needs --remote NAME"),
        "{}",
        stderr(&out)
    );
    let out = v.run(&[
        "push",
        "--force-with-lease",
        &head,
        "--remote",
        "one",
        "--remote",
        "two",
    ]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("one --remote"), "{}", stderr(&out));

    // With `run` holding the state lock, the lease push still runs, alone.
    assert!(v.run(&["push", "--remote", "one"]).status.success());
    let mut child = spawn_run_with(&v, "0s");
    wait_until_running(&mut child);
    // `run`'s own first push may hold the remote's push lock: exit 75.
    let mut out = v.run(&["push", "--remote", "one", "--force-with-lease", &head]);
    for _ in 0..50 {
        if out.status.code() != Some(75) {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
        out = v.run(&["push", "--remote", "one", "--force-with-lease", &head]);
    }
    let text = stdout(&out);
    assert!(out.status.success(), "{text}{}", stderr(&out));
    assert_eq!(text.trim(), format!("one: up to date at {head}"));
    signal(&child, rustix::process::Signal::TERM);
    let status = wait_with_timeout(&mut child, Duration::from_secs(30));
    assert!(status.success(), "{status:?}");
}
