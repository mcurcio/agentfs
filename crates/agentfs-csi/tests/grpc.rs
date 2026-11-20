//! The CSI gRPC services over a Unix socket: Identity, Controller and Node
//! validation with the error codes csi-sanity expects, CreateVolume's
//! refusals, a recording-format volume refused by DeleteVolume and
//! NodePublish, ValidateVolumeCapabilities, and the server's stop. The
//! real `agentfs` binary is driven by crates/agentfs/tests/csi_trees.rs.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use hyper_util::rt::TokioIo;
use tokio::net::UnixStream;
use tokio::sync::oneshot;
use tonic::Code;
use tonic::transport::{Channel, Endpoint, Uri};
use tower::service_fn;

use agentfs_contracts::name::VolumeName;
use agentfs_csi::controller::ControllerService;
use agentfs_csi::csi::v1::controller_client::ControllerClient;
use agentfs_csi::csi::v1::identity_client::IdentityClient;
use agentfs_csi::csi::v1::node_client::NodeClient;
use agentfs_csi::csi::v1::volume_capability::access_mode::Mode;
use agentfs_csi::csi::v1::volume_capability::{AccessMode, AccessType, BlockVolume, MountVolume};
use agentfs_csi::csi::v1::*;
use agentfs_csi::identity::{IdentityService, TOPOLOGY_KEY};
use agentfs_csi::launcher::AgentfsLauncher;
use agentfs_csi::layout::Layout;
use agentfs_csi::mounter::KernelMounter;
use agentfs_csi::node::NodeService;
use agentfs_csi::records::{self, ClaimKind, ClaimRecord, RECORD_VERSION};
use agentfs_csi::server;
use agentfs_csi::supervisor::{NodeConfig, Supervisor};
use agentfs_csi::volume::MULTI_NODE;

