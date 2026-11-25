//! The CSI driver on tree volumes, end to end (docs/csi.md): the real
//! `agentfs` binary under the driver's Controller and Node services, over a
//! gRPC socket. A launcher wrapping `AgentfsLauncher` logs each child's
//! start, exit and every line its host writes back, so a test waits for
//! those events, never for a timer.
//!
//! Without root: CreateVolume makes a tree volume with the templated name,
//! the profile included and its rules installed; DeleteVolume drains it
//! through a host (no view, so no FUSE) and keeps it; a recording-format
//! volume is refused; a fork claim is a keyed fork in its source's volume,
//! approvals and forks of forks are refused, and DeleteVolume ends it.
//! With root and /dev/fuse (ignored by default; run by ci/fuse-tests.sh
//! with sudo): NodePublish mounts a view at the target, a write is
//! committed with the pod's trailers, a second pod is refused, a killed
//! host comes back with the view live, NodeUnpublish leaves nothing
//! behind; a fork claim's view runs beside its source's on one host and
//! lands in it; and a close held open by a file stalls only its own claim.

use std::collections::HashMap;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

use agentfs_contracts::host::{Envelope, Reply, Request};
use agentfs_contracts::ports::{Found, StateArea as _};
use agentfs_csi::controller::ControllerService;
use agentfs_csi::csi::v1::controller_client::ControllerClient;
use agentfs_csi::csi::v1::node_client::NodeClient;
use agentfs_csi::csi::v1::volume_capability::access_mode::Mode;
use agentfs_csi::csi::v1::volume_capability::{AccessMode, AccessType, MountVolume};
use agentfs_csi::csi::v1::*;
use agentfs_csi::identity::IdentityService;
use agentfs_csi::launcher::{AgentfsLauncher, Launcher, ProcessSpec, Spawned};
use agentfs_csi::layout::Layout;
use agentfs_csi::mounter::KernelMounter;
use agentfs_csi::node::NodeService;
use agentfs_csi::server;
use agentfs_csi::supervisor::{NodeConfig, Supervisor};
use agentfs_engine::state_area::TreeStateArea;
use agentfs_fuse::stale::{MountState, mount_state};
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::{oneshot, watch};
use tonic::Code;
use tonic::transport::{Channel, Endpoint, Uri};
use tower::service_fn;

/// `AgentfsLauncher`, with every child's start (`spawn <label>`), exit
/// (`exit <label> <exit>`), each line a host writes back (`line <label>
/// <line>`) and each request written to it (`send <label> <request>`)
/// logged as an event; an envelope is logged as its body.
struct Tap {
    inner: AgentfsLauncher,
    log: Arc<watch::Sender<Vec<String>>>,
}

fn event(log: &watch::Sender<Vec<String>>, event: String) {
    log.send_modify(|l| l.push(event));
}

/// Copies lines from `from` to `to`, logging each with `tag` (`line
/// <label>` or `send <label>`); at EOF shuts `to`'s write side, so the
/// other end sees EOF too.
async fn copy_lines(
    from: OwnedReadHalf,
    mut to: OwnedWriteHalf,
    tag: (String, Arc<watch::Sender<Vec<String>>>),
) {
    let mut lines = BufReader::new(from).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let (tag, log) = &tag;
        let shown = if let Ok(sent) = serde_json::from_str::<Envelope<Request>>(&line) {
            serde_json::to_string(&sent.body).unwrap()
        } else if let Ok(reply) = serde_json::from_str::<Envelope<Reply>>(&line) {
            serde_json::to_string(&reply.body).unwrap()
        } else {
            line.clone()
        };
        event(log, format!("{tag} {shown}"));
        if to.write_all(format!("{line}\n").as_bytes()).await.is_err() {
            break;
        }
    }
    let _ = to.shutdown().await;
}

impl Launcher for Tap {
    fn spawn(&self, spec: ProcessSpec) -> std::io::Result<Spawned> {
        let label = spec.label.clone();
        let mut spawned = self.inner.spawn(spec)?;
        event(&self.log, format!("spawn {label}"));
        let mut exited = spawned.exited.clone();
        let log = self.log.clone();
        let exit_label = label.clone();
        tokio::spawn(async move {
            let exit = exited.wait_for(Option::is_some).await.ok().and_then(|e| *e);
            let exit = exit.map_or_else(|| "unknown".to_owned(), |e| e.to_string());
            event(&log, format!("exit {exit_label} {exit}"));
        });
        if let Some(host) = spawned.control.take() {
            let (ours, theirs) = UnixStream::pair()?;
            let (host_read, host_write) = host.into_split();
            let (their_read, their_write) = theirs.into_split();
            tokio::spawn(copy_lines(
                their_read,
                host_write,
                (format!("send {label}"), self.log.clone()),
            ));
            tokio::spawn(copy_lines(
                host_read,
                their_write,
                (format!("line {label}"), self.log.clone()),
            ));
            spawned.control = Some(ours);
        }
        Ok(spawned)
    }

