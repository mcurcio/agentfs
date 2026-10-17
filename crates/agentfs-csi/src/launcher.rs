//! Starting the `agentfs` children the node plugin supervises (design
//! §14.2, §17.2): one `agentfs serve-history` per staged volume, one
//! `agentfs mount --history-service` front per published target, and the
//! release scan's children ([`crate::release_scan`]): one `agentfs flush`
//! then one `agentfs push` per worktree no pod stages, and one `agentfs
//! fork finish` per fork
//! whose deletion asked for it to be finished (design §11.5).
//!
//! The children are the same commands the single-node deployments run, so
//! a crashed front or service never takes the gRPC server down. Their
//! output is forwarded line by line to the plugin's stderr with a
//! `[service <handle>]` or `[front <ns>/<pod> <key>]` prefix; the last
//! [`RECENT_LINES`] are kept for error messages.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::fmt;
use std::io;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rustix::process::{Pid, Signal};
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::Command;
use tokio::sync::watch;

use crate::layout::Layout;
use crate::params::PodInfo;

/// How many of a child's last output lines are kept.
pub const RECENT_LINES: usize = 20;

/// A child to start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessSpec {
    /// The log prefix, without brackets: `service <handle>`.
    pub label: String,
    /// Arguments after the program.
    pub args: Vec<OsString>,
    /// Variables set for the child, after every inherited `AGENTFS_*` one
    /// is removed.
    pub env: Vec<(OsString, OsString)>,
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

/// A started child.
#[derive(Debug, Clone)]
pub struct Spawned {
    pub pid: u32,
    /// `Some` once the child has exited.
    pub exited: watch::Receiver<Option<Exit>>,
    pub recent_log: RecentLog,
}

impl Spawned {
    /// The recent output, one line per line.
    pub fn recent(&self) -> String {
        self.recent_log
            .lock()
            .map(|l| l.iter().cloned().collect::<Vec<_>>().join("\n"))
            .unwrap_or_default()
    }

    /// How it exited, if it has.
    pub fn exit(&self) -> Option<Exit> {
        *self.exited.borrow()
    }
}

/// Starts and signals children. A trait so tests can fake processes.
pub trait Launcher: Send + Sync + 'static {
    /// Starts a child. Called within a tokio runtime.
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
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("AGENTFS_") {
                cmd.env_remove(key);
            }
        }
        cmd.envs(spec.env.iter().map(|(k, v)| (k, v)));
        let mut child = cmd.spawn()?;
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

/// The node plugin's own identity, from the downward API: the history
/// services' `--namespace`, `--pod` and `--node`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PluginIdentity {
    pub namespace: String,
    pub pod: String,
    pub node: String,
}

/// How fronts mount.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrontOptions {
    /// `--passthrough`: auto, on or off.
    pub passthrough: String,
    /// `--threads`.
    pub threads: usize,
    /// `--history-wait`.
    pub history_wait: Duration,
}

fn os(s: impl Into<OsString>) -> OsString {
    s.into()
}

fn secs(d: Duration) -> OsString {
    os(format!("{}ms", d.as_millis()))
}

/// How long a history service waits for its volume's state lock.
pub const SERVICE_LOCK_WAIT: Duration = Duration::from_secs(30);

/// `agentfs --state S --root B --wait-lock 30000ms serve-history --listen unix:<socket> ...`
///
/// It waits up to [`SERVICE_LOCK_WAIT`] for the state lock, which the
/// release scan's `agentfs push` of the then unstaged worktree, a
/// finisher or DeleteVolume's listing may hold for a moment as the volume
/// is staged.
pub fn service_spec(layout: &Layout, handle: &str, plugin: &PluginIdentity) -> ProcessSpec {
    let args = vec![
        os("--state"),
        layout.state(handle).into_os_string(),
        os("--root"),
        layout.backing(handle).into_os_string(),
        os("--wait-lock"),
        secs(SERVICE_LOCK_WAIT),
        os("serve-history"),
        os("--listen"),
        os(format!("unix:{}", layout.socket(handle).display())),
        os("--namespace"),
        os(plugin.namespace.as_str()),
        os("--pod"),
        os(plugin.pod.as_str()),
        os("--node"),
        os(plugin.node.as_str()),
        os("--shutdown-grace"),
        os("10s"),
        os("--push-grace"),
        os("5s"),
    ];
    ProcessSpec {
        label: format!("service {handle}"),
        args,
        env: Vec::new(),
    }
}