struct Driver {
    dir: tempfile::TempDir,
    layout: Layout,
    channel: Channel,
    stop: Option<oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl Driver {
    async fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout {
            base: dir.path().join("base"),
            node_state: dir.path().join("node"),
            profiles: dir.path().join("profiles"),
        };
        fs::create_dir_all(&layout.profiles).unwrap();
        fs::write(layout.profiles.join("p.config"), "").unwrap();
        let socket = dir.path().join("csi.sock");
        let (stop, stopped) = oneshot::channel::<()>();
        // These tests never get as far as a child that must succeed.
        let launcher = Arc::new(AgentfsLauncher {
            bin: "/bin/false".into(),
        });
        let supervisor = Supervisor::new(
            NodeConfig::new("node-1", layout.clone()),
            launcher,
            Arc::new(KernelMounter),
        );
        let bound = server::bind(&socket).unwrap();
        let task = tokio::spawn({
            let controller =
                ControllerService::new(layout.clone(), "node-1".into(), supervisor.clone());
            let node = NodeService::new(supervisor);
            async move {
                bound
                    .serve(IdentityService::new(), controller, node, async {
                        let _ = stopped.await;
                    })
                    .await
                    .unwrap();
            }
        });
        let channel = connect(&socket).await;
        Self {
            dir,
            layout,
            channel,
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

    /// A claim record for `handle`, as CreateVolume writes it.
    fn claim(&self, handle: &str) {
        records::store_claim(
            &self.layout,
            handle,
            &ClaimRecord {
                v: RECORD_VERSION,
                capacity_bytes: 1 << 30,
                claim: ClaimKind::Volume {
                    name: VolumeName::parse(handle).unwrap(),
                    profile: "p".into(),
                },
            },
        )
        .unwrap();
    }

    /// The recording driver's volume directory: a state area of another
    /// format (an unrelated non-empty file in it), and its `csi.json`.
    fn recording_fixture(&self, handle: &str) {
        let state = self.layout.state(handle);
        fs::create_dir_all(&state).unwrap();
        fs::write(state.join("journal"), "recording\n").unwrap();
        fs::write(
            self.layout.csi_json(handle),
            r#"{"params":{"profile":"p","volume":"v","on_delete":"retain"},"capacity_bytes":1,"created":1}"#,
        )
        .unwrap();
    }

    async fn stop(mut self) {
        let _ = self.stop.take().unwrap().send(());
        self.task.take().unwrap().await.unwrap();
        assert!(!self.dir.path().join("csi.sock").exists());
    }
}

/// A channel to a socket `server::bind` has returned: it is listening, so
/// the first connect succeeds.
async fn connect(socket: &Path) -> Channel {
    let socket: PathBuf = socket.to_path_buf();
    Endpoint::try_from("http://[::]:50051")
        .unwrap()
        .connect_with_connector(service_fn(move |_: Uri| {
            let socket = socket.clone();
            async move { UnixStream::connect(socket).await.map(TokioIo::new) }
        }))
        .await
        .unwrap()
}

fn mount_cap(mode: Mode) -> VolumeCapability {
    VolumeCapability {
        access_mode: Some(AccessMode { mode: mode as i32 }),
        access_type: Some(AccessType::Mount(MountVolume::default())),
    }
}

fn create_req(name: &str, params: &[(&str, &str)]) -> CreateVolumeRequest {
    CreateVolumeRequest {
        name: name.into(),
        capacity_range: Some(CapacityRange {
            required_bytes: 1 << 30,
            limit_bytes: 0,
        }),
        volume_capabilities: vec![mount_cap(Mode::SingleNodeWriter)],
        parameters: params
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect(),
        ..Default::default()
    }
}

fn delete_req(id: &str) -> DeleteVolumeRequest {
    DeleteVolumeRequest {
        volume_id: id.into(),
        ..Default::default()
    }
}

fn topology(node: &str) -> Topology {
    Topology {
        segments: HashMap::from([(TOPOLOGY_KEY.to_owned(), node.to_owned())]),
    }
}

fn node_publish_req(id: &str, target: &str) -> NodePublishVolumeRequest {
    NodePublishVolumeRequest {
        volume_id: id.into(),
        target_path: target.into(),
        volume_capability: Some(mount_cap(Mode::SingleNodeWriter)),
        ..Default::default()
    }
}

#[track_caller]
fn code<T: std::fmt::Debug>(r: Result<T, tonic::Status>) -> Code {
    r.expect_err("expected an error").code()
}

#[tokio::test]
async fn identity() {
    let d = Driver::start().await;
    let mut id = IdentityClient::new(d.channel.clone());
    let info = id
        .get_plugin_info(GetPluginInfoRequest {})
        .await
        .unwrap()
        .into_inner();
    assert_eq!(info.name, agentfs_csi::DRIVER_NAME);
    assert_eq!(info.vendor_version, env!("CARGO_PKG_VERSION"));
    let caps = id
        .get_plugin_capabilities(GetPluginCapabilitiesRequest {})
        .await
        .unwrap()
        .into_inner();
    let services: Vec<i32> = caps
        .capabilities
        .iter()
        .filter_map(|c| match &c.r#type {
            Some(plugin_capability::Type::Service(s)) => Some(s.r#type),
            _ => None,
        })
        .collect();
    use plugin_capability::service::Type as Service;
    assert_eq!(
        services,
        [
            Service::ControllerService as i32,
            Service::VolumeAccessibilityConstraints as i32
        ]
    );
    let probe = id.probe(ProbeRequest {}).await.unwrap().into_inner();
    assert_eq!(probe.ready, Some(true));

    let caps = d
        .controller()
        .controller_get_capabilities(ControllerGetCapabilitiesRequest {})
        .await
        .unwrap()
        .into_inner();
    use controller_service_capability::rpc::Type as Rpc;
    let rpcs: Vec<i32> = caps
        .capabilities
        .iter()
        .filter_map(|c| match &c.r#type {
            Some(controller_service_capability::Type::Rpc(r)) => Some(r.r#type),
            _ => None,
        })
        .collect();
    assert_eq!(
        rpcs,
        [
            Rpc::CreateDeleteVolume as i32,
            Rpc::SingleNodeMultiWriter as i32,
            Rpc::CloneVolume as i32
        ],
        "no PUBLISH_UNPUBLISH_VOLUME: there is no attach step"
    );
    // Everything else is UNIMPLEMENTED, ControllerPublish included.
    let r = d
        .controller()
        .list_volumes(ListVolumesRequest::default())
        .await;
    assert_eq!(code(r), Code::Unimplemented);
    let r = d
        .controller()
        .controller_publish_volume(ControllerPublishVolumeRequest::default())
        .await;
    assert_eq!(code(r), Code::Unimplemented);
    d.stop().await;
}

#[tokio::test]
async fn create_volume_refuses_what_it_does_not_support() {
    let d = Driver::start().await;
    let mut c = d.controller();
    let ok = create_req("v", &[("profile", "p")]);
    let mut cases: Vec<(&str, CreateVolumeRequest, Option<&str>)> = Vec::new();
    let mut r = ok.clone();
    r.name = String::new();
    cases.push(("no name", r, None));
    let mut r = ok.clone();
    r.volume_capabilities.clear();
    cases.push(("no capabilities", r, None));
    let mut r = ok.clone();
    r.volume_capabilities = vec![mount_cap(Mode::MultiNodeMultiWriter)];
    cases.push(("RWX", r, Some(MULTI_NODE)));
    let mut r = ok.clone();
    r.volume_capabilities[0].access_type = Some(AccessType::Block(BlockVolume {}));
    cases.push(("block", r, None));
    let mut r = ok.clone();
    r.volume_capabilities[0].access_type = Some(AccessType::Mount(MountVolume {
        mount_flags: vec!["noexec".into()],
        ..Default::default()
    }));
    cases.push(("mount flags", r, None));
    let mut r = ok.clone();
    r.volume_content_source = Some(VolumeContentSource::default());
    cases.push(("content source naming nothing", r, None));
    let mut r = ok.clone();
    r.volume_content_source = Some(VolumeContentSource {
        r#type: Some(volume_content_source::Type::Snapshot(
            volume_content_source::SnapshotSource {
                snapshot_id: "s".into(),
            },
        )),
    });
    cases.push(("snapshot", r, Some("snapshots are not supported")));
    let clone = Some(VolumeContentSource {
        r#type: Some(volume_content_source::Type::Volume(
            volume_content_source::VolumeSource {
                volume_id: "src".into(),
            },
        )),
    });
    let mut r = ok.clone();
    r.volume_content_source = clone.clone();
    cases.push((
        "clone into an ordinary class",
        r,
        Some("cloning needs a fork class (fork: \"true\")"),
    ));
    let mut r = create_req("v", &[("fork", "true")]);
    r.volume_content_source = clone;
    cases.push(("a fork claim", r, Some("fork claims are not built yet")));
    cases.push((
        "onDelete: delete",
        create_req("v", &[("profile", "p"), ("onDelete", "delete")]),
        Some(
            "onDelete: delete is not built on tree volumes: a deleted claim's volume is kept, \
             because its history is pushed nowhere yet",
        ),
    ));
    cases.push((
        "startFrom",
        create_req(
            "v",
            &[("profile", "p"), ("startFrom", "origin:agentfs/x@1")],
        ),
        Some(
            "startFrom is not built on tree volumes: a volume starts empty; moving a volume \
             between nodes is not built (docs/runbooks/moving.md)",
        ),
    ));
    cases.push((
        "workloadMounts",
        create_req("v", &[("profile", "p"), ("workloadMounts", "/workspace")]),
        Some(
            "workloadMounts is not built on tree volumes: tree volumes keep nested repositories \
             untracked",
        ),
    ));
    cases.push((
        "unknown parameter",
        create_req("v", &[("profile", "p"), ("x", "y")]),
        Some("unknown StorageClass parameter \"x\" (known: profile, volumeName, onDelete, fork)"),
    ));
    cases.push((
        "a name without {pvName}",
        create_req("v", &[("profile", "p"), ("volumeName", "fixed")]),
        None,
    ));
    cases.push(("no profile", create_req("v", &[]), None));
    cases.push((
        "missing profile",
        create_req("v", &[("profile", "nope")]),
        None,
    ));
    cases.push(("bad handle", create_req("a/b", &[("profile", "p")]), None));
    for (what, req, message) in cases {
        let err = c.create_volume(req).await.unwrap_err();
        assert_eq!(err.code(), Code::InvalidArgument, "{what}: {err:?}");
        if let Some(message) = message {
            assert_eq!(err.message(), message, "{what}");
        }
    }
    assert!(!d.layout.volume_dir("v").exists(), "nothing was made");
    d.stop().await;
}