    fn signal(&self, pid: u32, signal: rustix::process::Signal) -> std::io::Result<()> {
        self.inner.signal(pid, signal)
    }
}

struct Driver {
    dir: tempfile::TempDir,
    layout: Layout,
    channel: Channel,
    log: Arc<watch::Sender<Vec<String>>>,
    stop: Option<oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<()>>,
}

const EXCLUDE: &str = "*.log\ntmp/\n";

impl Driver {
    async fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        // Absolute: `agentfs init --include` takes an absolute path.
        let root = dir.path().canonicalize().unwrap();
        let profiles = root.join("profiles");
        fs::create_dir_all(&profiles).unwrap();
        fs::write(
            profiles.join("p.config"),
            "# the test profile\n[agentfs]\n\tpushInterval = 60s\n",
        )
        .unwrap();
        fs::write(profiles.join("p.exclude"), EXCLUDE).unwrap();
        Self::start_in(dir).await
    }

    /// The plugin started over `dir`, reconciling its records first, as
    /// `agentfs-csi` starts.
    async fn start_in(dir: tempfile::TempDir) -> Self {
        let root = dir.path().canonicalize().unwrap();
        let layout = Layout {
            base: root.join("base"),
            node_state: root.join("node"),
            profiles: root.join("profiles"),
        };
        let log = Arc::new(watch::channel(Vec::new()).0);
        let launcher = Arc::new(Tap {
            inner: AgentfsLauncher {
                bin: env!("CARGO_BIN_EXE_agentfs").into(),
            },
            log: log.clone(),
        });
        let supervisor = Supervisor::new(
            NodeConfig::new("node-1", layout.clone()),
            launcher,
            Arc::new(KernelMounter),
        );
        supervisor.reconcile().await;
        let socket = root.join("csi.sock");
        let bound = server::bind(&socket).unwrap();
        let (stop, stopped) = oneshot::channel::<()>();
        let task = tokio::spawn({
            let controller =
                ControllerService::new(layout.clone(), "node-1".into(), supervisor.clone());
            let node = NodeService::new(supervisor.clone());
            async move {
                bound
                    .serve(IdentityService::new(), controller, node, async {
                        let _ = stopped.await;
                    })
                    .await
                    .unwrap();
                supervisor.shutdown().await;
            }
        });
        let channel = Endpoint::try_from("http://[::]:50051")
            .unwrap()
            .connect_with_connector(service_fn(move |_: Uri| {
                let socket = socket.clone();
                async move { UnixStream::connect(socket).await.map(TokioIo::new) }
            }))
            .await
            .unwrap();
        Self {
            dir,
            layout,
            channel,
            log,
            stop: Some(stop),
            task: Some(task),
        }
    }

    fn controller(&self) -> ControllerClient<Channel> {
        ControllerClient::new(self.channel.clone())
    }

    fn node(&self) -> NodeClient<Channel> {
        NodeClient::new(self.channel.clone())
    }

    fn events(&self) -> Vec<String> {
        self.log.borrow().clone()
    }

    /// Waits until `count` events start with `prefix`.
    async fn until(&self, prefix: &str, count: usize) {
        let mut rx = self.log.subscribe();
        rx.wait_for(|l| l.iter().filter(|e| e.starts_with(prefix)).count() >= count)
            .await
            .unwrap();
    }

    /// The events so far that start with `prefix`. A child's exit is
    /// logged from the tap's own task, after the supervisor has seen it:
    /// wait for it with [`Driver::until`] before counting it.
    fn count(&self, prefix: &str) -> usize {
        self.events()
            .iter()
            .filter(|e| e.starts_with(prefix))
            .count()
    }

    fn target(&self, pod: &str) -> PathBuf {
        let target = self
            .layout
            .base
            .parent()
            .unwrap()
            .join("pods")
            .join(pod)
            .join("mount");
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        target
    }

    async fn stop(mut self) {
        let _ = self.stop.take().unwrap().send(());
        self.task.take().unwrap().await.unwrap();
    }

    /// The plugin stopped (its hosts' control channels closed) and started
    /// again over the same node.
    async fn restart(mut self) -> Self {
        let _ = self.stop.take().unwrap().send(());
        self.task.take().unwrap().await.unwrap();
        Self::start_in(self.dir).await
    }
}

fn mount_cap() -> VolumeCapability {
    VolumeCapability {
        access_mode: Some(AccessMode {
            mode: Mode::SingleNodeWriter as i32,
        }),
        access_type: Some(AccessType::Mount(MountVolume::default())),
    }
}

