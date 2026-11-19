//! `agentfs mount` on real FUSE mounts (docs/trees.md, "Run it"): a command
//! run in the mount, the operator settings' not-built lines, `probe` on a
//! live and a stale view, and a restart after a crash under each
//! `--on-stale-mount` policy. Ignored by default: they need root and
//! /dev/fuse, and ci/fuse-tests.sh runs them with sudo.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc;

struct Volume {
    dir: tempfile::TempDir,
}

impl Volume {
    /// A tree volume `t`, or `None` (with a message) without root and FUSE.
    fn new() -> Option<Self> {
        if !rustix::process::geteuid().is_root() || !Path::new("/dev/fuse").exists() {
            eprintln!("skipped: needs root and /dev/fuse (ci/fuse-tests.sh)");
            return None;
        }
        let v = Self {
            dir: tempfile::tempdir().unwrap(),
        };
        let out = v.run(&["init", "--volume", "t"]);
        assert!(out.status.success(), "{}", text(&out.stderr));
        Some(v)
    }

    fn p(&self, rel: &str) -> PathBuf {
        self.dir.path().join(rel)
    }

    fn root(&self) -> PathBuf {
        self.p("root")
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_agentfs"));
        for (name, _) in std::env::vars_os() {
            let n = name.to_string_lossy();
            if n.starts_with("OTEL_") || n.starts_with("AGENTFS_") {
                cmd.env_remove(name);
            }
        }
        cmd.arg("--state").arg(self.p("state"));
        cmd
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command().args(args).output().unwrap()
    }