/// A recording-format volume (a state area of another format) is never
/// served or deleted: DeleteVolume and NodePublish refuse it, naming the
/// moving runbook, and leave its directory; CreateVolume of its name is
/// ALREADY_EXISTS.
#[tokio::test]
async fn a_recording_format_volume_is_refused_and_left() {
    let d = Driver::start().await;
    d.recording_fixture("old");
    let err = d
        .controller()
        .delete_volume(delete_req("old"))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::FailedPrecondition, "{err:?}");
    assert_eq!(
        err.message(),
        format!(
            "volume old is a recording-format volume ({}): this driver never serves or deletes \
             it; archive it and remove the directory by hand (docs/runbooks/moving.md)",
            d.layout.volume_dir("old").display()
        )
    );
    assert!(d.layout.state("old").join("journal").exists());
    assert!(d.layout.csi_json("old").exists());
    let target = d.dir.path().join("pods/x/mount");
    let err = d
        .node()
        .node_publish_volume(node_publish_req("old", target.to_str().unwrap()))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::FailedPrecondition, "{err:?}");
    assert!(
        err.message().contains("recording-format"),
        "{}",
        err.message()
    );
    let err = d
        .controller()
        .create_volume(create_req("old", &[("profile", "p")]))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::AlreadyExists, "{err:?}");
    assert!(
        err.message().contains("recording-format"),
        "{}",
        err.message()
    );
    // Nothing of ours, nothing to delete.
    d.controller()
        .delete_volume(delete_req("gone"))
        .await
        .unwrap();
    d.controller()
        .delete_volume(delete_req("a/b"))
        .await
        .unwrap();
    assert_eq!(
        code(d.controller().delete_volume(delete_req("")).await),
        Code::InvalidArgument
    );
    d.stop().await;
}

