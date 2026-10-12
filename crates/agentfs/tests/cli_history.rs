//! `agentfs serve-history` and `record --history-service` from the command
//! line (design §14). No FUSE: fronts here are `record` commands, which
//! take the same path through the history service as a front mount.

use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::time::{Duration, Instant};

use agentfs_core::volume::StateDir;

struct Volume {
    dir: tempfile::TempDir,
}

fn command(state: &Path, tree: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_agentfs"));
    for (name, _) in std::env::vars_os() {
        let name_str = name.to_string_lossy();
        if name_str.starts_with("OTEL_") || name_str.starts_with("AGENTFS_") {
            cmd.env_remove(name);
        }
    }
    cmd.arg("--state").arg(state).arg("--root").arg(tree);
    cmd
}

impl Volume {
    fn new() -> Self {
        let v = Self {
            dir: tempfile::tempdir().unwrap(),
        };
        fs::create_dir_all(v.tree()).unwrap();
        let out = v.run(["init", "--volume", "demo"]);
        assert!(out.status.success(), "{}", stderr(&out));
        // Commit at once, so the history is there when a test looks.
        git2::Config::open(&v.state().git().join("config"))
            .unwrap()
            .set_str("agentfs.commitWindow", "10ms")
            .unwrap();
        v
    }

    fn state(&self) -> StateDir {
        StateDir::new(self.dir.path().join("state"))
    }

    fn tree(&self) -> PathBuf {
        self.dir.path().join("tree")
    }

