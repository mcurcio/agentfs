//! Starting the `agentfs` children the node plugin runs (docs/csi.md): one
//! `agentfs init` per CreateVolume (and per static PV adopted), and one
//! `agentfs host` per volume in use on the node, driven over its standard
//! input ([`crate::host_client`]).
//!
//! A child per volume follows the FUSE CSI drivers' practice (one FUSE
//! daemon per volume in its own process), so a crashed host never takes the
//! gRPC server down. The children's output is forwarded line by line to the
//! plugin's stderr with a `[host <volume>]` or `[init <handle>]` prefix; the
//! last [`RECENT_LINES`] are kept for error messages.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::fmt;
use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agentfs_contracts::name::VolumeName;
use rustix::process::{Pid, Signal};
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::Command;
use tokio::sync::watch;

use crate::layout::Layout;
use crate::params::VolumeParams;

/// How many of a child's last output lines are kept.
pub const RECENT_LINES: usize = 20;

/// The launcher id the hosts record their clients under: what a restarted
/// host rebinds them by.
pub const LAUNCHER_ID: &str = "csi";

/// A child to start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessSpec {
    /// The log prefix, without brackets: `host <volume>`.
    pub label: String,
    /// Arguments after the program.
    pub args: Vec<OsString>,
    /// Variables set for the child, after every inherited `AGENTFS_*` one
    /// is removed.
    pub env: Vec<(OsString, OsString)>,
    /// Whether the child's standard input is a control channel: one end of
    /// a Unix socket pair whose other end is [`Spawned::control`].
    pub control: bool,
}

/// How a child ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    Code(i32),
    Signal(i32),
}

impl From<ExitStatus> for Exit {
    fn from(status: ExitStatus) -> Self {
        match (status.code(), status.signal()) {
            (Some(code), _) => Self::Code(code),
            (None, Some(signal)) => Self::Signal(signal),
            (None, None) => Self::Code(-1),
        }
    }
}

impl fmt::Display for Exit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Code(code) => write!(f, "exit code {code}"),
            Self::Signal(9) => f.write_str("killed by SIGKILL"),
            Self::Signal(15) => f.write_str("killed by SIGTERM"),
            Self::Signal(n) => write!(f, "killed by signal {n}"),
        }
    }
}

/// The last lines a child printed.
pub type RecentLog = Arc<Mutex<VecDeque<String>>>;

/// The recent output of a child, one line per line.
pub fn recent(log: &RecentLog) -> String {
    log.lock()
        .map(|l| l.iter().cloned().collect::<Vec<_>>().join("\n"))
        .unwrap_or_default()
}

/// A started child.
#[derive(Debug)]
pub struct Spawned {
    pub pid: u32,
    /// `Some` once the child has exited.
    pub exited: watch::Receiver<Option<Exit>>,
    pub recent_log: RecentLog,
    /// The parent's end of the control channel, when the spec asked for one.
    pub control: Option<tokio::net::UnixStream>,
}

/// Starts and signals children. A trait so tests can fake processes.
pub trait Launcher: Send + Sync + 'static {
    /// Starts a child. Called within a tokio runtime. With
    /// [`ProcessSpec::control`], the child's standard input is one end of a
    /// socket pair and [`Spawned::control`] the other.
    fn spawn(&self, spec: ProcessSpec) -> io::Result<Spawned>;
    /// Sends `signal` to a child.
    fn signal(&self, pid: u32, signal: Signal) -> io::Result<()>;
}

/// Runs the `agentfs` binary.
#[derive(Debug, Clone)]
pub struct AgentfsLauncher {
    pub bin: PathBuf,
}