#[tokio::test]
async fn validate_volume_capabilities() {
    let d = Driver::start().await;
    let mut c = d.controller();
    d.claim("v");
    let req = |id: &str, caps: Vec<VolumeCapability>| ValidateVolumeCapabilitiesRequest {
        volume_id: id.into(),
        volume_capabilities: caps,
        ..Default::default()
    };
    let ok = c
        .validate_volume_capabilities(req("v", vec![mount_cap(Mode::SingleNodeMultiWriter)]))
        .await
        .unwrap()
        .into_inner();
    assert!(ok.confirmed.is_some());
    for mode in [
        Mode::MultiNodeReaderOnly,
        Mode::MultiNodeSingleWriter,
        Mode::MultiNodeMultiWriter,
    ] {
        let no = c
            .validate_volume_capabilities(req("v", vec![mount_cap(mode)]))
            .await
            .unwrap()
            .into_inner();
        assert!(no.confirmed.is_none(), "{mode:?}");
        assert_eq!(no.message, MULTI_NODE, "{mode:?}");
    }
    let r = c
        .validate_volume_capabilities(req("nope", vec![mount_cap(Mode::SingleNodeWriter)]))
        .await;
    assert_eq!(code(r), Code::NotFound);
    let r = c.validate_volume_capabilities(req("v", vec![])).await;
    assert_eq!(code(r), Code::InvalidArgument);
    let r = c
        .validate_volume_capabilities(req("", vec![mount_cap(Mode::SingleNodeWriter)]))
        .await;
    assert_eq!(code(r), Code::InvalidArgument);
    d.stop().await;
}

