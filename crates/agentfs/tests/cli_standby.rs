//! Standby history services, takeover and fencing (design §14.2), through
//! the binary: two `serve-history` on one state area, the second
//! `--standby`; fronts given `--history-service state` keep recording
//! across a handover. No FUSE, no root; the lock heartbeat is forced on
//! (`--lock-heartbeat on`) on a local filesystem. The heartbeat failure
//! test needs the `failpoints` feature.

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::time::{Duration, Instant};

use agentfs_core::history_service::Endpoint;
use agentfs_core::volume::StateDir;

struct Volume {
    dir: tempfile::TempDir,
}

fn command(state: &Path, tree: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_agentfs"));
    for (name, _) in std::env::vars_os() {
        let n = name.to_string_lossy();
        if n.starts_with("OTEL_") || n.starts_with("AGENTFS_") {
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
        let mut config = git2::Config::open(&v.state().git().join("config")).unwrap();
        config.set_str("agentfs.commitWindow", "10ms").unwrap();
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

    /// `record --history-service state` as `pod` on `node`.
    fn front_record(&self, pod: &str, node: &str, path: &str, content: &str) -> Output {
        fs::write(self.tree().join(path), content).unwrap();
        command(self.state().root(), &self.tree())
            .args(["record", "--history-service", "state"])
            .args(["--history-wait", "20s", "--history-timeout", "20s", path])
            .env("AGENTFS_POD", pod)
            .env("AGENTFS_NODE", node)
            .env("AGENTFS_REPLICA", pod)
            .output()
            .unwrap()
    }

    /// `serve-history` with `args`, its log in `name`.
    fn start(&self, name: &str, node: &str, args: &[&str], env: &[(&str, &str)]) -> Service {
        let log = self.dir.path().join(format!("{name}.log"));
        let file = fs::File::create(&log).unwrap();
        let mut cmd = command(self.state().root(), &self.tree());
        cmd.arg("serve-history")
            .args(args)
            .env("AGENTFS_REPLICA", name)
            .env("AGENTFS_NODE", node)
            .stdout(file.try_clone().unwrap())
            .stderr(file)
            .stdin(Stdio::null());
        for (k, v) in env {
            cmd.env(k, v);
        }
        Service {
            child: Some(cmd.spawn().unwrap()),
            log,
        }
    }

    fn head_blob(&self, path: &str) -> Option<String> {
        let repo = git2::Repository::open_bare(self.state().git()).ok()?;
        let tree = repo
            .find_reference("refs/heads/agentfs/demo")
            .ok()?
            .peel_to_tree()
            .ok()?;
        let entry = tree.get_path(Path::new(path)).ok()?;
        let blob = repo.find_blob(entry.id()).ok()?;
        Some(String::from_utf8_lossy(blob.content()).into_owned())
    }

    fn wait_head(&self, path: &str, content: &str) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while self.head_blob(path).as_deref() != Some(content) {
            assert!(
                Instant::now() < deadline,
                "history never had {path} = {content}"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

struct Service {
    child: Option<Child>,
    log: PathBuf,
}

impl Service {
    fn log(&self) -> String {
        fs::read_to_string(&self.log).unwrap_or_default()
    }

    fn wait_log(&self, text: &str) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while !self.log().contains(text) {
            assert!(
                Instant::now() < deadline,
                "never logged {text:?}:\n{}",
                self.log()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn signal(&self, signal: rustix::process::Signal) {
        let pid = rustix::process::Pid::from_child(self.child.as_ref().unwrap());
        rustix::process::kill_process(pid, signal).unwrap();
    }

    fn wait(&mut self) -> ExitStatus {
        let deadline = Instant::now() + Duration::from_secs(60);
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

const ACTIVE: &str = "serving history for volume demo";

#[test]
fn a_standby_takes_over_when_the_active_service_stops() {
    let v = Volume::new();
    let mut a = v.start("svc-a", "node-a", &["--lock-heartbeat", "on"], &[]);
    a.wait_log(ACTIVE);
    // Without --standby a second service exits 75.
    let out = v.run(["serve-history"]);
    assert_eq!(out.status.code(), Some(75), "{}", stderr(&out));

    let mut b = v.start(
        "svc-b",
        "node-b",
        &["--standby", "--lock-heartbeat", "on"],
        &[],
    );
    b.wait_log("standby for volume demo");
    assert!(b.log().contains("node=node-a"), "{}", b.log());
    assert!(!b.log().contains(ACTIVE));
    let endpoint = Endpoint::read(&v.state()).unwrap().unwrap();
    assert_eq!(endpoint.node, "node-a");
    let status = String::from_utf8_lossy(&v.run(["status"]).stdout).into_owned();
    assert!(status.contains("endpoint:  node node-a"), "{status}");

    let out = v.front_record("pod-1", "node-a", "one", "1");
    assert!(out.status.success(), "{}", stderr(&out));

    // A front starts while A stops and B takes over: it waits for the
    // handover rather than failing.
    a.signal(rustix::process::Signal::TERM);
    let front = {
        let tree = v.tree();
        let state = v.state();
        std::thread::spawn(move || {
            fs::write(tree.join("two"), "2").unwrap();
            command(state.root(), &tree)
                .args(["record", "--history-service", "state"])
                .args(["--history-wait", "20s", "--history-timeout", "20s", "two"])
                .env("AGENTFS_POD", "pod-2")
                .env("AGENTFS_NODE", "node-b")
                .env("AGENTFS_REPLICA", "pod-2")
                .output()
                .unwrap()
        })
    };
    assert!(a.wait().success(), "{}", a.log());
    assert!(a.log().contains("final commit"), "{}", a.log());
    b.wait_log("took over the state lock");
    b.wait_log(ACTIVE);
    let out = front.join().unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    let endpoint = Endpoint::read(&v.state()).unwrap().unwrap();
    assert_eq!(endpoint.node, "node-b");

    let out = v.front_record("pod-3", "node-b", "three", "3");
    assert!(out.status.success(), "{}", stderr(&out));
    for (p, c) in [("one", "1"), ("two", "2"), ("three", "3")] {
        v.wait_head(p, c);
    }
    b.signal(rustix::process::Signal::TERM);
    assert!(b.wait().success(), "{}", b.log());
    assert_eq!(Endpoint::read(&v.state()).unwrap(), None, "removed at stop");
}

#[test]
fn a_standby_recovers_what_a_killed_service_journaled() {
    let v = Volume::new();
    // A long commit window: records stay journaled, not committed.
    let mut config = git2::Config::open(&v.state().git().join("config")).unwrap();
    config.set_str("agentfs.commitWindow", "1h").unwrap();
    let mut a = v.start("svc-a", "node-a", &[], &[]);
    a.wait_log(ACTIVE);
    let mut b = v.start("svc-b", "node-b", &["--standby"], &[]);
    b.wait_log("standby for volume demo");
    let out = v.front_record("pod-1", "node-a", "kept", "journaled");
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(v.head_blob("kept"), None, "not committed yet");
    a.signal(rustix::process::Signal::KILL);
    a.wait();
    b.wait_log("took over the state lock");
    b.wait_log(ACTIVE);
    // Its endpoint replaced the dead one's.
    assert_eq!(Endpoint::read(&v.state()).unwrap().unwrap().node, "node-b");
    config.set_str("agentfs.commitWindow", "10ms").unwrap();
    b.signal(rustix::process::Signal::TERM);
    assert!(b.wait().success(), "{}", b.log());
    assert_eq!(v.head_blob("kept").as_deref(), Some("journaled"));
}

#[test]
fn a_standby_stops_on_a_signal_while_waiting() {
    let v = Volume::new();
    let a = v.start("svc-a", "node-a", &[], &[]);
    a.wait_log(ACTIVE);
    let mut b = v.start("svc-b", "node-b", &["--standby"], &[]);
    b.wait_log("standby for volume demo");
    b.signal(rustix::process::Signal::TERM);
    assert!(b.wait().success(), "{}", b.log());
    assert!(b.log().contains("stopped while waiting"), "{}", b.log());
    drop(a);
}

#[cfg(feature = "failpoints")]
#[test]
fn a_service_that_cannot_renew_its_lock_stops_writing_and_exits_75() {
    let v = Volume::new();
    // The third renewal fails, as a write to a lock the NFS server took
    // back would; the fence is short so the test is quick.
    let mut a = v.start(
        "svc-a",
        "node-a",
        &["--lock-heartbeat", "on", "--lock-fence", "1s"],
        &[(agentfs_core::failpoints::ENV_VAR, "lock.heartbeat=error@3")],
    );
    a.wait_log(ACTIVE);
    let status = a.wait();
    assert_eq!(status.code(), Some(75), "{}", a.log());
    let log = a.log();
    assert!(log.contains("lost the state lock"), "{log}");
    assert!(!log.contains("final commit:"), "no final commit: {log}");
    // A standby (or the next service) takes over.
    let mut b = v.start("svc-b", "node-b", &["--standby"], &[]);
    b.wait_log(ACTIVE);
    let out = v.front_record("pod-1", "node-b", "after", "x");
    assert!(out.status.success(), "{}", stderr(&out));
    v.wait_head("after", "x");
    b.signal(rustix::process::Signal::TERM);
    assert!(b.wait().success());
}