impl Launcher for AgentfsLauncher {
    fn spawn(&self, spec: ProcessSpec) -> io::Result<Spawned> {
        let mut cmd = Command::new(&self.bin);
        cmd.args(&spec.args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let ours = if spec.control {
            let (ours, theirs) = std::os::unix::net::UnixStream::pair()?;
            cmd.stdin(Stdio::from(OwnedFd::from(theirs)));
            ours.set_nonblocking(true)?;
            Some(tokio::net::UnixStream::from_std(ours)?)
        } else {
            cmd.stdin(Stdio::null());
            None
        };
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("AGENTFS_") {
                cmd.env_remove(key);
            }
        }
        cmd.envs(spec.env.iter().map(|(k, v)| (k, v)));
        let mut child = cmd.spawn()?;
        // The child's end of the socket pair went with `cmd`'s stdin: once
        // `cmd` is dropped only the child holds it, so the child's exit is
        // EOF on ours.
        drop(cmd);
        let pid = child.id().unwrap_or(0);
        let recent: RecentLog = Arc::new(Mutex::new(VecDeque::with_capacity(RECENT_LINES)));
        let label = Arc::new(spec.label);
        let mut readers = Vec::new();
        if let Some(out) = child.stdout.take() {
            readers.push(tokio::spawn(forward(out, label.clone(), recent.clone())));
        }
        if let Some(err) = child.stderr.take() {
            readers.push(tokio::spawn(forward(err, label.clone(), recent.clone())));
        }
        let (tx, rx) = watch::channel(None);
        tokio::spawn(async move {
            let exit = match child.wait().await {
                Ok(status) => Exit::from(status),
                Err(e) => {
                    eprintln!("[{label}] waiting for the process: {e}");
                    Exit::Code(-1)
                }
            };
            // Its last words before the exit is reported, when they come
            // promptly (a grandchild may hold the pipes open).
            for reader in readers {
                let _ = tokio::time::timeout(Duration::from_secs(1), reader).await;
            }
            let _ = tx.send(Some(exit));
        });
        Ok(Spawned {
            pid,
            exited: rx,
            recent_log: recent,
            control: ours,
        })
    }

    fn signal(&self, pid: u32, signal: Signal) -> io::Result<()> {
        let pid = i32::try_from(pid)
            .ok()
            .and_then(Pid::from_raw)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "bad pid"))?;
        rustix::process::kill_process(pid, signal).map_err(io::Error::from)
    }
}

async fn forward(stream: impl AsyncRead + Unpin, label: Arc<String>, recent: RecentLog) {
    let mut lines = BufReader::new(stream).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        eprintln!("[{label}] {line}");
        push_line(&recent, line);
    }
}

/// Keeps the last [`RECENT_LINES`] lines.
pub fn push_line(recent: &RecentLog, line: String) {
    if let Ok(mut recent) = recent.lock() {
        if recent.len() == RECENT_LINES {
            recent.pop_front();
        }
        recent.push_back(line);
    }
}

/// The node plugin's own identity, from the downward API: the children's
/// `--namespace`, `--pod` and `--node`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PluginIdentity {
    pub namespace: String,
    pub pod: String,
    pub node: String,
}

fn os(s: impl Into<OsString>) -> OsString {
    s.into()
}

fn identity_args(plugin: &PluginIdentity) -> [OsString; 6] {
    [
        os("--namespace"),
        os(plugin.namespace.as_str()),
        os("--pod"),
        os(plugin.pod.as_str()),
        os("--node"),
        os(plugin.node.as_str()),
    ]
}

/// `agentfs --state <base>/<h>/state init --volume <name> --include
/// <profiles>/<p>.config --namespace .. --pod .. --node ..`, label
/// `init <h>`.
pub fn init_spec(
    layout: &Layout,
    handle: &str,
    params: &VolumeParams,
    plugin: &PluginIdentity,
) -> ProcessSpec {
    let mut args = vec![
        os("--state"),
        layout.state(handle).into_os_string(),
        os("init"),
        os("--volume"),
        os(params.name.as_str()),
        os("--include"),
        layout
            .profile_file(&params.profile, "config")
            .into_os_string(),
    ];
    args.extend(identity_args(plugin));
    ProcessSpec {
        label: format!("init {handle}"),
        args,
        env: Vec::new(),
        control: false,
    }
}

