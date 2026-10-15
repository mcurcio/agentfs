//! `agentfs serve-history` and `record --history-service` from the command
//! line (design §14). No FUSE: fronts here are `record` commands, which
//! take the same path through the history service as a front mount.

use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agentfs_core::events::NullSink;
use agentfs_core::history_service::protocol::FrontInfo;
use agentfs_core::history_service::{
    Address, ClientConfig, FrontHooks, FrontLease, HistoryClient, ListenerHandle,
};
use agentfs_core::paths::RepoPath;
use agentfs_core::record::Origin;
use agentfs_core::telemetry::Telemetry;
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

#[test]
fn a_front_prefers_the_slot_it_is_given() {
    let v = Volume::new();
    let service = v.serve();
    let front = |slot: &str, path: &str| {
        fs::write(v.tree().join(path), path).unwrap();
        command(v.state().root(), &v.tree())
            .args(["record", "--history-service"])
            .arg(v.state().history_socket())
            .args(["--history-wait", "10s", "--history-slot", slot, path])
            .env("AGENTFS_POD", "pod-a")
            .output()
            .unwrap()
    };
    let out = front("7", "a");
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("(slot 7)"), "{}", stdout(&out));
    let holder = fs::read_to_string(v.state().front_lease(7)).unwrap();
    assert!(holder.contains("command=record pod=pod-a"), "{holder}");

    // A held preferred slot falls back to the lowest free one.
    let held =
        agentfs_core::history_service::FrontLease::acquire_preferring(&v.state(), "test", Some(7))
            .unwrap();
    assert_eq!(held.slot(), 7);
    let out = front("7", "b");
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("(slot 1)"), "{}", stdout(&out));

    // The environment variable works too, and 0 is refused.
    let out = command(v.state().root(), &v.tree())
        .args(["record", "--history-service"])
        .arg(v.state().history_socket())
        .args(["--history-slot", "0", "a"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    fs::write(v.tree().join("c"), "c").unwrap();
    let out = command(v.state().root(), &v.tree())
        .args(["record", "--history-service"])
        .arg(v.state().history_socket())
        .args(["--history-wait", "10s", "c"])
        .env("AGENTFS_HISTORY_SLOT", "9")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("(slot 9)"), "{}", stdout(&out));
    drop(held);
    service.stop();
}

