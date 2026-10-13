//! Crash injection in a real FUSE mount (design §18 test 3; the crash
//! model of docs/architecture.md): the daemon killed (SIGKILL to itself)
//! after a close was recorded but before FLUSH replied, and while saving
//! the repair queue of a rename whose record failed. The stale mount is
//! detached and the next command recovers; nothing journaled is lost and
//! the repair is made.
//!
//! Built only with the `failpoints` feature, and needs `/dev/fuse` and
//! root, so every test is `#[ignore]`d; the `crash` CI job runs them
//! through `ci/crash-tests.sh`:
//!
//! ```sh
//! cargo test -p agentfs --features failpoints --test crash_mount --no-run
//! sudo target/debug/deps/crash_mount-<hash> --ignored --test-threads=1
//! ```

#![cfg(feature = "failpoints")]

#[path = "../../agentfs-core/tests/support/crash_oracle.rs"]
mod crash_oracle;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use agentfs_core::failpoints::ENV_VAR;

const BRANCH: &str = "refs/heads/agentfs/demo";

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn is_mounted(mnt: &Path) -> bool {
    let info = fs::read_to_string("/proc/self/mountinfo").unwrap();
    let mnt = mnt.to_str().unwrap();
    info.lines().any(|l| l.split(' ').nth(4) == Some(mnt))
}

/// A volume: `backing/`, `state/` and `mnt/` side by side.
struct Vol {
    dir: tempfile::TempDir,
}

impl Vol {
    fn new() -> Self {
        assert!(
            rustix::process::geteuid().is_root(),
            "the mount crash tests run as root (ci/crash-tests.sh)"
        );
        let dir = tempfile::tempdir().unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).unwrap();
        let v = Self { dir };
        fs::create_dir_all(v.backing()).unwrap();
        fs::create_dir_all(v.mnt()).unwrap();
        v.ok("", &["init", "--volume", "demo"]);
        let mut c = git2::Config::open(&v.state().join("git/config")).unwrap();
        c.set_str("agentfs.commitWindow", "50ms").unwrap();
        v
    }

    fn backing(&self) -> PathBuf {
        self.dir.path().join("backing")
    }

    fn mnt(&self) -> PathBuf {
        self.dir.path().join("mnt")
    }

    fn state(&self) -> PathBuf {
        self.dir.path().join("state")
    }

    fn log(&self) -> PathBuf {
        self.dir.path().join("mount.log")
    }

    fn command(&self, spec: &str) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_agentfs"));
        for (name, _) in std::env::vars_os() {
            let n = name.to_string_lossy();
            if n.starts_with("OTEL_") || n.starts_with("AGENTFS_") {
                cmd.env_remove(name);
            }
        }
        cmd.arg("--state")
            .arg(self.state())
            .arg("--root")
            .arg(self.backing())
            .env("AGENTFS_REPLICA", "test-0")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null");
        if !spec.is_empty() {
            cmd.env(ENV_VAR, spec);
        }
        cmd
    }

    fn ok(&self, spec: &str, args: &[&str]) -> String {
        let out: Output = self.command(spec).args(args).output().unwrap();
        assert!(
            out.status.success(),
            "agentfs {args:?}: {}{}",
            text(&out.stdout),
            text(&out.stderr)
        );
        text(&out.stdout)
    }

    fn mount(&self, spec: &str) -> Child {
        let log = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.log())
            .unwrap();
        let child = self
            .command(spec)
            .args(["mount", "--unmount-grace", "3s"])
            .arg(self.backing())
            .arg(self.mnt())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .stdin(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        while !is_mounted(&self.mnt()) {
            assert!(
                Instant::now() < deadline,
                "never mounted:\n{}",
                self.log_text()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        child
    }

    fn log_text(&self) -> String {
        fs::read_to_string(self.log()).unwrap_or_default()
    }

    /// Runs `script` in the mount; its exit status.
    fn sh(&self, script: &str) -> std::process::ExitStatus {
        Command::new("sh")
            .arg("-c")
            .arg(script)
            .current_dir(self.mnt())
            .status()
            .unwrap()
    }

    /// Waits for a daemon killed by its failpoint, then detaches the stale
    /// mount it left (as `agentfs probe` would report it).
    fn reap_killed(&self, mut child: Child, point: &str) {
        let deadline = Instant::now() + Duration::from_secs(30);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            assert!(
                Instant::now() < deadline,
                "never died:\n{}",
                self.log_text()
            );
            std::thread::sleep(Duration::from_millis(50));
        };
        assert_eq!(status.signal(), Some(9), "{}", self.log_text());
        assert!(
            self.log_text()
                .contains(&format!("agentfs failpoint {point} triggered (kill)")),
            "{}",
            self.log_text()
        );
        let out = Command::new("umount")
            .arg("-l")
            .arg(self.mnt())
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", text(&out.stderr));
    }

    fn unmount(&self, mut child: Child) {
        let pid = rustix::process::Pid::from_child(&child);
        rustix::process::kill_process(pid, rustix::process::Signal::TERM).unwrap();
        let status = child.wait().unwrap();
        assert!(status.success(), "{status:?}:\n{}", self.log_text());
    }

    fn committed(&self, name: &str) -> Option<String> {
        let repo = git2::Repository::open_bare(self.state().join("git")).unwrap();
        let head = repo.find_reference(BRANCH).ok()?.peel_to_commit().ok()?;
        let entry = head.tree().ok()?.get_path(Path::new(name)).ok()?;
        Some(text(repo.find_blob(entry.id()).ok()?.content()))
    }
}