/// CreateVolume of `pvc-1` in namespace `apps`, claim `data`, profile `p`.
fn create_req(capacity: i64) -> CreateVolumeRequest {
    CreateVolumeRequest {
        name: "pvc-1".into(),
        capacity_range: Some(CapacityRange {
            required_bytes: capacity,
            limit_bytes: 0,
        }),
        volume_capabilities: vec![mount_cap()],
        parameters: [
            ("profile", "p"),
            ("csi.storage.k8s.io/pvc/name", "data"),
            ("csi.storage.k8s.io/pvc/namespace", "apps"),
            ("csi.storage.k8s.io/pv/name", "pvc-1"),
        ]
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect(),
        ..Default::default()
    }
}

/// CreateVolume of the fork claim `name` of the claim `source`, with the
/// fork class's `extra` parameters.
fn fork_req(name: &str, source: &str, extra: &[(&str, &str)]) -> CreateVolumeRequest {
    let mut parameters: HashMap<String, String> = [
        ("fork", "true"),
        ("csi.storage.k8s.io/pvc/name", "fork"),
        ("csi.storage.k8s.io/pvc/namespace", "apps"),
        ("csi.storage.k8s.io/pv/name", name),
    ]
    .iter()
    .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
    .collect();
    parameters.extend(
        extra
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned())),
    );
    CreateVolumeRequest {
        name: name.into(),
        volume_capabilities: vec![mount_cap()],
        parameters,
        volume_content_source: Some(VolumeContentSource {
            r#type: Some(volume_content_source::Type::Volume(
                volume_content_source::VolumeSource {
                    volume_id: source.into(),
                },
            )),
        }),
        ..Default::default()
    }
}

fn publish_req(target: &Path, context: HashMap<String, String>) -> NodePublishVolumeRequest {
    publish_claim_req("pvc-1", target, context)
}

fn publish_claim_req(
    claim: &str,
    target: &Path,
    context: HashMap<String, String>,
) -> NodePublishVolumeRequest {
    NodePublishVolumeRequest {
        volume_id: claim.into(),
        target_path: target.to_str().unwrap().into(),
        volume_capability: Some(mount_cap()),
        volume_context: context,
        ..Default::default()
    }
}

fn unpublish_req(target: &Path) -> NodeUnpublishVolumeRequest {
    unpublish_claim_req("pvc-1", target)
}

fn unpublish_claim_req(claim: &str, target: &Path) -> NodeUnpublishVolumeRequest {
    NodeUnpublishVolumeRequest {
        volume_id: claim.into(),
        target_path: target.to_str().unwrap().into(),
    }
}

fn delete_req() -> DeleteVolumeRequest {
    delete_claim_req("pvc-1")
}

fn delete_claim_req(claim: &str) -> DeleteVolumeRequest {
    DeleteVolumeRequest {
        volume_id: claim.into(),
        ..Default::default()
    }
}

/// The trees the source's hosts answered fork creates with, in order
/// (`fork/<n>`).
fn forked_trees(d: &Driver) -> Vec<String> {
    let prefix = "line host pvc-1 {\"forked\":{\"tree\":\"";
    d.events()
        .iter()
        .filter_map(|e| e.strip_prefix(prefix)?.split('"').next().map(str::to_owned))
        .collect()
}

/// The refs of the volume's history under `prefix`.
fn refs(state: &Path, prefix: &str) -> Vec<String> {
    let out = Command::new("git")
        .arg("--git-dir")
        .arg(state.join("history.git"))
        .args(["for-each-ref", "--format=%(refname)", prefix])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", text(&out.stderr));
    text(&out.stdout).lines().map(str::to_owned).collect()
}