/// `agentfs --state <base>/<v>/state host --launcher csi --volume <name>
/// --namespace .. --pod .. --node ..`, label `host <v>`, with a control
/// channel.
pub fn host_spec(
    layout: &Layout,
    volume: &str,
    name: &VolumeName,
    plugin: &PluginIdentity,
) -> ProcessSpec {
    let mut args = vec![
        os("--state"),
        layout.state(volume).into_os_string(),
        os("host"),
        os("--launcher"),
        os(LAUNCHER_ID),
        os("--volume"),
        os(name.as_str()),
    ];
    args.extend(identity_args(plugin));
    ProcessSpec {
        label: format!("host {volume}"),
        args,
        env: Vec::new(),
        control: true,
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::AsyncWriteExt as _;

    use super::*;

    fn layout() -> Layout {
        Layout {
            base: "/b".into(),
            node_state: "/n".into(),
            profiles: "/p".into(),
        }
    }

    fn strings(spec: &ProcessSpec) -> Vec<String> {
        spec.args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    fn plugin() -> PluginIdentity {
        PluginIdentity {
            namespace: "agentfs-csi".into(),
            pod: "agentfs-csi-node-x".into(),
            node: "n1".into(),
        }
    }

    #[test]
    fn init_arguments() {
        let params = VolumeParams {
            profile: "example".into(),
            name: VolumeName::parse("apps.data.pvc-1").unwrap(),
        };
        let spec = init_spec(&layout(), "pvc-1", &params, &plugin());
        assert_eq!(spec.label, "init pvc-1");
        assert!(!spec.control);
        assert!(spec.env.is_empty());
        assert_eq!(
            strings(&spec).join(" "),
            "--state /b/pvc-1/state init --volume apps.data.pvc-1 --include /p/example.config \
             --namespace agentfs-csi --pod agentfs-csi-node-x --node n1"
        );
    }

    #[test]
    fn host_arguments() {
        let name = VolumeName::parse("apps.data.pvc-1").unwrap();
        let spec = host_spec(&layout(), "pvc-1", &name, &plugin());
        assert_eq!(spec.label, "host pvc-1");
        assert!(spec.control);
        assert!(spec.env.is_empty());
        assert_eq!(
            strings(&spec).join(" "),
            "--state /b/pvc-1/state host --launcher csi --volume apps.data.pvc-1 --namespace \
             agentfs-csi --pod agentfs-csi-node-x --node n1"
        );
    }

    #[test]
    fn exits_display() {
        assert_eq!(Exit::Code(3).to_string(), "exit code 3");
        assert_eq!(Exit::Signal(9).to_string(), "killed by SIGKILL");
        assert_eq!(Exit::Signal(6).to_string(), "killed by signal 6");
    }

    #[test]
    fn the_recent_log_keeps_the_last_lines() {
        let log: RecentLog = Arc::default();
        for i in 0..RECENT_LINES + 5 {
            push_line(&log, i.to_string());
        }
        assert_eq!(log.lock().unwrap().len(), RECENT_LINES);
        assert!(recent(&log).starts_with("5\n6"));
    }

    #[tokio::test]
    async fn a_real_child_reports_its_output_exit_and_signals() {
        // AGENTFS_* in the parent must not reach it.
        let launcher = AgentfsLauncher { bin: "sh".into() };
        let spawned = launcher
            .spawn(ProcessSpec {
                label: "test".into(),
                args: vec![
                    os("-c"),
                    os("echo out; echo err >&2; echo \"v=${AGENTFS_X:-unset} y=$Y\"; exit 7"),
                ],
                env: vec![(os("Y"), os("set"))],
                control: false,
            })
            .unwrap();
        assert!(spawned.control.is_none());
        let mut exited = spawned.exited.clone();
        let exit = *exited.wait_for(Option::is_some).await.unwrap();
        assert_eq!(exit, Some(Exit::Code(7)));
        let text = recent(&spawned.recent_log);
        assert!(text.contains("out") && text.contains("err"), "{text}");
        assert!(text.contains("v=unset y=set"), "{text}");

        let sleeper = launcher
            .spawn(ProcessSpec {
                label: "sleep".into(),
                args: vec![os("-c"), os("exec sleep 30")],
                env: Vec::new(),
                control: false,
            })
            .unwrap();
        launcher.signal(sleeper.pid, Signal::KILL).unwrap();
        let mut exited = sleeper.exited.clone();
        let exit = *exited.wait_for(Option::is_some).await.unwrap();
        assert_eq!(exit, Some(Exit::Signal(9)));
    }

    /// The control channel is the child's standard input: what the parent
    /// writes the child reads, what the child writes back on it the parent
    /// reads, and the child's exit is EOF.
    #[tokio::test]
    async fn the_control_channel_is_the_childs_stdin() {
        let launcher = AgentfsLauncher { bin: "sh".into() };
        let spawned = launcher
            .spawn(ProcessSpec {
                label: "echo".into(),
                args: vec![os("-c"), os("read line; echo \"got $line\" >&0")],
                env: Vec::new(),
                control: true,
            })
            .unwrap();
        let control = spawned.control.unwrap();
        let (read, mut write) = control.into_split();
        write.write_all(b"hello\n").await.unwrap();
        let mut lines = tokio::io::BufReader::new(read).lines();
        assert_eq!(
            lines.next_line().await.unwrap().as_deref(),
            Some("got hello")
        );
        assert_eq!(lines.next_line().await.unwrap(), None, "EOF at the exit");
        let mut exited = spawned.exited.clone();
        let exit = *exited.wait_for(Option::is_some).await.unwrap();
        assert_eq!(exit, Some(Exit::Code(0)));
    }
}