#[tokio::test]
async fn node_calls_are_validated() {
    let d = Driver::start().await;
    let mut n = d.node();
    let info = n
        .node_get_info(NodeGetInfoRequest {})
        .await
        .unwrap()
        .into_inner();
    assert_eq!(info.node_id, "node-1");
    assert_eq!(info.accessible_topology, Some(topology("node-1")));
    let caps = n
        .node_get_capabilities(NodeGetCapabilitiesRequest {})
        .await
        .unwrap()
        .into_inner();
    use node_service_capability::rpc::Type as Rpc;
    let rpcs: Vec<i32> = caps
        .capabilities
        .iter()
        .filter_map(|c| match &c.r#type {
            Some(node_service_capability::Type::Rpc(r)) => Some(r.r#type),
            _ => None,
        })
        .collect();
    assert_eq!(
        rpcs,
        [Rpc::SingleNodeMultiWriter as i32],
        "no STAGE_UNSTAGE_VOLUME"
    );
    let r = n.node_stage_volume(NodeStageVolumeRequest::default()).await;
    assert_eq!(code(r), Code::Unimplemented);
    let r = n
        .node_unstage_volume(NodeUnstageVolumeRequest::default())
        .await;
    assert_eq!(code(r), Code::Unimplemented);

    let ephemeral = HashMap::from([("csi.storage.k8s.io/ephemeral".to_owned(), "true".to_owned())]);
    let mut publish_cases = vec![
        ("no id", node_publish_req("", "/t")),
        ("no target", node_publish_req("v", "")),
        ("relative target", node_publish_req("v", "t")),
    ];
    let mut r = node_publish_req("v", "/t");
    r.volume_capability = None;
    publish_cases.push(("no capability", r));
    let mut r = node_publish_req("v", "/t");
    r.volume_capability = Some(mount_cap(Mode::MultiNodeMultiWriter));
    publish_cases.push(("RWX", r));
    let mut r = node_publish_req("v", "/t");
    r.volume_capability.as_mut().unwrap().access_type = Some(AccessType::Block(BlockVolume {}));
    publish_cases.push(("block", r));
    let mut r = node_publish_req("v", "/t");
    r.volume_context = ephemeral;
    publish_cases.push(("ephemeral", r));
    for (what, req) in publish_cases {
        let err = n.node_publish_volume(req).await.unwrap_err();
        assert_eq!(err.code(), Code::InvalidArgument, "{what}: {err:?}");
        if what == "ephemeral" {
            assert!(err.message().contains("ephemeral"), "{}", err.message());
        }
        if what == "RWX" {
            assert_eq!(err.message(), MULTI_NODE);
        }
    }
    // No volume on this node: NOT_FOUND, naming the node (the pod waits).
    let err = n
        .node_publish_volume(node_publish_req("missing", "/t"))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::NotFound);
    assert!(
        err.message().contains("no volume missing on node node-1"),
        "{}",
        err.message()
    );

    let unpublish = |id: &str, target: &str| NodeUnpublishVolumeRequest {
        volume_id: id.into(),
        target_path: target.into(),
    };
    assert_eq!(
        code(n.node_unpublish_volume(unpublish("", "/t")).await),
        Code::InvalidArgument
    );
    assert_eq!(
        code(n.node_unpublish_volume(unpublish("v", "")).await),
        Code::InvalidArgument
    );
    // Nothing published there: done already.
    let target = d.dir.path().join("pods/x/mount");
    fs::create_dir_all(&target).unwrap();
    n.node_unpublish_volume(unpublish("v", target.to_str().unwrap()))
        .await
        .unwrap();
    assert!(!target.exists(), "the target directory is removed");
    n.node_unpublish_volume(unpublish("v", target.to_str().unwrap()))
        .await
        .unwrap();
    d.stop().await;
}

