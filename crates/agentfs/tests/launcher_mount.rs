//! `agentfs mount` on real FUSE mounts (docs/trees.md, "Run it"): a command
//! run in the mount, a stop sent on to it, a refused view keeping it from
//! running, the operator settings' not-built lines, `probe` on a live and a
//! stale view, and a restart after a crash under each `--on-stale-mount`
//! policy. Ignored by default: they need root and
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
    let out = v.run(&[
        "init",
        "--volume",
        "t",
        "--include",
        include.to_str().unwrap(),
    ]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let root = v.root().display().to_string();
    let out = v.mount(&["--", "sh", "-c", &format!("echo c > {root}/c")]);
    let err = text(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{err}");
    assert!(err.contains("not built: remote.o.url (push)"), "{err}");
    assert!(
        err.contains("not built: agentfs.patchremote (patches)"),
        "{err}"
    );
    assert_eq!(v.history("c").as_deref(), Some("c\n"));
}

/// (b2) The operator's `agentfs.overlayLimit` reaches the lag gate: under a
/// limit of 1 KiB, a few creates take the overlay over it, so new work
/// waits for the builder and `tree HistoryStalled` is logged; every file is
/// recorded. Under the default 32 MiB these creates never stall.
#[test]
#[ignore = "needs root and /dev/fuse: ci/fuse-tests.sh"]
fn the_operators_overlay_limit_holds_new_work() {
    let Some(v) = Volume::new() else { return };
    let include = v.p("operator.conf");
    std::fs::write(&include, "[agentfs]\n\toverlayLimit = 1k\n").unwrap();
    let out = v.run(&[
        "init",
        "--volume",
        "t",
        "--include",
        include.to_str().unwrap(),
    ]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let root = v.root().display().to_string();
    let script = format!("for i in $(seq 1 50); do echo $i > {root}/f$i; done");
    let out = v.mount(&["--", "sh", "-c", &script]);
    let err = text(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{err}");
    assert!(err.contains("tree HistoryStalled"), "{err}");
    assert_eq!(v.history("f50").as_deref(), Some("50\n"));
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
    assert_eq!(
        v.probe(&["--allow-stale"]),
        Some(0),
        "the stale mount is left"
    );
    // A held boot reads and logs the operator settings too.
    let include = v.p("operator.conf");
    std::fs::write(&include, "[remote \"o\"]\n\turl = /x\n").unwrap();
    let out = v.run(&[
        "init",
        "--volume",
        "t",
        "--include",
        include.to_str().unwrap(),
    ]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let mut held = v.spawn(&["--on-stale-mount", "hold"]);
    held.wait_for("not built: remote.o.url (push)");
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

/// (f) A SIGTERM to the mount while its command runs is sent on to the
/// command: a command whose cwd is in the mount (so the view stays busy
/// until it exits) traps it and exits 7, the mount exits 7, and nothing is
/// left mounted.
#[test]
#[ignore = "needs root and /dev/fuse: ci/fuse-tests.sh"]
fn a_stop_is_sent_on_to_the_command() {
    let Some(v) = Volume::new() else { return };
    let root = v.root().display().to_string();
    let script = format!(
        "cd {root}; trap 'kill $! 2>/dev/null; exit 7' TERM; sleep 1000 & echo up >&2; wait"
    );
    let mut mount = v.spawn(&["--", "sh", "-c", &script]);
    mount.wait_for("up");
    rustix::process::kill_process(
        rustix::process::Pid::from_child(&mount.child),
        rustix::process::Signal::TERM,
    )
    .unwrap();
    let status = mount.child.wait().unwrap();
    assert_eq!(status.code(), Some(7), "{}", mount.seen.join("\n"));
    assert_eq!(v.probe(&["--allow-stale"]), Some(1), "nothing is mounted");
    assert_eq!(agentfs_fuse::stale::agentfs_layers(&v.root()).unwrap(), 0);
}

/// (g) A view that cannot open means the command never runs: the root's
/// view opens, then the fork's is refused when it opens (the metrics output
/// is inside its mount point, which only the view host judges); the mount
/// exits 1, the command's marker is never made, and no view is left
/// mounted.
#[test]
#[ignore = "needs root and /dev/fuse: ci/fuse-tests.sh"]
fn a_view_that_cannot_open_means_the_command_never_runs() {
    let Some(v) = Volume::new() else { return };
    let marker = v.p("marker");
    let fork = v.p("fork");
    let out = v.mount(&[
        "--fork-mount",
        fork.to_str().unwrap(),
        "--metrics-output",
        fork.join("metrics.json").to_str().unwrap(),
        "--",
        "touch",
        marker.to_str().unwrap(),
    ]);
    let err = text(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(
        err.contains(&format!("mounting {}", fork.display())),
        "{err}"
    );
    assert!(!marker.exists(), "the command ran: {err}");
    assert_eq!(v.probe(&["--allow-stale"]), Some(1), "nothing is mounted");
    assert_eq!(agentfs_fuse::stale::agentfs_layers(&v.root()).unwrap(), 0);
    assert_eq!(agentfs_fuse::stale::agentfs_layers(&fork).unwrap(), 0);
}

/// (h) A restart with `recover` never stacks a new fork over a crashed
/// fork's mount: a crashed `mount ROOT --fork-mount F` without --fork-key
/// restarted the same way exits 1 naming the fix, and F stays stale.
#[test]
#[ignore = "needs root and /dev/fuse: ci/fuse-tests.sh"]
fn recover_never_stacks_a_new_fork_over_a_crashed_one() {
    let Some(v) = Volume::new() else { return };
    let fork = v.p("fork");
    let fork_arg = fork.to_str().unwrap();
    let mut mount = v.spawn(&["--fork-mount", fork_arg]);
    mount.wait_for("mounted volume t");
    mount.child.kill().unwrap();
    mount.child.wait().unwrap();
    let out = v.mount(&["--fork-mount", fork_arg, "--on-stale-mount", "recover"]);
    let err = text(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(err.contains("--fork-key"), "{err}");
    assert!(!err.contains("mounted volume"), "{err}");
    assert_eq!(agentfs_fuse::stale::agentfs_layers(&fork).unwrap(), 1);
    agentfs_fuse::stale::detach_stale_layers(&fork, 16).unwrap();
    agentfs_fuse::stale::detach_stale_layers(&v.root(), 16).unwrap();
}

/// (i) The tree view's requests are measured at the FUSE edge, by op, and
/// every hold of the volume lock is one wait to take it (docs/telemetry.md,
/// "The volume lock"): a command that creates, writes, reads and lists in
/// the mount leaves each op counted in the metrics file written at unmount,
/// each request in the histogram bucket of its time.
#[test]
#[ignore = "needs root and /dev/fuse: ci/fuse-tests.sh"]
fn requests_are_measured_by_op_at_the_fuse_edge() {
    let Some(v) = Volume::new() else { return };
    let metrics = v.p("metrics.json");
    let root = v.root().display().to_string();
    let script = format!("echo x > {root}/a && cat {root}/a >/dev/null && ls {root} >/dev/null");
    let out = v.mount(&[
        "--metrics-output",
        metrics.to_str().unwrap(),
        "--",
        "sh",
        "-c",
        &script,
    ]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let file: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&metrics).unwrap()).unwrap();
    let stages = &file["stages"];
    for op in [
        "lookup", "getattr", "create", "write", "flush", "release", "read", "readdir",
    ] {
        let op_figure = &stages["requests"][op];
        let count = op_figure["count"].as_u64().unwrap();
        assert!(count > 0, "{op}: {op_figure}");
        let bucketed: u64 = op_figure["buckets"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n.as_u64().unwrap())
            .sum();
        assert_eq!(bucketed, count, "{op}: {op_figure}");
    }
    // One wait per hold; the snapshot is read inside a hold, so that one
    // hold's wait is counted and the hold itself not yet.
    let waits = stages["lock_wait"]["count"].as_u64().unwrap();
    let holds = stages["actor_lock"]["count"].as_u64().unwrap();
    assert!(holds > 0 && waits - holds <= 1, "{waits} waits, {holds} holds");
    assert!(stages["lock_hold"]["close"]["count"].as_u64().unwrap() > 0);
}
