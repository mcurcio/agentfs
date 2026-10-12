//! The CSI gRPC services end to end over a Unix socket: Identity and
//! Controller (design §14.2), with the error codes csi-sanity expects.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use hyper_util::rt::TokioIo;
use tokio::net::UnixStream;
use tokio::sync::oneshot;
use tonic::Code;
use tonic::transport::{Channel, Endpoint, Uri};
use tower::service_fn;

use agentfs_core::history::History;
use agentfs_csi::controller::ControllerService;
use agentfs_csi::csi::v1::controller_client::ControllerClient;
use agentfs_csi::csi::v1::identity_client::IdentityClient;
use agentfs_csi::csi::v1::node_server::Node;
use agentfs_csi::csi::v1::volume_capability::access_mode::Mode;
use agentfs_csi::csi::v1::volume_capability::{AccessMode, AccessType, BlockVolume, MountVolume};
use agentfs_csi::csi::v1::*;
use agentfs_csi::identity::IdentityService;
use agentfs_csi::layout::Layout;
use agentfs_csi::server;

/// No Node service in these tests.
struct NoNode;
impl Node for NoNode {}

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
            mount_dir: dir.path().join("mounts"),
            profiles: dir.path().join("profiles"),
        };
        fs::create_dir_all(&layout.profiles).unwrap();
        fs::write(
            layout.profiles.join("p.config"),
            "[agentfs]\n\tcommitWindow = 100ms\n",
        )
        .unwrap();
        fs::write(layout.profiles.join("p.exclude"), "*.tmp\n").unwrap();
        fs::create_dir_all(layout.nodes_dir()).unwrap();
        fs::write(layout.node_marker("node-1"), "").unwrap();
        fs::write(layout.node_marker("node-2"), "").unwrap();
        let socket = dir.path().join("csi.sock");
        let (stop, stopped) = oneshot::channel::<()>();
        let task = tokio::spawn({
            let socket = socket.clone();
            let controller = ControllerService::new(layout.clone());
            async move {
                server::serve(
                    &socket,
                    IdentityService::new(true),
                    Some(controller),
                    None::<NoNode>,
                    async {
                        let _ = stopped.await;
                    },
                )
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

    async fn stop(mut self) {
        let _ = self.stop.take().unwrap().send(());
        self.task.take().unwrap().await.unwrap();
        assert!(!self.dir.path().join("csi.sock").exists());
    }
}

async fn connect(socket: &Path) -> Channel {
    for _ in 0..200 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
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

fn publish_req(volume: &str, node: &str) -> ControllerPublishVolumeRequest {
    ControllerPublishVolumeRequest {
        volume_id: volume.into(),
        node_id: node.into(),
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
    assert_eq!(caps.capabilities.len(), 1);
    let probe = id.probe(ProbeRequest {}).await.unwrap().into_inner();
    assert_eq!(probe.ready, Some(true));

    let caps = d
        .controller()
        .controller_get_capabilities(ControllerGetCapabilitiesRequest {})
        .await
        .unwrap()
        .into_inner();
    assert_eq!(caps.capabilities.len(), 3);
    // Everything else is UNIMPLEMENTED.
    let r = d
        .controller()
        .list_volumes(ListVolumesRequest::default())
        .await;
    assert_eq!(code(r), Code::Unimplemented);
    d.stop().await;
}

#[tokio::test]
async fn create_volume_makes_a_state_area_from_the_profile() {
    let d = Driver::start().await;
    let mut c = d.controller();
    let req = create_req(
        "pvc-1",
        &[
            ("profile", "p"),
            ("csi.storage.k8s.io/pvc/name", "data"),
            ("csi.storage.k8s.io/pvc/namespace", "team"),
            ("csi.storage.k8s.io/pv/name", "pvc-1"),
        ],
    );
    let vol = c
        .create_volume(req.clone())
        .await
        .unwrap()
        .into_inner()
        .volume
        .unwrap();
    assert_eq!(vol.volume_id, "pvc-1");
    assert_eq!(vol.capacity_bytes, 1 << 30);
    assert_eq!(vol.volume_context["profile"], "p");
    assert_eq!(vol.volume_context["volume"], "team.data");
    assert_eq!(vol.volume_context["branch"], "agentfs/team.data");
    assert_eq!(vol.volume_context["onDelete"], "retain");

    let history = History::open(&d.layout.state("pvc-1").join("git")).unwrap();
    assert_eq!(history.settings().volume, "team.data");
    assert_eq!(
        history.settings().commit_window,
        Duration::from_millis(100),
        "the profile's config is included"
    );
    assert_eq!(
        fs::read_to_string(d.layout.state("pvc-1").join("git/info/exclude")).unwrap(),
        "*.tmp\n"
    );
    assert!(d.layout.backing("pvc-1").is_dir());
    assert!(d.layout.csi_json("pvc-1").is_file());

    // Idempotent; a different capacity or other parameters are another volume.
    let again = c.create_volume(req.clone()).await.unwrap().into_inner();
    assert_eq!(again.volume.unwrap().volume_id, "pvc-1");
    let mut bigger = req.clone();
    bigger.capacity_range = Some(CapacityRange {
        required_bytes: 2 << 30,
        limit_bytes: 2 << 30,
    });
    assert_eq!(code(c.create_volume(bigger).await), Code::AlreadyExists);
    let mut other = req.clone();
    other.parameters.insert("branch".into(), "other".into());
    assert_eq!(code(c.create_volume(other).await), Code::AlreadyExists);
    d.stop().await;
}

#[tokio::test]
async fn create_volume_refuses_what_it_does_not_support() {
    let d = Driver::start().await;
    let mut c = d.controller();
    let ok = create_req("v", &[("profile", "p")]);
    let mut cases: Vec<(&str, CreateVolumeRequest)> = Vec::new();
    let mut r = ok.clone();
    r.name = String::new();
    cases.push(("no name", r));
    let mut r = ok.clone();
    r.volume_capabilities.clear();
    cases.push(("no capabilities", r));
    let mut r = ok.clone();
    r.volume_capabilities = vec![mount_cap(Mode::MultiNodeMultiWriter)];
    cases.push(("RWX", r));
    let mut r = ok.clone();
    r.volume_capabilities[0].access_type = Some(AccessType::Block(BlockVolume {}));
    cases.push(("block", r));
    let mut r = ok.clone();
    r.volume_capabilities[0].access_type = Some(AccessType::Mount(MountVolume {
        mount_flags: vec!["noexec".into()],
        ..Default::default()
    }));
    cases.push(("mount flags", r));
    let mut r = ok.clone();
    r.volume_content_source = Some(VolumeContentSource::default());
    cases.push(("content source", r));
    cases.push((
        "unknown parameter",
        create_req("v", &[("profile", "p"), ("x", "y")]),
    ));
    cases.push(("no profile", create_req("v", &[])));
    cases.push(("missing profile", create_req("v", &[("profile", "nope")])));
    cases.push(("bad handle", create_req("a/b", &[("profile", "p")])));
    for (what, req) in cases {
        let r = c.create_volume(req).await;
        assert_eq!(code(r), Code::InvalidArgument, "{what}");
    }
    let err = c
        .create_volume({
            let mut r = ok.clone();
            r.volume_capabilities = vec![mount_cap(Mode::MultiNodeMultiWriter)];
            r
        })
        .await
        .unwrap_err();
    assert!(err.message().contains("multi-node"), "{}", err.message());
    assert!(!d.layout.volume_dir("v").exists());
    // A 128-character name (csi-sanity's maximum) works.
    let long = "a".repeat(128);
    c.create_volume(create_req(&long, &[("profile", "p")]))
        .await
        .unwrap();
    d.stop().await;
}

#[tokio::test]
async fn delete_volume_retains_or_deletes() {
    let d = Driver::start().await;
    let mut c = d.controller();
    let r = c.delete_volume(DeleteVolumeRequest::default()).await;
    assert_eq!(code(r), Code::InvalidArgument);
    for id in ["missing", "bad/id", ".."] {
        c.delete_volume(DeleteVolumeRequest {
            volume_id: id.into(),
            ..Default::default()
        })
        .await
        .unwrap();
    }

    c.create_volume(create_req("kept", &[("profile", "p")]))
        .await
        .unwrap();
    fs::write(d.layout.backing("kept").join("f"), "x").unwrap();
    let delete = |id: &str| DeleteVolumeRequest {
        volume_id: id.into(),
        ..Default::default()
    };
    c.delete_volume(delete("kept")).await.unwrap();
    assert!(!d.layout.csi_json("kept").exists());
    assert!(d.layout.retained_json("kept").exists());
    assert!(d.layout.backing("kept").join("f").exists());
    c.delete_volume(delete("kept")).await.unwrap();
    // The retained directory is re-usable under the same name.
    c.create_volume(create_req("kept", &[("profile", "p")]))
        .await
        .unwrap();
    assert!(d.layout.backing("kept").join("f").exists());
    assert!(!d.layout.retained_json("kept").exists());

    c.create_volume(create_req(
        "gone",
        &[("profile", "p"), ("onDelete", "delete")],
    ))
    .await
    .unwrap();
    // Attached: refused.
    c.controller_publish_volume(publish_req("gone", "node-1"))
        .await
        .unwrap();
    assert_eq!(
        code(c.delete_volume(delete("gone")).await),
        Code::FailedPrecondition
    );
    c.controller_unpublish_volume(ControllerUnpublishVolumeRequest {
        volume_id: "gone".into(),
        node_id: "node-1".into(),
        ..Default::default()
    })
    .await
    .unwrap();
    // Locked (a history service runs): refused, naming the holder.
    let state = agentfs_core::volume::StateDir::new(d.layout.state("gone"));
    let lock =
        agentfs_core::lock::StateLock::acquire(&state, "serve-history", Duration::ZERO).unwrap();
    let err = c.delete_volume(delete("gone")).await.unwrap_err();
    assert_eq!(err.code(), Code::FailedPrecondition);
    assert!(err.message().contains("serve-history"), "{}", err.message());
    drop(lock);
    c.delete_volume(delete("gone")).await.unwrap();
    assert!(!d.layout.volume_dir("gone").exists());
    d.stop().await;
}

#[tokio::test]
async fn controller_publish_tracks_one_node() {
    let d = Driver::start().await;
    let mut c = d.controller();
    c.create_volume(create_req("v", &[("profile", "p")]))
        .await
        .unwrap();
    let mut r = publish_req("v", "node-1");
    r.volume_id.clear();
    assert_eq!(
        code(c.controller_publish_volume(r).await),
        Code::InvalidArgument
    );
    let mut r = publish_req("v", "node-1");
    r.node_id.clear();
    assert_eq!(
        code(c.controller_publish_volume(r).await),
        Code::InvalidArgument
    );
    let mut r = publish_req("v", "node-1");
    r.volume_capability = None;
    assert_eq!(
        code(c.controller_publish_volume(r).await),
        Code::InvalidArgument
    );
    assert_eq!(
        code(
            c.controller_publish_volume(publish_req("nope", "node-1"))
                .await
        ),
        Code::NotFound
    );
    assert_eq!(
        code(
            c.controller_publish_volume(publish_req("v", "node-9"))
                .await
        ),
        Code::NotFound
    );

    c.controller_publish_volume(publish_req("v", "node-1"))
        .await
        .unwrap();
    c.controller_publish_volume(publish_req("v", "node-1"))
        .await
        .unwrap();
    assert_eq!(
        code(
            c.controller_publish_volume(publish_req("v", "node-2"))
                .await
        ),
        Code::FailedPrecondition
    );
    let unpublish = |node: &str| ControllerUnpublishVolumeRequest {
        volume_id: "v".into(),
        node_id: node.into(),
        ..Default::default()
    };
    // Another node's detach changes nothing.
    c.controller_unpublish_volume(unpublish("node-2"))
        .await
        .unwrap();
    assert_eq!(
        code(
            c.controller_publish_volume(publish_req("v", "node-2"))
                .await
        ),
        Code::FailedPrecondition
    );
    c.controller_unpublish_volume(unpublish("node-1"))
        .await
        .unwrap();
    c.controller_unpublish_volume(unpublish("node-1"))
        .await
        .unwrap();
    c.controller_publish_volume(publish_req("v", "node-2"))
        .await
        .unwrap();
    let r = c
        .controller_unpublish_volume(ControllerUnpublishVolumeRequest::default())
        .await;
    assert_eq!(code(r), Code::InvalidArgument);
    d.stop().await;
}

#[tokio::test]
async fn controller_publish_adopts_a_static_volume() {
    let d = Driver::start().await;
    let mut c = d.controller();
    let mut r = publish_req("static-1", "node-1");
    r.volume_context = HashMap::from([
        ("profile".to_owned(), "p".to_owned()),
        ("volume".to_owned(), "legacy".to_owned()),
    ]);
    c.controller_publish_volume(r).await.unwrap();
    let history = History::open(&d.layout.state("static-1").join("git")).unwrap();
    assert_eq!(history.settings().volume, "legacy");
    let rec: agentfs_csi::records::VolumeRecord =
        agentfs_csi::records::load(&d.layout.csi_json("static-1"))
            .unwrap()
            .unwrap();
    assert!(rec.static_volume);
    assert_eq!(rec.published_node.as_deref(), Some("node-1"));
    d.stop().await;
}

#[tokio::test]
async fn validate_volume_capabilities() {
    let d = Driver::start().await;
    let mut c = d.controller();
    c.create_volume(create_req("v", &[("profile", "p")]))
        .await
        .unwrap();
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
    let no = c
        .validate_volume_capabilities(req("v", vec![mount_cap(Mode::MultiNodeReaderOnly)]))
        .await
        .unwrap()
        .into_inner();
    assert!(no.confirmed.is_none());
    assert!(no.message.contains("multi-node"), "{}", no.message);
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