    fn run<I, S>(&self, args: I) -> Output
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        command(self.state().root(), &self.tree())
            .args(args)
            .env("AGENTFS_REPLICA", "test-0")
            .output()
            .unwrap()
    }

    /// `record --history-service` as pod `pod`.
    fn front_record(&self, pod: &str, paths: &[&str]) -> Output {
        command(self.state().root(), &self.tree())
            .args(["record", "--history-service"])
            .arg(self.state().history_socket())
            .args(["--history-wait", "10s"])
            .args(paths)
            .env("AGENTFS_POD", pod)
            .env("AGENTFS_POD_UID", format!("{pod}-uid"))
            .env("AGENTFS_NAMESPACE", "agents")
            .env("AGENTFS_NODE", "node-1")
            .env("AGENTFS_REPLICA", pod)
            .output()
            .unwrap()
    }

    fn serve(&self) -> Service {
        let log = unique(self.dir.path());
        let child = command(self.state().root(), &self.tree())
            .arg("serve-history")
            .env("AGENTFS_REPLICA", "service-0")
            .stdout(fs::File::create(&log).unwrap())
            .stderr(fs::File::create(log.with_extension("err")).unwrap())
            .stdin(Stdio::null())
            .spawn()
            .unwrap();
        let service = Service {
            child: Some(child),
            log,
        };
        let deadline = Instant::now() + Duration::from_secs(20);
        while !service.log().contains("serving history for volume demo") {
            assert!(
                Instant::now() < deadline,
                "never served; log:\n{}",
                service.log()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        service
    }

    /// Every commit with a pod trailer, oldest first: the committer name
    /// and the `Agentfs-Pod` trailer.
    fn commits(&self) -> Vec<(String, String)> {
        let repo = git2::Repository::open_bare(self.state().git()).unwrap();
        let head = repo
            .find_reference("refs/heads/agentfs/demo")
            .unwrap()
            .peel_to_commit()
            .unwrap();
        let mut walk = repo.revwalk().unwrap();
        walk.push(head.id()).unwrap();
        walk.set_sorting(git2::Sort::TOPOLOGICAL | git2::Sort::REVERSE)
            .unwrap();
        walk.filter_map(|id| {
            let commit = repo.find_commit(id.unwrap()).unwrap();
            let pod = commit
                .message()
                .unwrap()
                .lines()
                .find_map(|l| l.strip_prefix("Agentfs-Pod: "))
                .map(str::to_owned)?;
            Some((commit.committer().name().unwrap().to_owned(), pod))
        })
        .collect()
    }
}

fn unique(dir: &Path) -> PathBuf {
    let mut n = 0;
    loop {
        let candidate = dir.join(format!("serve-{n}.log"));
        if !candidate.exists() {
            return candidate;
        }
        n += 1;
    }
}

/// A running `agentfs serve-history`.
struct Service {
    child: Option<Child>,
    log: PathBuf,
}

impl Service {
    /// stdout and stderr.
    fn log(&self) -> String {
        format!(
            "{}{}",
            fs::read_to_string(&self.log).unwrap_or_default(),
            fs::read_to_string(self.log.with_extension("err")).unwrap_or_default()
        )
    }

    fn signal(&self, signal: rustix::process::Signal) {
        let pid = rustix::process::Pid::from_child(self.child.as_ref().unwrap());
        rustix::process::kill_process(pid, signal).unwrap();
    }

    fn wait(&mut self) -> ExitStatus {
        let deadline = Instant::now() + Duration::from_secs(30);
        let child = self.child.as_mut().unwrap();
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                self.child = None;
                return status;
            }
            assert!(Instant::now() < deadline, "never exited:\n{}", self.log());
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn stop(mut self) -> String {
        self.signal(rustix::process::Signal::TERM);
        let status = self.wait();
        let log = self.log();
        assert!(status.success(), "exit {status:?}:\n{log}");
        log
    }

    fn kill(mut self) {
        self.signal(rustix::process::Signal::KILL);
        self.wait();
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn serve_history_needs_the_backing_tree() {
    let v = Volume::new();
    let out = Command::new(env!("CARGO_BIN_EXE_agentfs"))
        .arg("--state")
        .arg(v.state().root())
        .arg("serve-history")
        .env_remove("AGENTFS_ROOT")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains("serve-history needs --root"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn two_fronts_commit_with_their_own_committers() {
    let v = Volume::new();
    let service = v.serve();

    // The service holds the state area: single-process commands wait.
    fs::write(v.tree().join("x"), "x").unwrap();
    let out = v.run(["record", "x"]);
    assert_eq!(out.status.code(), Some(75), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("command=serve-history"),
        "{}",
        stderr(&out)
    );

    let dir = v.state().history_dir();
    let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&dir), 0o700);
    assert_eq!(mode(&v.state().history_socket()), 0o600);

    fs::write(v.tree().join("a"), "from a").unwrap();
    let out = v.front_record("pod-a", &["a"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stdout(&out).contains("recorded a as seq"),
        "{}",
        stdout(&out)
    );
    assert!(stdout(&out).contains("it commits them"), "{}", stdout(&out));
    fs::write(v.tree().join("b"), "from b").unwrap();
    let out = v.front_record("pod-b", &["b"]);
    assert!(out.status.success(), "{}", stderr(&out));

    let status = v.run(["status"]);
    let text = stdout(&status);
    assert!(
        text.contains("history:   serving fronts at unix:"),
        "{text}"
    );
    assert!(text.contains("fronts:    none"), "{text}");

    let log = service.stop();
    assert!(log.contains("final commit"), "{log}");
    assert_eq!(
        v.commits(),
        [
            ("pod-a".to_owned(), "pod-a".to_owned()),
            ("pod-b".to_owned(), "pod-b".to_owned()),
        ]
    );
    let status = v.run(["status"]);
    assert!(!stdout(&status).contains("history:"), "{}", stdout(&status));
}

#[test]
fn a_front_without_a_service_fails_and_names_it() {
    let v = Volume::new();
    fs::write(v.tree().join("a"), "1").unwrap();
    let start = Instant::now();
    let out = command(v.state().root(), &v.tree())
        .args(["record", "--history-service"])
        .arg(v.state().history_socket())
        .args(["--history-wait", "1s", "a"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(start.elapsed() < Duration::from_secs(10));
    let err = stderr(&out);
    assert!(err.contains("history.sock"), "{err}");
    assert!(err.contains("unreachable"), "{err}");
    assert!(err.contains("agentfs serve-history"), "{err}");

    let out = command(v.state().root(), &v.tree())
        .args(["record", "--history-service", "tcp:host:1", "a"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains("not supported yet"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn fronts_carry_on_after_the_service_is_killed_and_restarted() {
    let v = Volume::new();
    let service = v.serve();
    fs::write(v.tree().join("before"), "1").unwrap();
    let out = v.front_record("pod-a", &["before"]);
    assert!(out.status.success(), "{}", stderr(&out));
    service.kill();

    // The kernel released the lock; the stale socket is replaced.
    let service = v.serve();
    fs::write(v.tree().join("after"), "2").unwrap();
    let out = v.front_record("pod-a", &["after"]);
    assert!(out.status.success(), "{}", stderr(&out));
    service.stop();
    let commits = v.commits();
    assert_eq!(commits.len(), 2, "{commits:?}");
    assert!(commits.iter().all(|(c, _)| c == "pod-a"), "{commits:?}");
}