/// A Node whose `NodeGetInfo` waits until released: a call in flight, so
/// a graceful stop has something to drain.
struct HeldNode {
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

#[tonic::async_trait]
impl agentfs_csi::csi::v1::node_server::Node for HeldNode {
    async fn node_stage_volume(
        &self,
        _: tonic::Request<NodeStageVolumeRequest>,
    ) -> Result<tonic::Response<NodeStageVolumeResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented("held node"))
    }

    async fn node_unstage_volume(
        &self,
        _: tonic::Request<NodeUnstageVolumeRequest>,
    ) -> Result<tonic::Response<NodeUnstageVolumeResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented("held node"))
    }

    async fn node_publish_volume(
        &self,
        _: tonic::Request<NodePublishVolumeRequest>,
    ) -> Result<tonic::Response<NodePublishVolumeResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented("held node"))
    }

    async fn node_unpublish_volume(
        &self,
        _: tonic::Request<NodeUnpublishVolumeRequest>,
    ) -> Result<tonic::Response<NodeUnpublishVolumeResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented("held node"))
    }

    async fn node_get_capabilities(
        &self,
        _: tonic::Request<NodeGetCapabilitiesRequest>,
    ) -> Result<tonic::Response<NodeGetCapabilitiesResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented("held node"))
    }

    async fn node_get_info(
        &self,
        _: tonic::Request<NodeGetInfoRequest>,
    ) -> Result<tonic::Response<NodeGetInfoResponse>, tonic::Status> {
        self.entered.notify_one();
        self.release.notified().await;
        Ok(tonic::Response::new(NodeGetInfoResponse::default()))
    }
}

/// Once its shutdown completes, the driver refuses new connects while the
/// calls in flight finish: tonic keeps its incoming stream through the
/// drain, so the listener is stopped at the signal (bead agentfs-4lb.10,
/// fix round).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stopping_driver_refuses_connects_while_calls_finish() {
    let dir = tempfile::tempdir().unwrap();
    let layout = Layout {
        base: dir.path().join("base"),
        node_state: dir.path().join("node"),
        profiles: dir.path().join("profiles"),
    };
    let launcher = Arc::new(AgentfsLauncher {
        bin: "/bin/false".into(),
    });
    let supervisor = Supervisor::new(
        NodeConfig::new("node-1", layout.clone()),
        launcher,
        Arc::new(KernelMounter),
    );
    let controller = ControllerService::new(layout.clone(), "node-1".into(), supervisor);
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let node = HeldNode {
        entered: entered.clone(),
        release: release.clone(),
    };
    let socket = dir.path().join("csi.sock");
    let (stop, stopped) = oneshot::channel::<()>();
    let bound = server::bind(&socket).unwrap();
    let task = tokio::spawn(async move {
        bound
            .serve(IdentityService::new(), controller, node, async {
                let _ = stopped.await;
            })
            .await
            .unwrap();
    });
    let channel = connect(&socket).await;
    let call = tokio::spawn(async move {
        NodeClient::new(channel)
            .node_get_info(NodeGetInfoRequest {})
            .await
    });
    entered.notified().await;
    stop.send(()).unwrap();
    // The stop runs on the server's task just after the signal: wait for
    // it, bounded, while the held call keeps the drain going.
    let deadline = Instant::now() + Duration::from_secs(5);
    let refused = loop {
        match UnixStream::connect(&socket).await {
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => break true,
            _ if Instant::now() >= deadline => break false,
            _ => tokio::time::sleep(Duration::from_millis(10)).await,
        }
    };
    assert!(!task.is_finished(), "the drain ended before the check");
    assert!(refused, "a connect during the drain was not refused");
    release.notify_one();
    call.await.unwrap().unwrap();
    task.await.unwrap();
    assert!(!socket.exists());
}

/// `server::bind` returning is the readiness a client waits on: the socket
/// is listening, and a connect succeeds with nothing serving it yet (design
/// §2.2).
#[tokio::test]
async fn a_bound_socket_takes_a_connect_before_anything_serves_it() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("csi.sock");
    let _bound = server::bind(&socket).unwrap();
    UnixStream::connect(&socket).await.unwrap();
}