/// The history service answers `agentfs push` over the control socket
/// (design §2.3).
#[test]
fn the_history_service_pushes_now() {
    let v = Volume::new();
    let remote = v.dir.path().join("remote.git");
    git2::Repository::init_bare(&remote).unwrap();
    let mut config = git2::Config::open(&v.state().git().join("config")).unwrap();
    config.set_str("agentfs.pushInterval", "1h").unwrap();
    config
        .set_str("remote.origin.url", &remote.display().to_string())
        .unwrap();
    let service = v.serve();

    fs::write(v.tree().join("a"), "from a").unwrap();
    let out = v.front_record("pod-a", &["a"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let out = v.run(["push", "--timeout", "30s"]);
    let text = stdout(&out);
    assert!(out.status.success(), "{text}{}", stderr(&out));
    assert!(text.contains("through the running daemon (pid "), "{text}");
    assert!(text.contains("origin: pushed none.."), "{text}");
    let pushed = git2::Repository::open_bare(&remote)
        .unwrap()
        .refname_to_id("refs/heads/agentfs/demo")
        .unwrap();
    let local = git2::Repository::open_bare(v.state().git())
        .unwrap()
        .refname_to_id("refs/heads/agentfs/demo")
        .unwrap();
    assert_eq!(pushed, local);

    let log = service.stop();
    assert!(log.contains("final push: origin: nothing new"), "{log}");
}

/// A front in this process that listens for the service's calls: `open`
/// paths answer open for writing; invalidations are noted.
struct TestFront {
    lease: FrontLease,
    hooks: Arc<Noting>,
    listener: ListenerHandle,
}

#[derive(Default)]
struct Noting {
    open: Mutex<Vec<String>>,
    /// `(file, content, path)`: `path` is open for writing once `file`
    /// holds `content` (a pod opening it while a restore runs).
    open_when: Mutex<Option<(PathBuf, String, String)>>,
    settles: Mutex<usize>,
    settle_error: Mutex<Option<String>>,
    invalidated: Mutex<Vec<String>>,
}

impl FrontHooks for Noting {
    fn settle(&self) -> Result<(usize, usize), String> {
        *self.settles.lock().unwrap() += 1;
        match self.settle_error.lock().unwrap().clone() {
            Some(e) => Err(e),
            None => Ok((0, 0)),
        }
    }

    fn open_for_writing(&self, paths: &[RepoPath]) -> Vec<bool> {
        let open = self.open.lock().unwrap();
        let late = self
            .open_when
            .lock()
            .unwrap()
            .clone()
            .and_then(|(file, content, path)| {
                (fs::read_to_string(file).ok().as_deref() == Some(content.as_str())).then_some(path)
            });
        paths
            .iter()
            .map(|p| open.contains(&p.to_string()) || late.as_deref() == Some(p.as_str()))
            .collect()
    }

    fn invalidate(&self, paths: &[RepoPath]) -> Result<(), String> {
        let mut noted = self.invalidated.lock().unwrap();
        noted.extend(paths.iter().map(ToString::to_string));
        Ok(())
    }
}

impl Volume {
    fn listening_front(&self, pod: &str) -> TestFront {
        let state = self.state();
        let lease = FrontLease::acquire(&state, &format!("command=test pod={pod}")).unwrap();
        let client = HistoryClient::connect(
            ClientConfig {
                address: Address::Unix(state.history_socket()),
                timeout: Duration::from_secs(10),
                connect_wait: Duration::from_secs(10),
            },
            "demo",
            "refs/heads/agentfs/demo",
            FrontInfo {
                origin: Origin {
                    namespace: "agents".into(),
                    pod: pod.into(),
                    pod_uid: format!("{pod}-uid"),
                    node: "node-1".into(),
                    replica: pod.into(),
                },
                slot: lease.slot(),
                instance: u64::from(std::process::id()) << 16 | u64::from(lease.slot()),
                pid: std::process::id(),
                version: "test".into(),
                views: Vec::new(),
                tree_dev: None,
            },
            Arc::new(NullSink),
            Telemetry::noop(),
        )
        .unwrap();
        let hooks = Arc::new(Noting::default());
        let listener = Arc::new(client).listen(hooks.clone()).unwrap();
        assert!(listener.wait_listening(Duration::from_secs(10)));
        TestFront {
            lease,
            hooks,
            listener,
        }
    }

    fn head_has(&self, path: &str) -> Option<String> {
        let repo = git2::Repository::open_bare(self.state().git()).ok()?;
        let head = repo
            .find_reference("refs/heads/agentfs/demo")
            .ok()?
            .peel_to_commit()
            .ok()?;
        let entry = head.tree().ok()?.get_path(Path::new(path)).ok()?;
        let blob = repo.find_blob(entry.id()).ok()?;
        Some(String::from_utf8_lossy(blob.content()).into_owned())
    }

    /// The branch head's commit id.
    fn head_id(&self) -> String {
        let repo = git2::Repository::open_bare(self.state().git()).unwrap();
        repo.refname_to_id("refs/heads/agentfs/demo")
            .unwrap()
            .to_string()
    }

    fn wait_head(&self, path: &str, content: &str) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while self.head_has(path).as_deref() != Some(content) {
            assert!(
                Instant::now() < deadline,
                "HEAD never had {path} = {content}: {:?}",
                self.head_has(path)
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn head_message(&self) -> String {
        let repo = git2::Repository::open_bare(self.state().git()).unwrap();
        let head = repo
            .find_reference("refs/heads/agentfs/demo")
            .unwrap()
            .peel_to_commit()
            .unwrap();
        head.message().unwrap().to_owned()
    }
}

/// `restore` and `revert` while the history service runs (design §15):
/// served by the service, which writes the backing tree, records it as
/// the operation and commits, with no front, and with a listening front
/// it settles, asks about open files and tells to drop its caches.
#[test]
fn the_history_service_restores_and_reverts_live() {
    let v = Volume::new();
    let service = v.serve();
    for content in ["one", "two"] {
        fs::write(v.tree().join("a"), content).unwrap();
        let out = v.front_record("pod-a", &["a"]);
        assert!(out.status.success(), "{}", stderr(&out));
        v.wait_head("a", content);
    }

    // No front runs: written straight into the backing tree.
    let out = v.run(["restore", ".", "HEAD~1"]);
    assert!(out.status.success(), "{}{}", stdout(&out), stderr(&out));
    assert!(
        stdout(&out).contains("restored 1 path(s)"),
        "{}",
        stdout(&out)
    );
    assert_eq!(fs::read_to_string(v.tree().join("a")).unwrap(), "one");
    assert_eq!(v.head_has("a").as_deref(), Some("one"));
    assert!(
        v.head_message().contains("Agentfs-Trigger: restore"),
        "{}",
        v.head_message()
    );
    let out = v.run(["revert", "HEAD"]);
    assert!(out.status.success(), "{}{}", stdout(&out), stderr(&out));
    assert_eq!(fs::read_to_string(v.tree().join("a")).unwrap(), "two");
    assert_eq!(v.head_has("a").as_deref(), Some("two"));

    // A listening front holding `a` open for writing: refused, nothing
    // written; with --force, written, and the front drops its caches.
    let front = v.listening_front("pod-b");
    front.hooks.open.lock().unwrap().push("a".into());
    let out = v.run(["restore", "a", "HEAD~1"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("open for writing"),
        "{}",
        stderr(&out)
    );
    assert_eq!(fs::read_to_string(v.tree().join("a")).unwrap(), "two");
    assert!(*front.hooks.settles.lock().unwrap() >= 1);
    let out = v.run(["restore", "--force", "a", "HEAD~1"]);
    assert!(out.status.success(), "{}{}", stdout(&out), stderr(&out));
    assert_eq!(fs::read_to_string(v.tree().join("a")).unwrap(), "one");
    assert_eq!(*front.hooks.invalidated.lock().unwrap(), ["a"]);
    // A dry run checks the same way, and writes nothing.
    front.hooks.open.lock().unwrap().clear();
    let out = v.run(["revert", "--dry-run", "HEAD"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("dry run"), "{}", stdout(&out));
    assert_eq!(fs::read_to_string(v.tree().join("a")).unwrap(), "one");

    // A live front that stops listening: busy (75) once the grace is over.
    let TestFront {
        lease, listener, ..
    } = front;
    listener.stop();
    let started = Instant::now();
    let out = v.run(["restore", "a", "HEAD~1"]);
    assert_eq!(out.status.code(), Some(75), "{}", stderr(&out));
    assert!(stderr(&out).contains("not listening"), "{}", stderr(&out));
    assert!(started.elapsed() < Duration::from_secs(15));
    assert_eq!(fs::read_to_string(v.tree().join("a")).unwrap(), "one");
    drop(lease);
    let out = v.run(["restore", "a", "HEAD~1"]);
    assert!(out.status.success(), "{}{}", stdout(&out), stderr(&out));
    service.stop();
}

/// A file a pod opens for writing while a live restore is already writing
/// (after the check) is refused at its own step: the service asks the
/// fronts again just before writing each path (design §15).
#[test]
fn a_file_opened_during_a_live_restore_is_left_alone() {
    let v = Volume::new();
    let service = v.serve();
    // Each path is its own record, maybe its own commit: the restores
    // name the commit where both were "one", not `HEAD~n`.
    let mut first = None;
    for content in ["one", "two"] {
        fs::write(v.tree().join("a"), content).unwrap();
        fs::write(v.tree().join("z"), content).unwrap();
        let out = v.front_record("pod-a", &["a", "z"]);
        assert!(out.status.success(), "{}", stderr(&out));
        v.wait_head("a", content);
        v.wait_head("z", content);
        first.get_or_insert_with(|| v.head_id());
    }
    let first = first.unwrap();
    let front = v.listening_front("pod-b");
    // `z` is opened once `a` has been restored: after the check, before
    // `z`'s own step.
    *front.hooks.open_when.lock().unwrap() = Some((v.tree().join("a"), "one".into(), "z".into()));
    let out = v.run(["restore", ".", &first]);
    assert_eq!(
        out.status.code(),
        Some(1),
        "{}{}",
        stdout(&out),
        stderr(&out)
    );
    assert!(
        stderr(&out).contains("opened for writing"),
        "{}",
        stderr(&out)
    );
    assert_eq!(fs::read_to_string(v.tree().join("a")).unwrap(), "one");
    assert_eq!(fs::read_to_string(v.tree().join("z")).unwrap(), "two");
    // What was written is recorded, and the front drops its caches of it.
    v.wait_head("a", "one");
    assert_eq!(v.head_has("z").as_deref(), Some("two"));
    assert_eq!(*front.hooks.invalidated.lock().unwrap(), ["a"]);
    // A dry run settles the fronts too: what it checks is what a real run
    // would.
    let settles = *front.hooks.settles.lock().unwrap();
    let out = v.run(["revert", "--dry-run", "HEAD"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(*front.hooks.settles.lock().unwrap() > settles);
    // A front that cannot settle: busy (75), nothing written.
    *front.hooks.settle_error.lock().unwrap() = Some("sweep failed".into());
    let out = v.run(["restore", "z", &first]);
    assert_eq!(out.status.code(), Some(75), "{}", stderr(&out));
    assert!(stderr(&out).contains("sweep failed"), "{}", stderr(&out));
    assert_eq!(fs::read_to_string(v.tree().join("z")).unwrap(), "two");
    drop(front);
    service.stop();
}