    /// `mount ROOT ARGS...` to its exit.
    fn mount(&self, args: &[&str]) -> Output {
        self.command()
            .arg("mount")
            .arg(self.root())
            .args(args)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    /// `mount ROOT ARGS...` in the background, its stderr read line by line.
    fn spawn(&self, args: &[&str]) -> Running {
        let mut child = self
            .command()
            .arg("mount")
            .arg(self.root())
            .args(args)
            .stdin(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let (sender, lines) = mpsc::channel();
        let stderr = child.stderr.take().unwrap();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines() {
                let Ok(line) = line else { break };
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
        Running {
            child,
            lines,
            seen: Vec::new(),
        }
    }

    /// `agentfs probe [FLAGS] ROOT`'s exit status.
    fn probe(&self, flags: &[&str]) -> Option<i32> {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_agentfs"));
        cmd.arg("probe").args(flags).arg(self.root());
        cmd.output().unwrap().status.code()
    }

    /// What the root's history holds at `path`, if anything.
    fn history(&self, path: &str) -> Option<String> {
        let hash = Command::new("git")
            .args(["hash-object", "--stdin"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .and_then(|mut git| {
                use std::io::Write as _;
                git.stdin.take().unwrap().write_all(path.as_bytes())?;
                git.wait_with_output()
            })
            .unwrap();
        let hex = text(&hash.stdout).trim().to_owned();
        let out = Command::new("git")
            .arg("--git-dir")
            .arg(self.p("state/history.git"))
            .arg("show")
            .arg(format!("root:files/{}/{}/{path}", &hex[..2], &hex[2..4]))
            .output()
            .unwrap();
        out.status.success().then(|| text(&out.stdout))
    }

    /// A stale mount at the root: a mount killed with SIGKILL after it
    /// saved `before`.
    fn stale(&self) {
        let mut mount = self.spawn(&[]);
        mount.wait_for("mounted volume t");
        std::fs::write(self.root().join("before"), "kept\n").unwrap();
        mount.child.kill().unwrap();
        mount.child.wait().unwrap();
        assert_eq!(self.probe(&["--allow-stale"]), Some(0), "stale is allowed");
        assert_eq!(self.probe(&[]), Some(1), "a stale mount is not live");
    }
}

/// A mount running in the background.
struct Running {
    child: Child,
    lines: mpsc::Receiver<String>,
    seen: Vec<String>,
}

impl Running {
    /// Waits for a stderr line holding `needle`: an event, never a timer.
    fn wait_for(&mut self, needle: &str) {
        loop {
            match self.lines.recv() {
                Ok(line) => {
                    let found = line.contains(needle);
                    self.seen.push(line);
                    if found {
                        return;
                    }
                }
                Err(_) => panic!(
                    "the mount ended before {needle:?}: {}",
                    self.seen.join("\n")
                ),
            }
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// (a) The command runs once the view is up, its write is recorded, and
/// the mount exits with the command's status.
#[test]
#[ignore = "needs root and /dev/fuse: ci/fuse-tests.sh"]
fn a_command_runs_in_the_mount() {
    let Some(v) = Volume::new() else { return };
    let root = v.root().display().to_string();
    let out = v.mount(&["--", "sh", "-c", &format!("echo hi > {root}/a")]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert!(
        text(&out.stderr).contains(&format!("mounted volume t at {root}")),
        "{}",
        text(&out.stderr)
    );
    assert_eq!(v.history("a").as_deref(), Some("hi\n"));
    let out = v.mount(&["--", "sh", "-c", &format!("echo b > {root}/b; exit 3")]);
    assert_eq!(out.status.code(), Some(3), "{}", text(&out.stderr));
    assert_eq!(v.history("b").as_deref(), Some("b\n"));
    let out = v.mount(&["--", "/nonexistent/command"]);
    assert_eq!(out.status.code(), Some(127), "{}", text(&out.stderr));
}

/// (b) A remote and a patch source in the operator settings: each logged
/// as not built, and the view records anyway.
#[test]
#[ignore = "needs root and /dev/fuse: ci/fuse-tests.sh"]
fn not_built_settings_are_logged_and_the_view_records() {
    let Some(v) = Volume::new() else { return };
    let include = v.p("operator.conf");
    std::fs::write(
        &include,
        "[remote \"o\"]\n\turl = /x\n[agentfs]\n\tpatchRemote = o\n",
    )
    .unwrap();
    let out = v.run(&["init", "--volume", "t", "--include", include.to_str().unwrap()]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let root = v.root().display().to_string();
    let out = v.mount(&["--", "sh", "-c", &format!("echo c > {root}/c")]);
    let err = text(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{err}");
    assert!(err.contains("not built: remote.o.url (push)"), "{err}");
    assert!(err.contains("not built: agentfs.patchremote (patches)"), "{err}");
    assert_eq!(v.history("c").as_deref(), Some("c\n"));
}

/// (c) `probe` answers 0 on a live tree view: run as the mount's command.
#[test]
#[ignore = "needs root and /dev/fuse: ci/fuse-tests.sh"]
fn probe_sees_a_live_view() {
    let Some(v) = Volume::new() else { return };
    let root = v.root().display().to_string();
    let out = v.mount(&["--", env!("CARGO_BIN_EXE_agentfs"), "probe", &root]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert!(
        text(&out.stdout).contains("live agentfs mount (agentfs:t)"),
        "{}",
        text(&out.stdout)
    );
}

/// (d) A restart with `recover` stacks over the stale mount: what was saved
/// before the kill reads through the same path, and once it stops no
/// agentfs layer is left there.
#[test]
#[ignore = "needs root and /dev/fuse: ci/fuse-tests.sh"]
fn recover_stacks_over_a_crashed_view() {
    let Some(v) = Volume::new() else { return };
    v.stale();
    let root = v.root().display().to_string();
    let out = v.mount(&[
        "--on-stale-mount",
        "recover",
        "--",
        "sh",
        "-c",
        &format!("test \"$(cat {root}/before)\" = kept"),
    ]);
    let err = text(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{err}");
    assert!(
        err.contains(&format!(
            "stacked over a stale mount at {root} left by a crashed daemon"
        )),
        "{err}"
    );
    assert_eq!(agentfs_fuse::stale::agentfs_layers(&v.root()).unwrap(), 0);
}

/// (e) `refuse` exits 1 and leaves the stale mount; `hold` mounts nothing,
/// and at SIGTERM detaches it and exits 1.
#[test]
#[ignore = "needs root and /dev/fuse: ci/fuse-tests.sh"]
fn refuse_and_hold_leave_a_stale_mount_alone() {
    let Some(v) = Volume::new() else { return };
    v.stale();
    let out = v.mount(&[]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    assert!(
        text(&out.stderr).contains("--on-stale-mount recover"),
        "{}",
        text(&out.stderr)
    );
    assert_eq!(v.probe(&["--allow-stale"]), Some(0), "the stale mount is left");
    let mut held = v.spawn(&["--on-stale-mount", "hold"]);
    held.wait_for("Holding until SIGTERM");
    assert_eq!(v.probe(&["--allow-stale"]), Some(0), "nothing was mounted");
    rustix::process::kill_process(
        rustix::process::Pid::from_child(&held.child),
        rustix::process::Signal::TERM,
    )
    .unwrap();
    let status = held.child.wait().unwrap();
    assert_eq!(status.code(), Some(1), "{}", held.seen.join("\n"));
    assert_eq!(v.probe(&[]), Some(1));
    assert_eq!(agentfs_fuse::stale::agentfs_layers(&v.root()).unwrap(), 0);
}