/// The volume context the kubelet sends with `podInfoOnMount`.
fn pod_info(volume_context: &HashMap<String, String>) -> HashMap<String, String> {
    let mut context = volume_context.clone();
    for (k, v) in [
        ("csi.storage.k8s.io/pod.name", "writer-0"),
        ("csi.storage.k8s.io/pod.namespace", "apps"),
        ("csi.storage.k8s.io/pod.uid", "u-1"),
        ("csi.storage.k8s.io/serviceAccount.name", "default"),
    ] {
        context.insert(k.into(), v.into());
    }
    context
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Every commit message of the root's history that touched `path`.
fn messages(state: &Path, path: &str) -> String {
    let hash = Command::new("git")
        .args(["hash-object", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .and_then(|mut git| {
            git.stdin.take().unwrap().write_all(path.as_bytes())?;
            git.wait_with_output()
        })
        .unwrap();
    let hex = text(&hash.stdout).trim().to_owned();
    let out = Command::new("git")
        .arg("--git-dir")
        .arg(state.join("history.git"))
        .args(["log", "--format=%B", "root", "--"])
        .arg(format!("files/{}/{}/{path}", &hex[..2], &hex[2..4]))
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", text(&out.stderr));
    text(&out.stdout)
}

/// The pids of processes running `agentfs --state <state> host`: hosts,
/// not their children. A child the host forks (a git child of a builder
/// batch) carries its parent's command line until it execs, so one caught
/// between its fork and its exec matches too; it is dropped by its parent.
fn host_pids(state: &Path) -> Vec<i32> {
    let mut matched = Vec::new();
    for entry in fs::read_dir("/proc").unwrap().filter_map(Result::ok) {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<i32>() else {
            continue;
        };
        let Ok(cmdline) = fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        let args: Vec<&[u8]> = cmdline.split(|b| *b == 0).collect();
        let at_state = args.windows(3).any(|w| {
            w[0] == b"--state" && w[1] == state.as_os_str().as_encoded_bytes() && w[2] == b"host"
        });
        if at_state {
            let parent = fs::read_to_string(entry.path().join("status"))
                .ok()
                .and_then(|status| {
                    status
                        .lines()
                        .find_map(|line| line.strip_prefix("PPid:"))
                        .and_then(|ppid| ppid.trim().parse::<i32>().ok())
                });
            matched.push((pid, parent));
        }
    }
    not_forked_from_one(&matched)
}

/// The pids of `matched` (each with its parent's pid) whose parent is not
/// itself in `matched`.
fn not_forked_from_one(matched: &[(i32, Option<i32>)]) -> Vec<i32> {
    matched
        .iter()
        .filter(|(_, parent)| !matched.iter().any(|(pid, _)| Some(*pid) == *parent))
        .map(|(pid, _)| *pid)
        .collect()
}

/// A host's child caught between its fork and its exec matches the host's
/// command line: it is not counted as a second host.
#[test]
fn a_hosts_forked_child_is_not_a_second_host() {
    use std::io::BufRead;
    let state = tempfile::tempdir().unwrap();
    // `sh -c SCRIPT --state <state> host`: the shell's command line ends as
    // a host's does. Its subshell is a fork that never execs: it says so,
    // then waits for the end of its parent's stdin.
    let mut host = Command::new("sh")
        .args([
            "-c",
            "exec 3<&0; (echo forked; read -r line <&3; :) & wait",
            "--state",
        ])
        .arg(state.path())
        .arg("host")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut said = String::new();
    std::io::BufReader::new(host.stdout.take().unwrap())
        .read_line(&mut said)
        .unwrap();
    assert_eq!(said, "forked\n");
    let pids = host_pids(state.path());
    drop(host.stdin.take());
    assert!(host.wait().unwrap().success());
    assert_eq!(pids, vec![i32::try_from(host.id()).unwrap()]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_volume_makes_a_tree_volume_and_delete_keeps_it() {
    let d = Driver::start().await;
    let mut c = d.controller();
    let created = c
        .create_volume(create_req(1 << 30))
        .await
        .unwrap()
        .into_inner()
        .volume
        .unwrap();
    assert_eq!(created.volume_id, "pvc-1");
    assert_eq!(
        created.volume_context,
        HashMap::from([
            ("profile".to_owned(), "p".to_owned()),
            ("volume".to_owned(), "apps.data.pvc-1".to_owned())
        ])
    );
    let state = d.layout.state("pvc-1");
    let Found::Volume(identity) = TreeStateArea::at(&state).inspect().unwrap() else {
        panic!("no tree volume at {}", state.display());
    };
    assert_eq!(identity.name.as_str(), "apps.data.pvc-1");
    let config = fs::read_to_string(state.join("config")).unwrap();
    assert!(
        config.contains(&d.layout.profiles.join("p.config").display().to_string()),
        "{config}"
    );
    let installed = state.join("history.git/info/exclude");
    assert_eq!(fs::read_to_string(&installed).unwrap(), EXCLUDE);
    assert!(d.layout.csi_json("pvc-1").exists());
    assert_eq!(d.count("spawn init pvc-1"), 1);

    // Idempotent; another capacity is ALREADY_EXISTS.
    let again = c
        .create_volume(create_req(1 << 30))
        .await
        .unwrap()
        .into_inner()
        .volume
        .unwrap();
    assert_eq!(again, created);
    let err = c.create_volume(create_req(1 << 20)).await.unwrap_err();
    assert_eq!(err.code(), Code::AlreadyExists, "{err:?}");

    // The operator edits the profile: the host DeleteVolume starts
    // installs the rules again before it opens the volume.
    fs::write(d.layout.profiles.join("p.exclude"), "*.log\n").unwrap();
    c.delete_volume(delete_req()).await.unwrap();
    assert_eq!(d.count("spawn host pvc-1"), 1, "{:?}", d.events());
    d.until("exit host pvc-1 exit code 0", 1).await;
    assert_eq!(
        d.count("exit host pvc-1 exit code 0"),
        1,
        "the host drained and Volume released the volume: {:?}",
        d.events()
    );
    assert_eq!(fs::read_to_string(&installed).unwrap(), "*.log\n");
    assert!(!d.layout.csi_json("pvc-1").exists());
    assert!(d.layout.retained_json("pvc-1").exists());
    assert!(matches!(
        TreeStateArea::at(&state).inspect().unwrap(),
        Found::Volume(_)
    ));
    // Gone already: done.
    c.delete_volume(delete_req()).await.unwrap();
    assert_eq!(d.count("spawn host pvc-1"), 1);
    d.stop().await;
}

/// A fork claim is a keyed fork, under its handle, in its source's volume,
/// made on a host started for the call and stopped after it; a repeat
/// answers the same; DeleteVolume ends the fork by key, removes the
/// claim's directory, and is idempotent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fork_claim_is_a_keyed_fork_and_delete_ends_it() {
    let d = Driver::start().await;
    let mut c = d.controller();
    c.create_volume(create_req(1 << 30)).await.unwrap();
    let state = d.layout.state("pvc-1");
    let req = fork_req(
        "pvc-2",
        "pvc-1",
        &[("landing", "automatic"), ("link", "linked")],
    );
    let created = c
        .create_volume(req.clone())
        .await
        .unwrap()
        .into_inner()
        .volume
        .unwrap();
    assert_eq!(created.volume_id, "pvc-2");
    assert_eq!(
        created.volume_context,
        HashMap::from([("source".to_owned(), "pvc-1".to_owned())])
    );
    assert_eq!(created.content_source, req.volume_content_source);
    let trees = forked_trees(&d);
    assert_eq!(trees.len(), 1, "{:?}", d.events());
    let fork_ref = format!("refs/agentfs/trees/{}", trees[0]);
    assert_eq!(refs(&state, &fork_ref), vec![fork_ref.clone()]);
    assert_eq!(d.count("spawn host pvc-1"), 1);
    d.until("exit host pvc-1 exit code 0", 1).await;
    assert_eq!(
        d.count("exit host pvc-1 exit code 0"),
        1,
        "the host started for the call stopped after it: {:?}",
        d.events()
    );
    let record = fs::read_to_string(d.layout.csi_json("pvc-2")).unwrap();
    assert!(record.contains("\"kind\": \"fork\""), "{record}");
    assert!(record.contains("\"source\": \"pvc-1\""), "{record}");
    assert!(
        !d.layout.state("pvc-2").exists(),
        "a fork claim has no state"
    );

    // Idempotent: the same answer, no host.
    let again = c
        .create_volume(req.clone())
        .await
        .unwrap()
        .into_inner()
        .volume
        .unwrap();
    assert_eq!(again, created);
    assert_eq!(d.count("spawn host pvc-1"), 1);

    c.delete_volume(delete_claim_req("pvc-2")).await.unwrap();
    assert!(!d.layout.volume_dir("pvc-2").exists());
    // The fork ended in the store: its key is not made again.
    let err = c.create_volume(req).await.unwrap_err();
    assert_eq!(err.code(), Code::AlreadyExists, "{err:?}");
    assert!(err.message().contains("ended"), "{}", err.message());
    assert!(!d.layout.csi_json("pvc-2").exists());
    // A host for each call, each stopped after it.
    assert_eq!(d.count("spawn host pvc-1"), 3);
    d.until("exit host pvc-1 exit code 0", 3).await;
    assert_eq!(d.count("exit host pvc-1 exit code 0"), 3);
    c.delete_volume(delete_claim_req("pvc-2")).await.unwrap();
    assert_eq!(d.count("spawn host pvc-1"), 3, "gone already: no host");
    d.stop().await;
}

/// Trees refuses a fork claim that needs approval, writing nothing, and a
/// fork of a fork claim; each is INVALID_ARGUMENT with Trees' words.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn approvals_and_forks_of_forks_are_refused_by_trees() {
    let d = Driver::start().await;
    let mut c = d.controller();
    c.create_volume(create_req(1 << 30)).await.unwrap();
    let state = d.layout.state("pvc-1");
    let before = refs(&state, "refs/agentfs/trees/");
    let err = c
        .create_volume(fork_req("pvc-2", "pvc-1", &[("landing", "approval")]))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::InvalidArgument, "{err:?}");
    assert!(
        err.message().contains("landing approvals are not built"),
        "{}",
        err.message()
    );
    assert!(!d.layout.csi_json("pvc-2").exists());
    assert_eq!(refs(&state, "refs/agentfs/trees/"), before, "no fork made");
    assert!(forked_trees(&d).is_empty(), "{:?}", d.events());

    c.create_volume(fork_req("pvc-3", "pvc-1", &[]))
        .await
        .unwrap();
    assert_eq!(forked_trees(&d).len(), 1, "{:?}", d.events());
    assert_eq!(refs(&state, "refs/agentfs/trees/").len(), before.len() + 1);

    let err = c
        .create_volume(fork_req("pvc-4", "pvc-3", &[]))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::InvalidArgument, "{err:?}");
    assert!(
        err.message().contains("forks of forks are not built"),
        "{}",
        err.message()
    );
    assert!(!d.layout.csi_json("pvc-4").exists());
    assert_eq!(forked_trees(&d).len(), 1);
    d.stop().await;
}

/// The recording driver's volume directory: never served.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_recording_format_volume_is_not_published() {
    let d = Driver::start().await;
    let state = d.layout.state("pvc-1");
    fs::create_dir_all(&state).unwrap();
    fs::write(state.join("journal"), "recording\n").unwrap();
    let target = d.target("p0");
    let err = d
        .node()
        .node_publish_volume(publish_req(&target, HashMap::new()))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::FailedPrecondition, "{err:?}");
    assert!(
        err.message().contains("recording-format"),
        "{}",
        err.message()
    );
    assert_eq!(d.count("spawn"), 0, "{:?}", d.events());
    d.stop().await;
}