/// `agentfs --state <state> push` for the release scan, with the plugin's identity env.
pub fn push_spec(layout: &Layout, handle: &str, identity: &PluginIdentity) -> ProcessSpec {
    ProcessSpec {
        label: format!("push {handle}"),
        args: vec![
            os("--state"),
            layout.state(handle).into_os_string(),
            os("push"),
        ],
        env: identity_env(identity),
    }
}

/// `agentfs --state <state> --root <backing> flush` for the release scan:
/// commits what a stopped history service left in the journal, before the
/// push; with the plugin's identity env.
pub fn flush_spec(layout: &Layout, handle: &str, identity: &PluginIdentity) -> ProcessSpec {
    ProcessSpec {
        label: format!("flush {handle}"),
        args: vec![
            os("--state"),
            layout.state(handle).into_os_string(),
            os("--root"),
            layout.backing(handle).into_os_string(),
            os("flush"),
        ],
        env: identity_env(identity),
    }
}

fn identity_env(identity: &PluginIdentity) -> Vec<(OsString, OsString)> {
    vec![
        (os("AGENTFS_NAMESPACE"), os(identity.namespace.as_str())),
        (os("AGENTFS_POD"), os(identity.pod.as_str())),
        (os("AGENTFS_NODE"), os(identity.node.as_str())),
    ]
}

/// How long one finisher child publishes before it exits 1 and frees the
/// fork's state lock; the release scan's next pass starts another. Between
/// them an operator's `fork abandon --wait-lock` gets the lock (design
/// §11.5).
pub const FINISH_TIMEOUT: Duration = Duration::from_secs(300);

/// argv of the finisher child, with the plugin's identity flags: `--state
/// S --root B fork finish --timeout 300000ms --namespace NS --pod POD --node
/// NODE` ([`FINISH_TIMEOUT`]).
pub fn fork_finish_args(state: &Path, root: &Path, identity: &PluginIdentity) -> Vec<OsString> {
    vec![
        os("--state"),
        state.as_os_str().to_owned(),
        os("--root"),
        root.as_os_str().to_owned(),
        os("fork"),
        os("finish"),
        os("--timeout"),
        secs(FINISH_TIMEOUT),
        os("--namespace"),
        os(identity.namespace.as_str()),
        os("--pod"),
        os(identity.pod.as_str()),
        os("--node"),
        os(identity.node.as_str()),
    ]
}

/// The finisher child of the fork `handle`.
pub fn fork_finish_spec(layout: &Layout, handle: &str, identity: &PluginIdentity) -> ProcessSpec {
    ProcessSpec {
        label: format!("fork {handle}"),
        args: fork_finish_args(&layout.state(handle), &layout.backing(handle), identity),
        env: Vec::new(),
    }
}

/// The replica name of a front without pod information (csi-sanity, a CO
/// without podInfoOnMount).
pub fn anonymous_replica(key: &str) -> String {
    format!("publish-{key}")
}