/// Killed after a close was journaled, before FLUSH replied: the record
/// was durable, so the next command commits it.
#[test]
#[ignore = "needs /dev/fuse and root; run by ci/crash-tests.sh"]
fn killed_after_a_close_was_recorded() {
    let v = Vol::new();
    let child = v.mount("fuse.after_record=kill@1");
    // The close fails (the daemon is gone); what it recorded stays.
    let _ = v.sh("printf 'x\\n' > a.txt");
    v.reap_killed(child, "fuse.after_record");
    v.ok("", &["flush"]);
    assert_eq!(v.committed("a.txt").as_deref(), Some("x\n"));
    crash_oracle::check_clean(&v.state(), BRANCH).unwrap();

    // The next mount serves the file and records the next change.
    let child = v.mount("");
    assert_eq!(fs::read_to_string(v.mnt().join("a.txt")).unwrap(), "x\n");
    assert!(v.sh("printf 'y\\n' > a.txt").success());
    v.unmount(child);
    assert_eq!(v.committed("a.txt").as_deref(), Some("y\n"));
    crash_oracle::check_clean(&v.state(), BRANCH).unwrap();
}

/// A rename whose record failed (`EIO`, queued for repair), then a kill
/// while the queue was being saved, after its temporary file was written
/// and before the rename: the next mount repairs the rename from the
/// temporary file before serving.
#[test]
#[ignore = "needs /dev/fuse and root; run by ci/crash-tests.sh"]
fn killed_while_saving_the_repair_queue() {
    let v = Vol::new();
    fs::write(v.backing().join("a.txt"), "a\n").unwrap();
    v.ok("", &["record", "a.txt"]);
    assert_eq!(v.committed("a.txt").as_deref(), Some("a\n"));

    let child = v.mount("journal.before_write=error@1;repairs.before_rename=kill@1");
    let _ = v.sh("mv a.txt b.txt");
    v.reap_killed(child, "repairs.before_rename");
    assert!(
        v.backing().join("b.txt").exists(),
        "the rename reached the backing tree"
    );
    assert_eq!(v.committed("a.txt").as_deref(), Some("a\n"));

    let child = v.mount("");
    let deadline = Instant::now() + Duration::from_secs(15);
    while v.committed("b.txt").is_none() || v.committed("a.txt").is_some() {
        assert!(
            Instant::now() < deadline,
            "the rename was never repaired:\n{}",
            v.log_text()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    v.unmount(child);
    assert_eq!(v.committed("b.txt").as_deref(), Some("a\n"));
    let repairs = agentfs_core::volume::StateDir::new(v.state()).repairs();
    assert!(!repairs.exists() && !repairs.with_extension("tmp").exists());
    crash_oracle::check_clean(&v.state(), BRANCH).unwrap();
}