/// The root tests, one after the other: ci/fuse-tests.sh runs this one
/// ignored test and checks that it ran.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs root and /dev/fuse: ci/fuse-tests.sh"]
async fn views_at_the_kubelet_target() {
    if !root_and_fuse() {
        return;
    }
    a_pod_view_at_the_kubelet_target().await;
    a_fork_claims_view_beside_its_source().await;
    a_held_close_stalls_only_its_claim().await;
}

fn root_and_fuse() -> bool {
    if !rustix::process::geteuid().is_root() || !Path::new("/dev/fuse").exists() {
        eprintln!("skipped: needs root and /dev/fuse (ci/fuse-tests.sh)");
        return false;
    }
    true
}

/// A pod's view at the kubelet target: a live agentfs mount, its writes
/// committed with the pod's trailers; one pod per claim; a killed host
/// comes back with the view live; NodeUnpublish leaves nothing; and
/// DeleteVolume keeps the volume.
async fn a_pod_view_at_the_kubelet_target() {
    let d = Driver::start().await;
    let created = d
        .controller()
        .create_volume(create_req(1 << 30))
        .await
        .unwrap()
        .into_inner()
        .volume
        .unwrap();
    let context = pod_info(&created.volume_context);
    let state = d.layout.state("pvc-1");
    let target = d.target("p0");
    // The operator edits the profile: the host the publish starts installs
    // the rules again before it opens the volume.
    fs::write(d.layout.profiles.join("p.exclude"), "*.log\nscratch/\n").unwrap();
    let mut n = d.node();
    n.node_publish_volume(publish_req(&target, context.clone()))
        .await
        .unwrap();
    assert_eq!(
        fs::read_to_string(state.join("history.git/info/exclude")).unwrap(),
        "*.log\nscratch/\n"
    );
    assert_eq!(
        mount_state(&target).unwrap(),
        MountState::Live {
            source: "agentfs:apps.data.pvc-1".into()
        }
    );
    fs::write(target.join("notes.md"), "first\n").unwrap();

    // One pod per claim; the same target again is done already.
    let other = d.target("p1");
    let err = n
        .node_publish_volume(publish_req(&other, context.clone()))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::FailedPrecondition, "{err:?}");
    assert!(
        err.message()
            .contains("several pods on one volume are not built"),
        "{}",
        err.message()
    );
    n.node_publish_volume(publish_req(&target, context.clone()))
        .await
        .unwrap();
    assert_eq!(d.count("spawn host pvc-1"), 1);

    // A killed host comes back: its view opened again over the stale mount.
    let pids = host_pids(&state);
    assert_eq!(pids.len(), 1, "{pids:?}");
    let killed = Command::new("kill")
        .args(["-9", &pids[0].to_string()])
        .status()
        .unwrap();
    assert!(killed.success());
    d.until("exit host pvc-1 killed by SIGKILL", 1).await;
    d.until("line host pvc-1 {\"reconciled\"", 2).await;
    assert_eq!(d.count("spawn host pvc-1"), 2);
    assert!(
        matches!(mount_state(&target).unwrap(), MountState::Live { .. }),
        "{:?}",
        mount_state(&target)
    );
    assert_eq!(
        fs::read_to_string(target.join("notes.md")).unwrap(),
        "first\n"
    );
    fs::write(target.join("after.md"), "after the restart\n").unwrap();

    n.node_unpublish_volume(unpublish_req(&target))
        .await
        .unwrap();
    assert!(!target.exists(), "the target directory is removed");
    assert_eq!(
        agentfs_fuse::stale::agentfs_layers(&target).unwrap_or(0),
        0,
        "no agentfs mount is left"
    );
    d.until("exit host pvc-1 exit code 0", 1).await;
    assert_eq!(
        d.count("exit host pvc-1 exit code 0"),
        1,
        "the host stopped after its last view: {:?}",
        d.events()
    );
    let first = messages(&state, "notes.md");
    assert!(first.contains("Agentfs-Pod: writer-0"), "{first}");
    assert!(first.contains("Agentfs-Namespace: apps"), "{first}");
    assert!(
        messages(&state, "after.md").contains("Agentfs-Pod: writer-0"),
        "the restarted view kept the pod"
    );

    d.controller().delete_volume(delete_req()).await.unwrap();
    assert!(d.layout.retained_json("pvc-1").exists());
    d.until("exit host pvc-1 exit code 0", 2).await;
    assert_eq!(d.count("exit host pvc-1 exit code 0"), 2);
    let _ = &d.dir;
    d.stop().await;
}