/// `agentfs --state S mount --history-service unix:<socket> ... B <source>`,
/// with a `--workload-mount` for each of `workload_mounts` (design §5).
#[allow(clippy::too_many_arguments)]
pub fn front_spec(
    layout: &Layout,
    handle: &str,
    key: &str,
    pod: Option<&PodInfo>,
    slot: Option<u16>,
    node: &str,
    options: &FrontOptions,
    workload_mounts: &[String],
) -> ProcessSpec {
    let mut args = vec![
        os("--state"),
        layout.state(handle).into_os_string(),
        os("mount"),
        os("--history-service"),
        os(format!("unix:{}", layout.socket(handle).display())),
    ];
    if let Some(slot) = slot {
        args.extend([os("--history-slot"), os(slot.to_string())]);
    }
    let (namespace, name, uid, replica) = match pod {
        Some(p) => (
            p.namespace.as_str(),
            p.name.as_str(),
            p.uid.as_str(),
            p.name.clone(),
        ),
        None => ("", "", "", anonymous_replica(key)),
    };
    args.extend([
        os("--history-wait"),
        secs(options.history_wait),
        os("--threads"),
        os(options.threads.to_string()),
        os("--passthrough"),
        os(options.passthrough.as_str()),
        os("--namespace"),
        os(namespace),
        os("--pod"),
        os(name),
        os("--pod-uid"),
        os(uid),
        os("--node"),
        os(node),
        os("--replica"),
        os(replica),
    ]);
    for mount in workload_mounts {
        args.extend([os("--workload-mount"), os(mount.as_str())]);
    }
    args.extend([
        layout.backing(handle).into_os_string(),
        layout.front_source(handle, key).into_os_string(),
    ]);
    let label = match pod {
        Some(p) => format!("front {}/{} {key}", p.namespace, p.name),
        None => format!("front {handle} {key}"),
    };
    ProcessSpec {
        label,
        args,
        env: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout() -> Layout {
        Layout {
            base: "/b".into(),
            node_state: "/n".into(),
            mount_dir: "/m".into(),
            profiles: "/p".into(),
        }
    }

    fn strings(spec: &ProcessSpec) -> Vec<String> {
        spec.args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn service_arguments() {
        let plugin = PluginIdentity {
            namespace: "agentfs-csi".into(),
            pod: "agentfs-csi-node-x".into(),
            node: "n1".into(),
        };
        let spec = service_spec(&layout(), "v", &plugin);
        assert_eq!(spec.label, "service v");
        assert_eq!(
            strings(&spec).join(" "),
            "--state /b/v/state --root /b/v/backing --wait-lock 30000ms serve-history --listen \
             unix:/n/run/v/history.sock --namespace agentfs-csi --pod agentfs-csi-node-x \
             --node n1 --shutdown-grace 10s --push-grace 5s"
        );
    }

    #[test]
    fn front_arguments() {
        let options = FrontOptions {
            passthrough: "auto".into(),
            threads: 4,
            history_wait: Duration::from_secs(30),
        };
        let pod = PodInfo {
            namespace: "ns".into(),
            name: "web-0".into(),
            uid: "u-1".into(),
            service_account: "sa".into(),
        };
        let spec = front_spec(
            &layout(),
            "v",
            "k",
            Some(&pod),
            Some(3),
            "n1",
            &options,
            &[],
        );
        assert_eq!(spec.label, "front ns/web-0 k");
        assert_eq!(
            strings(&spec).join(" "),
            "--state /b/v/state mount --history-service unix:/n/run/v/history.sock \
             --history-slot 3 --history-wait 30000ms --threads 4 --passthrough auto \
             --namespace ns --pod web-0 --pod-uid u-1 --node n1 --replica web-0 /b/v/backing \
             /m/v/k"
        );
        let mounts = ["/workspace".to_owned(), "/src=proj".to_owned()];
        let spec = front_spec(&layout(), "v", "k", None, None, "n1", &options, &mounts);
        let args = strings(&spec);
        assert!(
            args.join(" ").ends_with(
                "--workload-mount /workspace --workload-mount /src=proj /b/v/backing /m/v/k"
            ),
            "{args:?}"
        );
        assert!(!args.contains(&"--history-slot".to_owned()), "{args:?}");
        let replica = args.iter().position(|a| a == "--replica").unwrap();
        assert_eq!(args[replica + 1], "publish-k");
        assert_eq!(spec.label, "front v k");
    }

    #[test]
    fn exits_display() {
        assert_eq!(Exit::Code(3).to_string(), "exit code 3");
        assert_eq!(Exit::Signal(9).to_string(), "killed by SIGKILL");
        assert_eq!(Exit::Signal(6).to_string(), "killed by signal 6");
    }

    #[test]
    fn the_recent_log_keeps_the_last_lines() {
        let recent: RecentLog = Arc::default();
        for i in 0..RECENT_LINES + 5 {
            push_line(&recent, i.to_string());
        }
        let lines = recent.lock().unwrap();
        assert_eq!(lines.len(), RECENT_LINES);
        assert_eq!(lines.front().unwrap(), "5");
    }

    #[tokio::test]
    async fn a_real_child_reports_its_output_exit_and_signals() {
        // SAFETY of the test: AGENTFS_* in the parent must not reach it.
        let launcher = AgentfsLauncher { bin: "sh".into() };
        let spawned = launcher
            .spawn(ProcessSpec {
                label: "test".into(),
                args: vec![
                    os("-c"),
                    os("echo out; echo err >&2; echo \"v=${AGENTFS_X:-unset} y=$Y\"; exit 7"),
                ],
                env: vec![(os("Y"), os("set"))],
            })
            .unwrap();
        let mut exited = spawned.exited.clone();
        let exit = *exited.wait_for(Option::is_some).await.unwrap();
        assert_eq!(exit, Some(Exit::Code(7)));
        let recent = spawned.recent();
        assert!(recent.contains("out") && recent.contains("err"), "{recent}");
        assert!(recent.contains("v=unset y=set"), "{recent}");

        let sleeper = launcher
            .spawn(ProcessSpec {
                label: "sleep".into(),
                args: vec![os("-c"), os("exec sleep 30")],
                env: Vec::new(),
            })
            .unwrap();
        launcher.signal(sleeper.pid, Signal::KILL).unwrap();
        let mut exited = sleeper.exited.clone();
        let exit = *exited.wait_for(Option::is_some).await.unwrap();
        assert_eq!(exit, Some(Exit::Signal(9)));
    }
}
