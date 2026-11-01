//! The CSI gRPC services end to end over a Unix socket: Identity,
//! Controller and Node validation (design §14.2), with the error codes
//! csi-sanity expects, the release decider against prepared `file://`
//! remotes, and start classes.

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

use agentfs_core::history::History;
use agentfs_csi::controller::ControllerService;
use agentfs_csi::csi::v1::controller_client::ControllerClient;
use agentfs_csi::csi::v1::identity_client::IdentityClient;
use agentfs_csi::csi::v1::node_client::NodeClient;
use agentfs_csi::csi::v1::volume_capability::access_mode::Mode;
use agentfs_csi::csi::v1::volume_capability::{AccessMode, AccessType, BlockVolume, MountVolume};
use agentfs_csi::csi::v1::*;
use agentfs_csi::identity::{IdentityService, TOPOLOGY_KEY};
use agentfs_csi::launcher::{AgentfsLauncher, PluginIdentity};
use agentfs_csi::layout::Layout;
use agentfs_csi::mounter::KernelMounter;
use agentfs_csi::node::NodeService;
use agentfs_csi::release_scan::ReleaseScan;
use agentfs_csi::server;
use agentfs_csi::supervisor::{Backoff, NodeConfig, Supervisor};
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
            mount_dir: dir.path().join("mounts"),
            profiles: dir.path().join("profiles"),
        };
        fs::create_dir_all(&layout.profiles).unwrap();
        // No remote: local-only.
        fs::write(
            layout.profiles.join("p.config"),
            "[agentfs]\n\tcommitWindow = 100ms\n",
        )
        .unwrap();
        fs::write(layout.profiles.join("p.exclude"), "*.tmp\n").unwrap();
        let socket = dir.path().join("csi.sock");
        let (stop, stopped) = oneshot::channel::<()>();
        // The validation tests never get as far as starting a child.
        let launcher = Arc::new(AgentfsLauncher {
            bin: "/bin/false".into(),
        });
        let release = Arc::new(ReleaseScan::new(
            Arc::new(layout.clone()),
            launcher.clone(),
            PluginIdentity::default(),
            Duration::from_secs(30),
            Backoff::default(),
        ));
        let task = tokio::spawn({
            let socket = socket.clone();
            let controller = ControllerService::new(layout.clone(), "node-1".into(), release);
            let node = NodeService::new(Supervisor::new(
                NodeConfig::new("node-1", layout.clone()),
                launcher,
                Arc::new(KernelMounter),
            ));
            async move {
                server::serve(&socket, IdentityService::new(), controller, node, async {
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

    /// A bare repository under the test's directory, and a profile
    /// `name` pushing to it as `origin` (plus `extra` config lines).
    fn remote_profile(&self, name: &str, extra: &str) -> PathBuf {
        let remote = self.dir.path().join(format!("{name}.git"));
        git2::Repository::init_bare(&remote).unwrap();
        fs::write(
            self.layout.profiles.join(format!("{name}.config")),
            format!("[remote \"origin\"]\n\turl = {}\n{extra}", remote.display()),
        )
        .unwrap();
        remote
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

#[track_caller]
fn code<T: std::fmt::Debug>(r: Result<T, tonic::Status>) -> Code {
    r.expect_err("expected an error").code()
}

/// A commit holding `files` on `refname` in `repo`, on top of its tip:
/// what a history service, or another worktree, writes.
fn commit(repo: &Path, refname: &str, files: &[(&str, &str)]) -> git2::Oid {
    let repo = git2::Repository::open_bare(repo).unwrap();
    let parent = repo
        .refname_to_id(refname)
        .ok()
        .map(|id| repo.find_commit(id).unwrap());
    let mut builder = repo
        .treebuilder(parent.as_ref().map(|p| p.tree().unwrap()).as_ref())
        .unwrap();
    for (name, content) in files {
        let blob = repo.blob(content.as_bytes()).unwrap();
        builder.insert(name, blob, 0o100644).unwrap();
    }
    let tree = repo.find_tree(builder.write().unwrap()).unwrap();
    let sig = git2::Signature::now("t", "t@example.invalid").unwrap();
    let parents: Vec<&git2::Commit<'_>> = parent.iter().collect();
    repo.commit(
        Some(refname),
        &sig,
        &sig,
        "write\n\nAgentfs-Seq: 1\nAgentfs-Trigger: close\n",
        &tree,
        &parents,
    )
    .unwrap()
}

/// Pushes the volume `handle`'s branch to every remote, as the release
/// scan's `agentfs push` would.
fn push(layout: &Layout, handle: &str) {
    let state = agentfs_core::volume::StateDir::new(layout.state(handle));
    let settings = agentfs_core::config::Settings::load(&state.git()).unwrap();
    for remote in settings.remotes.pushed() {
        let mut pusher = agentfs_core::push::RemotePusher::open(
            &state,
            &settings,
            remote,
            Arc::new(agentfs_core::events::NullSink),
            &agentfs_core::telemetry::Telemetry::noop(),
        )
        .unwrap();
        assert!(pusher.push_once().is_success());
    }
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
async fn create_volume_makes_a_worktree_on_this_node() {
    let d = Driver::start().await;
    let mut c = d.controller();
    let mut req = create_req(
        "pvc-1",
        &[
            ("profile", "p"),
            ("csi.storage.k8s.io/pvc/name", "data"),
            ("csi.storage.k8s.io/pvc/namespace", "team"),
            ("csi.storage.k8s.io/pv/name", "pvc-1"),
        ],
    );
    req.accessibility_requirements = Some(TopologyRequirement {
        requisite: vec![topology("node-1")],
        preferred: vec![topology("node-1")],
    });
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
    assert_eq!(vol.volume_context["volume"], "team.data.pvc-1");
    assert_eq!(vol.volume_context["onDelete"], "retain");
    assert!(!vol.volume_context.contains_key("branch"));
    assert_eq!(vol.accessible_topology, [topology("node-1")]);

    let history = History::open(&d.layout.state("pvc-1").join("git")).unwrap();
    assert_eq!(history.settings().volume, "team.data.pvc-1");
    assert_eq!(
        history.settings().branch_ref,
        "refs/heads/agentfs/team.data.pvc-1"
    );
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
    other.parameters.insert("onDelete".into(), "delete".into());
    assert_eq!(code(c.create_volume(other).await), Code::AlreadyExists);

    // Another node's volume is never made here.
    let mut elsewhere = create_req("pvc-2", &[("profile", "p")]);
    elsewhere.accessibility_requirements = Some(TopologyRequirement {
        requisite: vec![topology("node-2")],
        preferred: Vec::new(),
    });
    let err = c.create_volume(elsewhere).await.unwrap_err();
    assert_eq!(err.code(), Code::ResourceExhausted, "{}", err.message());
    assert!(err.message().contains("node-2"), "{}", err.message());
    assert!(!d.layout.volume_dir("pvc-2").exists());
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
    // F8 rule 1: no class names a branch, and every name names the PV.
    cases.push((
        "a branch",
        create_req("v", &[("profile", "p"), ("branch", "team/x")]),
    ));
    cases.push((
        "a name without {pvName}",
        create_req("v", &[("profile", "p"), ("volumeName", "fixed")]),
    ));
    cases.push((
        "a bad startFrom",
        create_req("v", &[("profile", "p"), ("startFrom", "origin:agentfs/x")]),
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

/// F8 R2: a start class starts a new worktree from a pinned commit of
/// another worktree's branch, fetched once from the start remote, with
/// each refusal's code (design §14.2 error table).
#[tokio::test]
async fn create_volume_on_a_start_class() {
    let d = Driver::start().await;
    let remote = d.remote_profile("r", "");
    let old = "refs/heads/agentfs/v1";
    let f = commit(&remote, old, &[("f.txt", "f")]);
    let g = commit(&remote, old, &[("g.txt", "g")]);
    let mut c = d.controller();
    let start = format!("origin:agentfs/v1@{f}");
    let req = create_req(
        "pvc-2",
        &[
            ("profile", "r"),
            ("volumeName", "{pvName}"),
            ("startFrom", &start),
        ],
    );
    let vol = c
        .create_volume(req.clone())
        .await
        .unwrap()
        .into_inner()
        .volume
        .unwrap();
    assert_eq!(vol.volume_context["volume"], "pvc-2");
    assert_eq!(
        fs::read_to_string(d.layout.backing("pvc-2").join("f.txt")).unwrap(),
        "f"
    );
    assert!(
        !d.layout.backing("pvc-2").join("g.txt").exists(),
        "g is after the pin"
    );
    let settings =
        agentfs_core::config::Settings::load(&d.layout.state("pvc-2").join("git")).unwrap();
    assert_eq!(settings.start.unwrap().to_string(), start);
    let repo = git2::Repository::open_bare(d.layout.state("pvc-2").join("git")).unwrap();
    assert_eq!(repo.refname_to_id("refs/heads/agentfs/pvc-2").unwrap(), f);
    // Retried: the same volume; another start point: another volume.
    c.create_volume(req.clone()).await.unwrap();
    let mut other = req.clone();
    other
        .parameters
        .insert("startFrom".into(), format!("origin:agentfs/v1@{g}"));
    assert_eq!(code(c.create_volume(other).await), Code::AlreadyExists);
    assert_eq!(
        git2::Repository::open_bare(&remote)
            .unwrap()
            .refname_to_id(old)
            .unwrap(),
        g,
        "nothing was written to the start branch"
    );

    let unrelated = commit(&remote, "refs/heads/other", &[("x", "x")]);
    let refused = |name: &str, start: String| {
        create_req(
            name,
            &[
                ("profile", "r"),
                ("volumeName", "{pvName}"),
                ("startFrom", &start),
            ],
        )
    };
    for (name, start, wanted) in [
        (
            "pvc-3",
            format!("origin:agentfs/v1@{unrelated}"),
            Code::FailedPrecondition,
        ),
        (
            "pvc-4",
            format!("origin:agentfs/none@{f}"),
            Code::FailedPrecondition,
        ),
        (
            "pvc-5",
            format!("backup:agentfs/v1@{f}"),
            Code::InvalidArgument,
        ),
        (
            "pvc-6",
            format!("origin:agentfs/pvc-6@{f}"),
            Code::InvalidArgument,
        ),
    ] {
        let err = c.create_volume(refused(name, start)).await.unwrap_err();
        assert_eq!(err.code(), wanted, "{name}: {}", err.message());
        assert!(!d.layout.csi_json(name).exists(), "{name}");
    }
    // An unreachable start remote is UNAVAILABLE: the CO retries.
    fs::write(
        d.layout.profiles.join("gone.config"),
        format!(
            "[remote \"origin\"]\n\turl = {}\n",
            d.dir.path().join("gone.git").display()
        ),
    )
    .unwrap();
    let err = c
        .create_volume(create_req(
            "pvc-7",
            &[
                ("profile", "gone"),
                ("volumeName", "{pvName}"),
                ("startFrom", &start),
            ],
        ))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::Unavailable, "{}", err.message());
    d.stop().await;
}

#[tokio::test]
async fn delete_volume_retains_or_deletes() {
    let d = Driver::start().await;
    let mut c = d.controller();
    let r = c.delete_volume(DeleteVolumeRequest::default()).await;
    assert_eq!(code(r), Code::InvalidArgument);
    for id in ["missing", "bad/id", ".."] {
        c.delete_volume(delete_req(id)).await.unwrap();
    }

    // No history: nothing can be lost.
    c.create_volume(create_req("kept", &[("profile", "p")]))
        .await
        .unwrap();
    fs::write(d.layout.backing("kept").join("f"), "x").unwrap();
    c.delete_volume(delete_req("kept")).await.unwrap();
    assert!(!d.layout.csi_json("kept").exists());
    assert!(d.layout.retained_json("kept").exists());
    assert!(d.layout.backing("kept").join("f").exists());
    c.delete_volume(delete_req("kept")).await.unwrap();
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
    // Staged on this node: refused.
    fs::create_dir_all(d.layout.staged_dir()).unwrap();
    fs::write(d.layout.staged_record("gone"), "{}").unwrap();
    let err = c.delete_volume(delete_req("gone")).await.unwrap_err();
    assert_eq!(err.code(), Code::FailedPrecondition);
    assert!(
        err.message().contains("staged on this node"),
        "{}",
        err.message()
    );
    fs::remove_file(d.layout.staged_record("gone")).unwrap();
    // Locked (a push or a history service runs): refused, naming the holder.
    let state = agentfs_core::volume::StateDir::new(d.layout.state("gone"));
    let lock =
        agentfs_core::lock::StateLock::acquire(&state, "serve-history", Duration::ZERO).unwrap();
    let err = c.delete_volume(delete_req("gone")).await.unwrap_err();
    assert_eq!(err.code(), Code::FailedPrecondition);
    assert!(err.message().contains("serve-history"), "{}", err.message());
    drop(lock);
    c.delete_volume(delete_req("gone")).await.unwrap();
    assert!(!d.layout.volume_dir("gone").exists());
    assert!(!d.layout.deleting_dir("gone").exists());
    // Files an earlier delete left aside go at the next delete of the name.
    fs::create_dir_all(d.layout.deleting_dir("gone").join("state")).unwrap();
    c.delete_volume(delete_req("gone")).await.unwrap();
    assert!(!d.layout.deleting_dir("gone").exists());

    // No branch yet, but a record journaled and never committed (a history
    // service killed at NodeUnstage): it was promised, so nothing goes.
    c.create_volume(create_req(
        "journaled",
        &[("profile", "p"), ("onDelete", "delete")],
    ))
    .await
    .unwrap();
    let state = agentfs_core::volume::StateDir::new(d.layout.state("journaled"));
    let (journal, _) =
        agentfs_core::journal::Journal::open(state.journal(), 0, Box::new(|| Ok(()))).unwrap();
    journal
        .record(agentfs_core::record::Draft {
            trigger: agentfs_core::record::Trigger::Unlink,
            writer: agentfs_core::record::Writer::agentfs(agentfs_core::record::Caller {
                uid: 0,
                gid: 0,
                pid: 1,
            }),
            time_ms: 0,
            changes: vec![agentfs_core::record::Change::Remove {
                path: agentfs_core::paths::RepoPath::new("f").unwrap(),
            }],
            operation: None,
            atomic: false,
            origin: None,
        })
        .unwrap();
    drop(journal);
    let err = c.delete_volume(delete_req("journaled")).await.unwrap_err();
    assert_eq!(err.code(), Code::Unavailable, "{}", err.message());
    assert!(
        err.message().contains("journal holds 1 record(s)"),
        "{}",
        err.message()
    );
    assert!(d.layout.csi_json("journaled").exists());
    assert!(state.journal().exists());
    d.stop().await;
}

/// F8 rule 3: DeleteVolume asks each remote what it holds, in a listing
/// made for the call, and removes nothing a remote lacks; every `Release`
/// answer against prepared `file://` remotes.
#[tokio::test]
async fn delete_volume_asks_the_remotes() {
    let d = Driver::start().await;
    let remote = d.remote_profile("r", "");
    let mut c = d.controller();
    for (handle, on_delete) in [("v", "delete"), ("kept", "retain")] {
        c.create_volume(create_req(
            handle,
            &[
                ("profile", "r"),
                ("onDelete", on_delete),
                ("volumeName", "{pvName}"),
            ],
        ))
        .await
        .unwrap();
        // A commit, as a history service's final commit leaves it.
        commit(
            &d.layout.state(handle).join("git"),
            &format!("refs/heads/agentfs/{handle}"),
            &[("a.txt", "a")],
        );
    }
    // The remote lacks it: UNAVAILABLE, naming the remote; retain waits too.
    for handle in ["v", "kept"] {
        let err = c.delete_volume(delete_req(handle)).await.unwrap_err();
        assert_eq!(err.code(), Code::Unavailable, "{}", err.message());
        assert!(
            err.message().contains("remote origin does not hold"),
            "{}",
            err.message()
        );
        assert!(d.layout.csi_json(handle).exists());
    }
    // Pushed (the release scan's work), but behind the head: still waits.
    push(&d.layout, "v");
    commit(
        &d.layout.state("v").join("git"),
        "refs/heads/agentfs/v",
        &[("b.txt", "b")],
    );
    assert_eq!(
        code(c.delete_volume(delete_req("v")).await),
        Code::Unavailable
    );
    // Held: delete removes, retain keeps.
    push(&d.layout, "v");
    c.delete_volume(delete_req("v")).await.unwrap();
    assert!(!d.layout.volume_dir("v").exists());
    push(&d.layout, "kept");
    c.delete_volume(delete_req("kept")).await.unwrap();
    assert!(d.layout.retained_json("kept").exists());
    assert!(d.layout.state("kept").exists());

    // Another worktree wrote the branch: diverged, never removed.
    c.create_volume(create_req(
        "w",
        &[
            ("profile", "r"),
            ("onDelete", "delete"),
            ("volumeName", "{pvName}"),
        ],
    ))
    .await
    .unwrap();
    commit(
        &d.layout.state("w").join("git"),
        "refs/heads/agentfs/w",
        &[("a.txt", "a")],
    );
    commit(&remote, "refs/heads/agentfs/w", &[("theirs.txt", "x")]);
    let err = c.delete_volume(delete_req("w")).await.unwrap_err();
    assert_eq!(err.code(), Code::Unavailable);
    assert!(err.message().contains("diverged"), "{}", err.message());
    assert!(d.layout.volume_dir("w").exists());

    // An unreachable remote: UNAVAILABLE, naming it.
    fs::write(
        d.layout.profiles.join("u.config"),
        format!(
            "[remote \"far\"]\n\turl = {}\n",
            d.dir.path().join("nope.git").display()
        ),
    )
    .unwrap();
    c.create_volume(create_req(
        "u",
        &[
            ("profile", "u"),
            ("onDelete", "delete"),
            ("volumeName", "{pvName}"),
        ],
    ))
    .await
    .unwrap();
    commit(
        &d.layout.state("u").join("git"),
        "refs/heads/agentfs/u",
        &[("a.txt", "a")],
    );
    let err = c.delete_volume(delete_req("u")).await.unwrap_err();
    assert_eq!(err.code(), Code::Unavailable);
    assert!(
        err.message().contains("remote far could not be listed"),
        "{}",
        err.message()
    );

    // Local-only (no remote section): kept whatever onDelete says.
    c.create_volume(create_req(
        "solo",
        &[
            ("profile", "p"),
            ("onDelete", "delete"),
            ("volumeName", "{pvName}"),
        ],
    ))
    .await
    .unwrap();
    commit(
        &d.layout.state("solo").join("git"),
        "refs/heads/agentfs/solo",
        &[("a.txt", "a")],
    );
    c.delete_volume(delete_req("solo")).await.unwrap();
    assert!(d.layout.retained_json("solo").exists());
    assert!(
        d.layout.state("solo").exists(),
        "nothing elsewhere holds it"
    );
    d.stop().await;
}

/// F8 S3: a source that received a fork while local-only, then gained a
/// remote, holds that fork's only copy: DeleteVolume waits, naming the
/// ref, until the remote holds it as well as the head (the push carries
/// it), and only then deletes.
#[tokio::test]
async fn delete_volume_waits_for_received_forks() {
    let d = Driver::start().await;
    d.remote_profile("r", "");
    let mut c = d.controller();
    c.create_volume(create_req(
        "s",
        &[
            ("profile", "r"),
            ("onDelete", "delete"),
            ("volumeName", "{pvName}"),
        ],
    ))
    .await
    .unwrap();
    let git = d.layout.state("s").join("git");
    commit(&git, "refs/heads/agentfs/s", &[("a.txt", "a")]);
    push(&d.layout, "s");
    // What receive_fork_change writes for fork s-1.
    let finish = commit(&git, "refs/heads/agentfs/s-1", &[("b.txt", "b")]);
    let repo = git2::Repository::open_bare(&git).unwrap();
    for name in ["refs/agentfs/s-1/change", "refs/agentfs/s-1/finish"] {
        repo.reference(name, finish, false, "test").unwrap();
    }
    let err = c.delete_volume(delete_req("s")).await.unwrap_err();
    assert_eq!(err.code(), Code::Unavailable, "{}", err.message());
    assert!(
        err.message()
            .contains("does not hold refs/agentfs/s-1/change, a fork received"),
        "{}",
        err.message()
    );
    assert!(d.layout.volume_dir("s").exists());
    push(&d.layout, "s");
    c.delete_volume(delete_req("s")).await.unwrap();
    assert!(!d.layout.volume_dir("s").exists());
    d.stop().await;
}

/// DeleteVolume cannot ask the remotes of a worktree whose settings fail
/// to load (its profile removed from the ConfigMap, or holding a bad
/// value): FAILED_PRECONDITION, and nothing deleted or released.
#[tokio::test]
async fn a_volume_with_unreadable_settings_is_not_released() {
    let d = Driver::start().await;
    let mut c = d.controller();
    for profile in ["removed", "broken"] {
        fs::write(
            d.layout.profiles.join(format!("{profile}.config")),
            "[agentfs]\n\tcommitWindow = 100ms\n",
        )
        .unwrap();
        c.create_volume(create_req(profile, &[("profile", profile)]))
            .await
            .unwrap();
    }
    fs::remove_file(d.layout.profiles.join("removed.config")).unwrap();
    fs::write(
        d.layout.profiles.join("broken.config"),
        "[agentfs]\n\tcommitWindow = soon\n",
    )
    .unwrap();
    for profile in ["removed", "broken"] {
        let state = agentfs_core::volume::StateDir::new(d.layout.state(profile));
        assert!(
            agentfs_core::config::Settings::load(&state.git()).is_err(),
            "{profile}: its settings must fail to load for this test to mean anything"
        );
        let err = c.delete_volume(delete_req(profile)).await.unwrap_err();
        assert_eq!(err.code(), Code::FailedPrecondition, "{}", err.message());
        assert!(
            err.message().contains("nothing deleted"),
            "{}",
            err.message()
        );
        assert!(d.layout.csi_json(profile).exists());
    }
    d.stop().await;
}

#[tokio::test]
async fn multi_node_modes_are_refused_with_the_decision() {
    let d = Driver::start().await;
    let mut c = d.controller();
    let mut n = NodeClient::new(d.channel.clone());
    c.create_volume(create_req("v", &[("profile", "p")]))
        .await
        .unwrap();
    for mode in [
        Mode::MultiNodeReaderOnly,
        Mode::MultiNodeSingleWriter,
        Mode::MultiNodeMultiWriter,
    ] {
        let refused = |r: tonic::Status, call: &str| {
            assert_eq!(r.code(), Code::InvalidArgument, "{call} {mode:?}");
            assert_eq!(r.message(), MULTI_NODE, "{call} {mode:?}");
        };
        let mut req = create_req("w", &[("profile", "p")]);
        req.volume_capabilities = vec![mount_cap(mode)];
        refused(c.create_volume(req).await.unwrap_err(), "CreateVolume");
        assert!(!d.layout.volume_dir("w").exists());

        let no = c
            .validate_volume_capabilities(ValidateVolumeCapabilitiesRequest {
                volume_id: "v".into(),
                volume_capabilities: vec![mount_cap(mode)],
                ..Default::default()
            })
            .await
            .unwrap()
            .into_inner();
        assert!(no.confirmed.is_none(), "{mode:?}");
        assert_eq!(no.message, MULTI_NODE, "{mode:?}");

        let mut req = stage_req("v", "/staging");
        req.volume_capability = Some(mount_cap(mode));
        refused(n.node_stage_volume(req).await.unwrap_err(), "NodeStage");

        let mut req = node_publish_req("v", "/target");
        req.volume_capability = Some(mount_cap(mode));
        refused(n.node_publish_volume(req).await.unwrap_err(), "NodePublish");
    }
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

fn stage_req(id: &str, staging: &str) -> NodeStageVolumeRequest {
    NodeStageVolumeRequest {
        volume_id: id.into(),
        staging_target_path: staging.into(),
        volume_capability: Some(mount_cap(Mode::SingleNodeWriter)),
        ..Default::default()
    }
}

fn node_publish_req(id: &str, target: &str) -> NodePublishVolumeRequest {
    NodePublishVolumeRequest {
        volume_id: id.into(),
        staging_target_path: "/staging".into(),
        target_path: target.into(),
        volume_capability: Some(mount_cap(Mode::SingleNodeWriter)),
        ..Default::default()
    }
}

#[tokio::test]
async fn node_calls_are_validated() {
    let d = Driver::start().await;
    let mut n = NodeClient::new(d.channel.clone());
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
    assert_eq!(caps.capabilities.len(), 2);

    let ephemeral = HashMap::from([("csi.storage.k8s.io/ephemeral".to_owned(), "true".to_owned())]);
    let mut stage_cases = vec![
        ("no id", stage_req("", "/staging")),
        ("no staging path", stage_req("v", "")),
        ("relative staging path", stage_req("v", "staging")),
    ];
    let mut r = stage_req("v", "/staging");
    r.volume_capability = None;
    stage_cases.push(("no capability", r));
    let mut r = stage_req("v", "/staging");
    r.volume_capability.as_mut().unwrap().access_type = Some(AccessType::Block(BlockVolume {}));
    stage_cases.push(("block", r));
    let mut r = stage_req("v", "/staging");
    r.volume_context = ephemeral.clone();
    stage_cases.push(("ephemeral", r));
    for (what, req) in stage_cases {
        assert_eq!(
            code(n.node_stage_volume(req).await),
            Code::InvalidArgument,
            "{what}"
        );
    }
    // No worktree on this node: NOT_FOUND, naming the node (the pod waits).
    let err = n
        .node_stage_volume(stage_req("missing", "/staging"))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::NotFound);
    assert!(
        err.message().contains("no worktree on node node-1"),
        "{}",
        err.message()
    );

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
    r.volume_context = ephemeral;
    publish_cases.push(("ephemeral", r));
    for (what, req) in publish_cases {
        let err = n.node_publish_volume(req).await.unwrap_err();
        assert_eq!(err.code(), Code::InvalidArgument, "{what}: {err:?}");
        if what == "ephemeral" {
            assert!(err.message().contains("ephemeral"), "{}", err.message());
        }
    }
    // Published before staged.
    assert_eq!(
        code(n.node_publish_volume(node_publish_req("v", "/t")).await),
        Code::FailedPrecondition
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

    let unstage = |id: &str, path: &str| NodeUnstageVolumeRequest {
        volume_id: id.into(),
        staging_target_path: path.into(),
    };
    assert_eq!(
        code(n.node_unstage_volume(unstage("", "/s")).await),
        Code::InvalidArgument
    );
    assert_eq!(
        code(n.node_unstage_volume(unstage("v", "")).await),
        Code::InvalidArgument
    );
    n.node_unstage_volume(unstage("v", "/s")).await.unwrap();
    d.stop().await;
}

/// A worktree kept by `onDelete: retain` is mounted again through a static
/// PV written as docs/csi.md shows it: `volumeName` names the volume the
/// state area holds, a misspelt attribute is refused, and NodeStage
/// adopts it (writes its `csi.json`) only over that worktree.
#[tokio::test]
async fn a_retained_volume_is_adopted_through_the_documented_attributes() {
    let d = Driver::start().await;
    let mut c = d.controller();
    let created = c
        .create_volume(create_req(
            "pvc-0b6f",
            &[
                ("profile", "p"),
                ("csi.storage.k8s.io/pvc/name", "claim"),
                ("csi.storage.k8s.io/pvc/namespace", "ns"),
                ("csi.storage.k8s.io/pv/name", "pvc-0b6f"),
            ],
        ))
        .await
        .unwrap()
        .into_inner()
        .volume
        .unwrap();
    let name = created.volume_context["volume"].clone();
    assert_eq!(
        name, "ns.claim.pvc-0b6f",
        "the default name is unique per PV"
    );
    c.delete_volume(delete_req("pvc-0b6f")).await.unwrap();
    assert!(d.layout.retained_json("pvc-0b6f").exists());

    let mut n = NodeClient::new(d.channel.clone());
    let stage = |pairs: &[(&str, &str)]| {
        let mut r = stage_req("pvc-0b6f", "/staging");
        r.volume_context = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        r
    };
    // A typo fails loudly instead of naming the volume after its handle.
    assert_eq!(
        code(
            n.node_stage_volume(stage(&[("profile", "p"), ("volumename", &name)]))
                .await
        ),
        Code::InvalidArgument
    );
    // Another name than the state area's is refused.
    assert_eq!(
        code(
            n.node_stage_volume(stage(&[("profile", "p"), ("volumeName", "other")]))
                .await
        ),
        Code::AlreadyExists
    );
    assert!(!d.layout.csi_json("pvc-0b6f").exists());
    // The documented attributes adopt it (the service this test's
    // launcher starts, /bin/false, then exits: that is the stage's own
    // failure, after the adoption).
    let _ = n
        .node_stage_volume(stage(&[("profile", "p"), ("volumeName", &name)]))
        .await;
    let rec: agentfs_csi::records::VolumeRecord =
        agentfs_csi::records::load(&d.layout.csi_json("pvc-0b6f"))
            .unwrap()
            .unwrap();
    assert!(rec.static_volume);
    assert_eq!(rec.params.volume, name);
    let history = History::open(&d.layout.state("pvc-0b6f").join("git")).unwrap();
    assert_eq!(history.settings().volume, name);
    d.stop().await;
}

/// A content source naming the volume `id`.
fn clone_of(id: &str) -> Option<VolumeContentSource> {
    Some(VolumeContentSource {
        r#type: Some(volume_content_source::Type::Volume(
            volume_content_source::VolumeSource {
                volume_id: id.into(),
            },
        )),
    })
}

/// A12, A13: CreateVolume of a fork class clones a source volume on this
/// node, and DeleteVolume waits until the fork is published.
#[tokio::test]
async fn forks_are_cloned_and_deleted_once_published() {
    let d = Driver::start().await;
    d.remote_profile("r", "");
    let mut c = d.controller();
    c.create_volume(create_req(
        "src",
        &[("profile", "r"), ("volumeName", "{pvName}")],
    ))
    .await
    .unwrap();
    // The source's history: one file, as a history service commits it.
    let base = commit(
        &d.layout.state("src").join("git"),
        "refs/heads/agentfs/src",
        &[("a.txt", "from the source\n")],
    );

    let fork_params = [
        ("fork", "true"),
        ("csi.storage.k8s.io/pvc/name", "sandbox"),
        ("csi.storage.k8s.io/pvc/namespace", "agents"),
        ("csi.storage.k8s.io/pv/name", "fork-1"),
    ];
    // What a fork class refuses.
    let mut cases = vec![(
        "a fork class without a dataSource",
        create_req("fork-1", &fork_params),
    )];
    let mut r = create_req("fork-1", &[("profile", "r")]);
    r.volume_content_source = clone_of("src");
    cases.push(("a dataSource on an ordinary class", r));
    let mut r = create_req("fork-1", &fork_params);
    r.volume_content_source = Some(VolumeContentSource {
        r#type: Some(volume_content_source::Type::Snapshot(
            volume_content_source::SnapshotSource {
                snapshot_id: "s".into(),
            },
        )),
    });
    cases.push(("a snapshot", r));
    for (key, value) in [
        ("profile", "r"),
        ("branch", "b"),
        ("startFrom", "origin:agentfs/src@x"),
        ("volumeName", "{pvcName}"),
    ] {
        let mut params = fork_params.to_vec();
        params.push((key, value));
        let mut r = create_req("fork-1", &params);
        r.volume_content_source = clone_of("src");
        cases.push((key, r));
    }
    for (what, req) in cases {
        let err = c.create_volume(req).await.unwrap_err();
        assert_eq!(
            err.code(),
            Code::InvalidArgument,
            "{what}: {}",
            err.message()
        );
    }
    // A source on another node: refused, naming the remedy.
    let mut r = create_req("fork-1", &fork_params);
    r.volume_content_source = clone_of("elsewhere");
    let err = c.create_volume(r).await.unwrap_err();
    assert_eq!(err.code(), Code::FailedPrecondition, "{}", err.message());
    assert!(err.message().contains("affinity"), "{}", err.message());

    let mut req = create_req("fork-1", &fork_params);
    req.volume_content_source = clone_of("src");
    let vol = c
        .create_volume(req.clone())
        .await
        .unwrap()
        .into_inner()
        .volume
        .unwrap();
    assert_eq!(vol.content_source, clone_of("src"));
    assert_eq!(vol.accessible_topology, [topology("node-1")]);
    assert_eq!(vol.volume_context["profile"], "r");
    assert_eq!(vol.volume_context["volume"], "agents.sandbox.fork-1");
    assert_eq!(vol.volume_context["onDelete"], "delete");
    let state = agentfs_core::volume::StateDir::new(d.layout.state("fork-1"));
    let settings = agentfs_core::config::Settings::load(&state.git()).unwrap();
    let identity = settings.fork.clone().unwrap();
    assert_eq!(identity.source, "src");
    assert_eq!(identity.base, base);
    assert_eq!(
        fs::read_to_string(d.layout.backing("fork-1").join("a.txt")).unwrap(),
        "from the source\n"
    );
    // Idempotent.
    c.create_volume(req).await.unwrap();

    // Unpublished: UNAVAILABLE, the request stored, nothing deleted.
    let err = c.delete_volume(delete_req("fork-1")).await.unwrap_err();
    assert_eq!(err.code(), Code::Unavailable, "{}", err.message());
    assert!(err.message().contains("is active"), "{}", err.message());
    let record = agentfs_csi::volume::load(&d.layout, "fork-1")
        .unwrap()
        .unwrap();
    assert!(record.finish_requested);
    assert!(d.layout.volume_dir("fork-1").exists());

    // Finished, with a mark that cannot be read: FAILED_PRECONDITION.
    let lock = agentfs_core::lock::StateLock::acquire(&state, "test", Duration::ZERO).unwrap();
    let mut finisher = agentfs_core::fork::finish::Finisher::open(
        &state,
        &d.layout.backing("fork-1"),
        fork_context(),
        Arc::new(agentfs_core::events::NullSink),
        &agentfs_core::telemetry::Telemetry::noop(),
        &lock,
    )
    .unwrap();
    finisher.finish().unwrap();
    fs::write(state.fork_mark(), "{").unwrap();
    drop(finisher);
    drop(lock);
    let err = c.delete_volume(delete_req("fork-1")).await.unwrap_err();
    assert_eq!(err.code(), Code::FailedPrecondition, "{}", err.message());
    assert!(
        err.message().contains("nothing deleted"),
        "{}",
        err.message()
    );
    fs::remove_file(state.fork_mark()).unwrap();
    let err = c.delete_volume(delete_req("fork-1")).await.unwrap_err();
    assert_eq!(err.code(), Code::Unavailable, "{}", err.message());
    assert!(err.message().contains("is finished"), "{}", err.message());

    // Published: onDelete applies (delete, the fork class's default).
    let lock = agentfs_core::lock::StateLock::acquire(&state, "test", Duration::ZERO).unwrap();
    let mut finisher = agentfs_core::fork::finish::Finisher::open(
        &state,
        &d.layout.backing("fork-1"),
        fork_context(),
        Arc::new(agentfs_core::events::NullSink),
        &agentfs_core::telemetry::Telemetry::noop(),
        &lock,
    )
    .unwrap();
    let outcome = finisher
        .publish(Some(Instant::now() + Duration::from_secs(10)))
        .unwrap();
    assert_eq!(
        outcome,
        agentfs_core::fork::finish::PublishOutcome::Published
    );
    drop(finisher);
    drop(lock);
    c.delete_volume(delete_req("fork-1")).await.unwrap();
    assert!(!d.layout.volume_dir("fork-1").exists());

    // B4: an abandoned fork, never published, is deleted as onDelete says.
    let mut params = fork_params.to_vec();
    params.retain(|(k, _)| *k != "csi.storage.k8s.io/pv/name");
    params.push(("csi.storage.k8s.io/pv/name", "fork-2"));
    let mut req = create_req("fork-2", &params);
    req.volume_content_source = clone_of("src");
    c.create_volume(req).await.unwrap();
    let err = c.delete_volume(delete_req("fork-2")).await.unwrap_err();
    assert_eq!(err.code(), Code::Unavailable, "{}", err.message());
    let state = agentfs_core::volume::StateDir::new(d.layout.state("fork-2"));
    let lock = agentfs_core::lock::StateLock::acquire(&state, "test", Duration::ZERO).unwrap();
    let operator = agentfs_core::record::Caller {
        uid: 0,
        gid: 0,
        pid: 1,
    };
    let status =
        agentfs_core::fork::finish::abandon(&state, &lock, "not wanted", operator).unwrap();
    assert_eq!(
        status.state,
        agentfs_core::fork::finish::ForkState::Abandoned
    );
    drop(lock);
    c.delete_volume(delete_req("fork-2")).await.unwrap();
    assert!(!d.layout.volume_dir("fork-2").exists());
    // The source is untouched.
    assert!(d.layout.volume_dir("src").exists());
    d.stop().await;
}

fn fork_context() -> agentfs_core::identity::Context {
    agentfs_core::identity::Context {
        volume: "agents.sandbox.fork-1".into(),
        replica: "test-0".into(),
        pod_uid: "u".into(),
        ..Default::default()
    }
}

/// Owner, 2026-10-17: "Remotes can come and go. It's not a error to have
/// zero remotes". CreateVolume takes a profile with no remote section, with
/// or without `onDelete: delete`: the worktree is local-only (the release
/// decider keeps it).
#[tokio::test]
async fn create_volume_takes_a_profile_without_remotes() {
    let d = Driver::start().await;
    let mut c = d.controller();
    fs::write(
        d.layout.profiles.join("bare.config"),
        "[agentfs]\n\tcommitWindow = 100ms\n",
    )
    .unwrap();
    for (handle, on_delete) in [("bare", "retain"), ("bare-delete", "delete")] {
        c.create_volume(create_req(
            handle,
            &[("profile", "bare"), ("onDelete", on_delete)],
        ))
        .await
        .unwrap();
        let state = agentfs_core::volume::StateDir::new(d.layout.state(handle));
        let settings = agentfs_core::config::Settings::load(&state.git()).unwrap();
        assert_eq!(
            settings.remotes,
            agentfs_core::config::Remotes::LocalOnly,
            "{handle}"
        );
    }
    d.stop().await;
}

/// Owner, 2026-10-17: "A fork can be created with or without a remote link
/// back to its parent. Configurable." A fork class's `link` reaches the
/// fork's identity at CreateVolume. An unlinked fork has no fork status:
/// DeleteVolume never asks it to finish. On where it pushes (owner,
/// 2026-10-17): "B. And if no remote is configured, the volume is lost,
/// and that's a choice the user is allowed to make." With its class's
/// `remotes` profile it pushes there only, never to its source's remote,
/// and DeleteVolume asks that remote; without one it is local-only and
/// DeleteVolume removes it unpushed.
#[tokio::test]
async fn a_fork_class_link_reaches_the_fork() {
    let d = Driver::start().await;
    let source_remote = d.remote_profile("r", "");
    let own_remote = d.dir.path().join("mine.git");
    git2::Repository::init_bare(&own_remote).unwrap();
    fs::write(
        d.layout.profiles.join("mine.config"),
        format!("[remote \"mine\"]\n\turl = {}\n", own_remote.display()),
    )
    .unwrap();
    let mut c = d.controller();
    c.create_volume(create_req(
        "src",
        &[("profile", "r"), ("volumeName", "{pvName}")],
    ))
    .await
    .unwrap();
    commit(
        &d.layout.state("src").join("git"),
        "refs/heads/agentfs/src",
        &[("a.txt", "from the source\n")],
    );
    let link_of = |handle: &str| {
        let state = agentfs_core::volume::StateDir::new(d.layout.state(handle));
        let settings = agentfs_core::config::Settings::load(&state.git()).unwrap();
        settings.fork.unwrap().link
    };
    for (handle, link, remotes) in [
        ("fork-default", None, None),
        ("fork-linked", Some("linked"), None),
        ("fork-unlinked", Some("unlinked"), None),
        ("fork-own", Some("unlinked"), Some("mine")),
    ] {
        let mut params = vec![
            ("fork", "true"),
            ("csi.storage.k8s.io/pvc/name", handle),
            ("csi.storage.k8s.io/pvc/namespace", "agents"),
            ("csi.storage.k8s.io/pv/name", handle),
        ];
        params.extend(link.map(|l| ("link", l)));
        params.extend(remotes.map(|r| ("remotes", r)));
        let mut req = create_req(handle, &params);
        req.volume_content_source = clone_of("src");
        c.create_volume(req).await.unwrap();
    }
    use agentfs_core::fork::ForkLink;
    assert_eq!(link_of("fork-default"), ForkLink::Linked);
    assert_eq!(link_of("fork-linked"), ForkLink::Linked);
    assert_eq!(link_of("fork-unlinked"), ForkLink::Unlinked);

    // The unlinked fork with its own remote: never asked to finish; that
    // remote, and only that, is asked and pushed to.
    let state = agentfs_core::volume::StateDir::new(d.layout.state("fork-own"));
    let branch = agentfs_core::config::Settings::load(&state.git())
        .unwrap()
        .branch_ref;
    commit(&state.git(), &branch, &[("b.txt", "the fork's\n")]);
    let err = c.delete_volume(delete_req("fork-own")).await.unwrap_err();
    assert_eq!(err.code(), Code::Unavailable, "{}", err.message());
    assert!(
        err.message().contains("remote mine does not hold"),
        "{}",
        err.message()
    );
    let record = agentfs_csi::volume::load(&d.layout, "fork-own")
        .unwrap()
        .unwrap();
    assert!(!record.finish_requested, "an unlinked fork never finishes");
    push(&d.layout, "fork-own");
    let on = |remote: &Path| {
        git2::Repository::open_bare(remote)
            .unwrap()
            .refname_to_id(&branch)
            .ok()
    };
    assert!(on(&own_remote).is_some());
    assert_eq!(on(&source_remote), None, "never the source's remote");
    c.delete_volume(delete_req("fork-own")).await.unwrap();
    assert!(!d.layout.volume_dir("fork-own").exists());

    // No remote of its own: local-only, so DeleteVolume removes it with
    // its unpushed commit (the class's onDelete defaults to delete).
    let state = agentfs_core::volume::StateDir::new(d.layout.state("fork-unlinked"));
    let settings = agentfs_core::config::Settings::load(&state.git()).unwrap();
    assert_eq!(settings.remotes, agentfs_core::config::Remotes::LocalOnly);
    commit(&state.git(), &settings.branch_ref, &[("b.txt", "lost\n")]);
    c.delete_volume(delete_req("fork-unlinked")).await.unwrap();
    assert!(!d.layout.volume_dir("fork-unlinked").exists());
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
        mount_dir: dir.path().join("mounts"),
        profiles: dir.path().join("profiles"),
    };
    let launcher = Arc::new(AgentfsLauncher {
        bin: "/bin/false".into(),
    });
    let release = Arc::new(ReleaseScan::new(
        Arc::new(layout.clone()),
        launcher,
        PluginIdentity::default(),
        Duration::from_secs(30),
        Backoff::default(),
    ));
    let controller = ControllerService::new(layout.clone(), "node-1".into(), release);
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let node = HeldNode {
        entered: entered.clone(),
        release: release.clone(),
    };
    let socket = dir.path().join("csi.sock");
    let (stop, stopped) = oneshot::channel::<()>();
    let task = tokio::spawn({
        let socket = socket.clone();
        async move {
            server::serve(&socket, IdentityService::new(), controller, node, async {
                let _ = stopped.await;
            })
            .await
            .unwrap();
        }
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