/// A fork claim's pod beside its source's, on one host: its write lands in
/// the root with its own pod; one pod per fork claim; the source is in use
/// while only the fork's view is open; once the fork's view has closed, the
/// fork has had its writer.
async fn a_fork_claims_view_beside_its_source() {
    let d = Driver::start().await;
    let mut c = d.controller();
    let source = c
        .create_volume(create_req(1 << 30))
        .await
        .unwrap()
        .into_inner()
        .volume
        .unwrap();
    let fork = c
        .create_volume(fork_req("pvc-2", "pvc-1", &[]))
        .await
        .unwrap()
        .into_inner()
        .volume
        .unwrap();
    let tree = forked_trees(&d).pop().unwrap();
    let state = d.layout.state("pvc-1");
    let (a, b) = (d.target("a"), d.target("b"));
    let mut n = d.node();
    n.node_publish_volume(publish_req(&a, pod_info(&source.volume_context)))
        .await
        .unwrap();
    let mut fork_pod = fork.volume_context.clone();
    for (k, v) in [
        ("csi.storage.k8s.io/pod.name", "forker-0"),
        ("csi.storage.k8s.io/pod.namespace", "apps"),
        ("csi.storage.k8s.io/pod.uid", "u-2"),
    ] {
        fork_pod.insert(k.into(), v.into());
    }
    n.node_publish_volume(publish_claim_req("pvc-2", &b, fork_pod.clone()))
        .await
        .unwrap();
    // CreateVolume's host, then one host for both views.
    assert_eq!(d.count("spawn host pvc-1"), 2, "{:?}", d.events());
    assert_eq!(
        mount_state(&b).unwrap(),
        MountState::Live {
            source: format!("agentfs:apps.data.pvc-1:{tree}")
        }
    );
    fs::write(b.join("forked.md"), "from the fork\n").unwrap();

    // A killed host comes back with both views, the fork's writer rebound.
    let pids = host_pids(&state);
    assert_eq!(pids.len(), 1, "{pids:?}");
    let killed = Command::new("kill")
        .args(["-9", &pids[0].to_string()])
        .status()
        .unwrap();
    assert!(killed.success());
    d.until("exit host pvc-1 killed by SIGKILL", 1).await;
    d.until("line host pvc-1 {\"reconciled\"", 3).await;
    for at in [&a, &b] {
        assert!(
            matches!(mount_state(at).unwrap(), MountState::Live { .. }),
            "{at:?}: {:?}",
            d.events()
        );
    }
    fs::write(b.join("after-kill.md"), "after the kill\n").unwrap();

    // The plugin restarts with both pods running: the host leaves their
    // mounts stale, and the new plugin's host stacks over them, the fork's
    // writer rebound, not taken.
    let d = d.restart().await;
    let mut c = d.controller();
    let mut n = d.node();
    for at in [&a, &b] {
        assert!(
            matches!(mount_state(at).unwrap(), MountState::Live { .. }),
            "{at:?}: {:?}",
            d.events()
        );
    }
    assert_eq!(
        fs::read_to_string(b.join("after-kill.md")).unwrap(),
        "after the kill\n"
    );
    n.node_publish_volume(publish_claim_req("pvc-2", &b, fork_pod.clone()))
        .await
        .unwrap();
    fs::write(b.join("after-restart.md"), "after the restart\n").unwrap();

    // One pod per fork claim.
    let other = d.target("c");
    let err = n
        .node_publish_volume(publish_claim_req("pvc-2", &other, fork_pod.clone()))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::FailedPrecondition, "{err:?}");
    assert!(
        err.message()
            .contains("several pods on one volume are not built"),
        "{}",
        err.message()
    );

    // The source's own pod gone, the fork's keeps it in use.
    n.node_unpublish_volume(unpublish_req(&a)).await.unwrap();
    let err = c.delete_volume(delete_req()).await.unwrap_err();
    assert_eq!(err.code(), Code::FailedPrecondition, "{err:?}");
    assert_eq!(
        err.message(),
        format!(
            "volume pvc-1 is in use on this node (published at {}); nothing deleted",
            b.display()
        )
    );
    assert!(d.layout.csi_json("pvc-1").exists(), "nothing deleted");

    // The last view closed: the host drains, and Volume releases only with
    // every queue empty, so its exit is the landing's event.
    n.node_unpublish_volume(unpublish_claim_req("pvc-2", &b))
        .await
        .unwrap();
    // This plugin's log starts at the restart: its one host has exited.
    d.until("exit host pvc-1 exit code 0", 1).await;
    assert_eq!(
        d.count("exit host pvc-1 exit code 0"),
        1,
        "{:?}",
        d.events()
    );
    for file in ["forked.md", "after-kill.md", "after-restart.md"] {
        let landed = messages(&state, file);
        assert!(landed.contains("Agentfs-Pod: forker-0"), "{file}: {landed}");
    }

    // The fork has had its writer.
    let err = n
        .node_publish_volume(publish_claim_req("pvc-2", &b, fork_pod))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::FailedPrecondition, "{err:?}");
    assert_eq!(
        err.message(),
        "the fork of claim pvc-2 has had its writer: a fork claim serves one pod (docs/csi.md)"
    );

    // Through the source's view.
    n.node_publish_volume(publish_req(&a, pod_info(&source.volume_context)))
        .await
        .unwrap();
    assert_eq!(
        fs::read_to_string(a.join("forked.md")).unwrap(),
        "from the fork\n"
    );
    n.node_unpublish_volume(unpublish_req(&a)).await.unwrap();
    c.delete_volume(delete_claim_req("pvc-2")).await.unwrap();
    c.delete_volume(delete_req()).await.unwrap();
    assert!(d.layout.retained_json("pvc-1").exists());
    d.stop().await;
}

/// A file held open in claim A's view keeps A's NodeUnpublish pending (its
/// close waits for the view's end). Meanwhile a second NodeUnpublish of A
/// is ABORTED at once, and a NodePublish of claim B, a fork claim on the
/// same volume and host, completes with its view live. Releasing the file
/// completes A's NodeUnpublish.
async fn a_held_close_stalls_only_its_claim() {
    let d = Driver::start().await;
    let mut c = d.controller();
    let source = c
        .create_volume(create_req(1 << 30))
        .await
        .unwrap()
        .into_inner()
        .volume
        .unwrap();
    let fork = c
        .create_volume(fork_req("pvc-2", "pvc-1", &[]))
        .await
        .unwrap()
        .into_inner()
        .volume
        .unwrap();
    let (a, b) = (d.target("a"), d.target("b"));
    let mut n = d.node();
    n.node_publish_volume(publish_req(&a, pod_info(&source.volume_context)))
        .await
        .unwrap();
    fs::write(a.join("held.md"), "held\n").unwrap();
    let held = fs::File::open(a.join("held.md")).unwrap();
    let unpublish = tokio::spawn({
        let mut n = d.node();
        let a = a.clone();
        async move { n.node_unpublish_volume(unpublish_req(&a)).await }
    });
    d.until("send host pvc-1 {\"op\":\"close\"", 1).await;
    let err = n
        .node_unpublish_volume(unpublish_req(&a))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::Aborted, "{err:?}");

    let mut fork_pod = fork.volume_context.clone();
    for (k, v) in [
        ("csi.storage.k8s.io/pod.name", "forker-1"),
        ("csi.storage.k8s.io/pod.namespace", "apps"),
        ("csi.storage.k8s.io/pod.uid", "u-3"),
    ] {
        fork_pod.insert(k.into(), v.into());
    }
    n.node_publish_volume(publish_claim_req("pvc-2", &b, fork_pod))
        .await
        .unwrap();
    assert!(
        matches!(mount_state(&b).unwrap(), MountState::Live { .. }),
        "{:?}",
        d.events()
    );
    assert!(!unpublish.is_finished(), "A's close is still pending");

    drop(held);
    unpublish.await.unwrap().unwrap();
    assert!(!a.exists(), "the target directory is removed");
    n.node_unpublish_volume(unpublish_claim_req("pvc-2", &b))
        .await
        .unwrap();
    d.until("exit host pvc-1 exit code 0", 2).await;
    c.delete_volume(delete_claim_req("pvc-2")).await.unwrap();
    c.delete_volume(delete_req()).await.unwrap();
    d.stop().await;
}
