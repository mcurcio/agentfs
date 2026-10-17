//! The CSI Controller service (design §14.2), served by every node plugin
//! pod, with csi-provisioner beside it (`--node-deployment`, F8 R8): each
//! node provisions and deletes the worktrees on its own disk.
//!
//! - CreateVolume makes `<base>/<pv-name>/{backing,state,csi.json}` on
//!   this node, which the request's accessibility requirements must
//!   include (`RESOURCE_EXHAUSTED` otherwise), and answers with this
//!   node's topology. A start class (`startFrom`) starts the worktree from
//!   another worktree's branch, fetched once from the start remote
//!   ([`agentfs_core::fork::start::start_from`]). A fork class (`fork:
//!   "true"`) with a `dataSource` claim makes a fork (design §11.5) of a
//!   source on this node, from its local history, without its lock or any
//!   credentials.
//! - DeleteVolume asks the one release decider
//!   ([`agentfs_core::release::decide`], F8 rule 3) over facts gathered
//!   here: the worktree's declared remotes, what each pushed remote
//!   advertises for the branch in a listing made for this call
//!   ([`agentfs_core::push::RemotePusher::branch_claim`]), the journal's
//!   backlog ([`agentfs_core::volume::inspect`], read under the state
//!   lock), the class's `onDelete`, and a fork's status. `Delete` removes
//!   the directory; `Keep` releases the volume and keeps the worktree,
//!   renaming `csi.json` to `csi.retained.json`; `Wait` is `UNAVAILABLE`,
//!   naming the remote (or the journal) and why, and wakes the node
//!   plugin's release scan, which commits and pushes the worktree
//!   (DeleteVolume never commits or pushes). A fork's deletion is
//!   its request to be finished (`finish_requested` in `csi.json`), which
//!   the release scan carries out. It refuses a worktree staged on this
//!   node, or whose state area is locked. `Delete` renames the directory
//!   to `.deleting.<handle>` under the state lock and removes it after
//!   releasing the lock.
//! - ControllerPublish and ControllerUnpublish are `UNIMPLEMENTED`: there
//!   is no attach step (`attachRequired: false`); the PV's node affinity
//!   keeps a worktree's pods on its node.
//!
//! Calls are serialised. DeleteVolume's listing is the only network call.

use std::fs;
use std::io;
use std::sync::Arc;

use tokio::sync::Mutex;
use tonic::{Request, Response, Status};

use agentfs_core::config::Settings;
use agentfs_core::events::NullSink;
use agentfs_core::fork::finish::ForkState;
use agentfs_core::journal;
use agentfs_core::push::RemotePusher;
use agentfs_core::release::{self, Declared, Release, Seen, Waiting, Worktree};
use agentfs_core::telemetry::Telemetry;
use agentfs_core::volume::{self as core_volume, StateDir};

use crate::csi::v1::controller_server::Controller;
use crate::csi::v1::controller_service_capability::{self, rpc};
use crate::csi::v1::validate_volume_capabilities_response::Confirmed;
use crate::csi::v1::volume_content_source;
use crate::csi::v1::{
    ControllerGetCapabilitiesRequest, ControllerGetCapabilitiesResponse,
    ControllerPublishVolumeRequest, ControllerPublishVolumeResponse, ControllerServiceCapability,
    ControllerUnpublishVolumeRequest, ControllerUnpublishVolumeResponse, CreateVolumeRequest,
    CreateVolumeResponse, DeleteVolumeRequest, DeleteVolumeResponse, Topology,
    ValidateVolumeCapabilitiesRequest, ValidateVolumeCapabilitiesResponse, Volume,
};
use crate::error::Error;
use crate::identity::TOPOLOGY_KEY;
use crate::layout::{Layout, validate_handle};
use crate::params::{ForkClass, OnDelete, VolumeParams};
use crate::records::VolumeRecord;
use crate::release_scan::ReleaseScan;
use crate::volume;

#[derive(Clone)]
pub struct ControllerService {
    layout: Arc<Layout>,
    node_id: Arc<str>,
    release: Arc<ReleaseScan>,
    serial: Arc<Mutex<()>>,
}

impl ControllerService {
    pub fn new(layout: Layout, node_id: String, release: Arc<ReleaseScan>) -> Self {
        Self {
            layout: Arc::new(layout),
            node_id: node_id.into(),
            release,
            serial: Arc::new(Mutex::new(())),
        }
    }

    /// Runs `f` on the blocking pool, one call at a time.
    async fn run<T: Send + 'static>(
        &self,
        f: impl FnOnce(&Layout, &str, &ReleaseScan) -> Result<T, Error> + Send + 'static,
    ) -> Result<T, Status> {
        let _serial = self.serial.lock().await;
        let layout = self.layout.clone();
        let node_id = self.node_id.clone();
        let release = self.release.clone();
        tokio::task::spawn_blocking(move || f(&layout, &node_id, &release))
            .await
            .map_err(|e| Status::internal(format!("controller task failed: {e}")))?
            .map_err(Status::from)
    }
}

fn capability(t: rpc::Type) -> ControllerServiceCapability {
    ControllerServiceCapability {
        r#type: Some(controller_service_capability::Type::Rpc(
            controller_service_capability::Rpc { r#type: t as i32 },
        )),
    }
}

fn required(value: &str, what: &str) -> Result<(), Error> {
    if value.is_empty() {
        Err(Error::InvalidArgument(format!("{what} is required")))
    } else {
        Ok(())
    }
}

/// The capacity to record: required, else the limit, else 0.
fn capacity(req: &CreateVolumeRequest) -> Result<i64, Error> {
    let Some(range) = req.capacity_range else {
        return Ok(0);
    };
    if range.required_bytes < 0 || range.limit_bytes < 0 {
        return Err(Error::InvalidArgument("negative capacity".into()));
    }
    if range.limit_bytes > 0 && range.required_bytes > range.limit_bytes {
        return Err(Error::InvalidArgument(format!(
            "required capacity {} exceeds the limit {}",
            range.required_bytes, range.limit_bytes
        )));
    }
    Ok(if range.required_bytes > 0 {
        range.required_bytes
    } else {
        range.limit_bytes
    })
}

/// This node's topology segment: `{TOPOLOGY_KEY: node_id}`.
pub fn topology(node_id: &str) -> Topology {
    Topology {
        segments: [(TOPOLOGY_KEY.to_owned(), node_id.to_owned())].into(),
    }
}

/// `RESOURCE_EXHAUSTED` when the request's requisite topologies name
/// other nodes only: a worktree is made on this node's disk or not at
/// all. A segment without this driver's key does not constrain.
fn check_accessible(req: &CreateVolumeRequest, node_id: &str) -> Result<(), Error> {
    let Some(wanted) = &req.accessibility_requirements else {
        return Ok(());
    };
    let here = |t: &Topology| {
        t.segments
            .get(TOPOLOGY_KEY)
            .is_none_or(|node| node == node_id)
    };
    if wanted.requisite.is_empty() || wanted.requisite.iter().any(here) {
        return Ok(());
    }
    let nodes: Vec<&str> = wanted
        .requisite
        .iter()
        .filter_map(|t| t.segments.get(TOPOLOGY_KEY).map(String::as_str))
        .collect();
    Err(Error::ResourceExhausted(format!(
        "the volume must be on node {}, and this is node {node_id}: each node provisions its own \
         worktrees",
        nodes.join(" or ")
    )))
}

pub(crate) fn create_volume(
    layout: &Layout,
    node_id: &str,
    req: &CreateVolumeRequest,
) -> Result<CreateVolumeResponse, Error> {
    required(&req.name, "the volume name")?;
    let handle = req.name.as_str();
    validate_handle(handle).map_err(|e| Error::InvalidArgument(e.to_string()))?;
    volume::check_capabilities(&req.volume_capabilities)?;
    let invalid = |e: crate::params::ParamError| Error::InvalidArgument(e.to_string());
    let class = ForkClass::from_create(handle, &req.parameters).map_err(invalid)?;
    let source = match req.volume_content_source.as_ref().map(|s| &s.r#type) {
        None => None,
        Some(Some(volume_content_source::Type::Volume(v))) => Some(v.volume_id.clone()),
        Some(Some(volume_content_source::Type::Snapshot(_))) => {
            return Err(Error::InvalidArgument("snapshots are not supported".into()));
        }
        Some(None) => {
            return Err(Error::InvalidArgument(
                "the volume content source names no volume".into(),
            ));
        }
    };
    let capacity_bytes = capacity(req)?;
    let (params, fork) = match (class, source) {
        (Some(class), Some(source)) => (None, Some((class, source))),
        (Some(_), None) => {
            return Err(Error::InvalidArgument(
                "a fork class needs a dataSource claim".into(),
            ));
        }
        (None, Some(_)) => {
            return Err(Error::InvalidArgument(
                "cloning needs a fork class (fork: \"true\")".into(),
            ));
        }
        (None, None) => (
            Some(VolumeParams::from_create(handle, &req.parameters).map_err(invalid)?),
            None,
        ),
    };
    check_accessible(req, node_id)?;
    let response = |record: &VolumeRecord, content_source| CreateVolumeResponse {
        volume: Some(Volume {
            capacity_bytes: record.capacity_bytes,
            volume_id: handle.to_owned(),
            volume_context: record.params.to_context(),
            content_source,
            accessible_topology: vec![topology(node_id)],
        }),
    };
    if let Some((class, source)) = fork {
        validate_handle(&source).map_err(|e| Error::NotFound(e.to_string()))?;
        let record = match volume::load(layout, handle)? {
            Some(existing) => existing,
            None => volume::create_fork(layout, node_id, handle, &class, &source, capacity_bytes)?,
        };
        return Ok(response(&record, req.volume_content_source.clone()));
    }
    let params = params.expect("an ordinary volume has its parameters");
    let record = match volume::load(layout, handle)? {
        Some(existing) => {
            if existing.capacity_bytes != capacity_bytes {
                return Err(Error::AlreadyExists(format!(
                    "volume {handle} exists with capacity {} bytes, not {capacity_bytes}",
                    existing.capacity_bytes
                )));
            }
            if existing.params != params {
                return Err(Error::AlreadyExists(format!(
                    "volume {handle} exists with other parameters ({:?})",
                    existing.params
                )));
            }
            existing
        }
        None => {
            let record = volume::create(layout, handle, &params, capacity_bytes)?;
            let started = params
                .start_from
                .as_deref()
                .map(|s| format!(", started from {s}"))
                .unwrap_or_default();
            eprintln!(
                "created volume {handle} on node {node_id} (agentfs volume {}, branch {}, profile \
                 {}, onDelete {}{started})",
                params.volume,
                params.branch_name(),
                params.profile,
                params.on_delete
            );
            record
        }
    };
    Ok(response(&record, None))
}

pub(crate) fn delete_volume(
    layout: &Layout,
    release: &ReleaseScan,
    req: &DeleteVolumeRequest,
) -> Result<(), Error> {
    required(&req.volume_id, "the volume id")?;
    let handle = req.volume_id.as_str();
    if validate_handle(handle).is_err() {
        // Not one of ours, so already gone.
        return Ok(());
    }
    // What an earlier delete of this name could not remove.
    if let Err(e) = remove_tree(&layout.deleting_dir(handle)) {
        eprintln!("{e}");
    }
    let Some(record) = volume::load(layout, handle)? else {
        return Ok(());
    };
    if layout.staged_record(handle).exists() {
        return Err(Error::FailedPrecondition(format!(
            "volume {handle} is staged on this node: its pods still use it"
        )));
    }
    let state = StateDir::new(layout.state(handle));
    // A fork's deletion asks for it to be finished (design §11.5): checked
    // before the lock, which the finisher holds while it publishes, and
    // again under it. A fork not yet published or abandoned is answered
    // here, without the lock: the decider asks it nothing else.
    if let Some(fork) = fork_gate(layout, handle, &state, &record)?
        && !fork.may_delete()
    {
        let unpublished = release::decide(&Worktree {
            has_history: true,
            uncommitted: 0,
            on_delete: record.params.on_delete,
            remotes: Declared::Pushed(&[]),
            fork: Some(fork),
        });
        if let Release::Wait(waiting) = unpublished {
            return Err(wait(handle, release, &waiting));
        }
    }
    let lock = if state.root().exists() {
        Some(volume::lock(&state, "agentfs-csi delete")?)
    } else {
        None
    };
    let fork = fork_gate(layout, handle, &state, &record)?;
    let decision = decide(handle, &state, record.params.on_delete, fork)?;
    match decision {
        Release::Delete => {
            let dir = layout.volume_dir(handle);
            // Under the lock, the volume goes aside in one rename (it no
            // longer exists for any call); the files go after the lock is
            // dropped, so the held lock file is never unlinked under it.
            let aside = layout.deleting_dir(handle);
            remove_tree(&aside)?;
            match fs::rename(&dir, &aside) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => {
                    return Err(Error::io(format!("moving {} aside", dir.display()), e));
                }
                _ => {}
            }
            drop(lock);
            if let Err(e) = remove_tree(&aside) {
                // The volume is gone; the next DeleteVolume of this name,
                // or the operator, removes what is left.
                eprintln!("deleted volume {handle}, but its files remain: {e}");
            } else {
                eprintln!(
                    "deleted volume {handle} ({}): nothing a remote lacks was on it",
                    dir.display()
                );
            }
            Ok(())
        }
        Release::Keep(kept) => {
            let to = layout.retained_json(handle);
            fs::rename(layout.csi_json(handle), &to)
                .map_err(|e| Error::io(format!("writing {}", to.display()), e))?;
            drop(lock);
            eprintln!(
                "deleted volume {handle}; its worktree stays in {} ({kept})",
                layout.volume_dir(handle).display()
            );
            Ok(())
        }
        Release::Wait(waiting) => {
            drop(lock);
            Err(wait(handle, release, &waiting))
        }
    }
}

/// DeleteVolume's answer to [`Release::Wait`]: `UNAVAILABLE`, naming why,
/// after waking the release scan, which commits and pushes (or finishes)
/// the worktree meanwhile. A remote that lacks the head contradicts the
/// scan's memory of a push that held it (the remote lost the branch since,
/// or was added after): the scan forgets it and pushes again.
fn wait(handle: &str, release: &ReleaseScan, waiting: &Waiting) -> Error {
    if let Waiting::Lacking { .. } = waiting {
        release.forget(handle);
    }
    release.wake();
    Error::Unavailable(format!(
        "volume {handle}: {waiting}; nothing deleted, DeleteVolume is retried"
    ))
}

/// The release decision for the worktree in `state`, from facts gathered
/// here ([`release::decide`]). Settings that cannot be read are
/// `FAILED_PRECONDITION`: what the remotes hold cannot be asked, so
/// nothing is deleted or released.
fn decide(
    handle: &str,
    state: &StateDir,
    on_delete: OnDelete,
    fork: Option<ForkState>,
) -> Result<Release, Error> {
    let unreadable = |e: &dyn std::error::Error| {
        Error::FailedPrecondition(format!(
            "volume {handle}: reading {}: {}; nothing deleted",
            state.root().display(),
            volume::chain(e)
        ))
    };
    if !state.git().join("config").exists() {
        let (records, _) = journal::read_records(&state.journal()).map_err(|e| unreadable(&e))?;
        return Ok(release::decide(&Worktree {
            has_history: false,
            uncommitted: records.len(),
            on_delete,
            remotes: Declared::Undeclared,
            fork,
        }));
    }
    // Under the state lock: the journal's backlog is what a stopped (or
    // killed) history service left uncommitted, and nothing appends to it.
    let (history, inspection) = core_volume::inspect(state).map_err(|e| unreadable(&e))?;
    let head = inspection.head;
    let settings: Settings = history.settings().clone();
    let mut seen = Vec::new();
    // A fork's remotes are not asked: publishing is its gate.
    if let (Some(head), None) = (head, fork) {
        for remote in &settings.remotes {
            let pusher = RemotePusher::open(
                state,
                &settings,
                remote,
                Arc::new(NullSink),
                &Telemetry::noop(),
            )
            .map_err(|e| unreadable(&e))?;
            seen.push(Seen {
                remote: remote.name.clone(),
                branch: pusher.branch_claim(head),
            });
        }
    }
    Ok(release::decide(&Worktree {
        has_history: head.is_some(),
        uncommitted: inspection.backlog,
        on_delete,
        remotes: Declared::of(&settings, &seen),
        fork,
    }))
}

/// For a fork: records that its deletion asks for it to be finished, and
/// its state; a status that cannot be read is `FAILED_PRECONDITION`.
/// `None` for any other worktree.
fn fork_gate(
    layout: &Layout,
    handle: &str,
    state: &StateDir,
    record: &VolumeRecord,
) -> Result<Option<ForkState>, Error> {
    if !state.git().join("config").exists() {
        return Ok(None);
    }
    let status = match fork_status(state) {
        Err(why) => return Err(Error::FailedPrecondition(format!("{why}; nothing deleted"))),
        Ok(None) => return Ok(None),
        Ok(Some(status)) => status,
    };
    let stored = volume::load(layout, handle)?;
    if !stored.as_ref().unwrap_or(record).finish_requested {
        let mut requested = record.clone();
        requested.finish_requested = true;
        volume::store(layout, handle, &requested)?;
        eprintln!("fork {handle}: finishing requested by DeleteVolume");
    }
    Ok(Some(status.state))
}

/// The fork status of the state area, or why it cannot be read.
fn fork_status(state: &StateDir) -> Result<Option<agentfs_core::fork::finish::ForkStatus>, String> {
    agentfs_core::fork::finish::status(state).map_err(|e| {
        format!(
            "reading the fork status of {}: {}",
            state.root().display(),
            volume::chain(&e)
        )
    })
}

/// Removes a directory tree, if it exists.
fn remove_tree(dir: &std::path::Path) -> Result<(), Error> {
    match fs::remove_dir_all(dir) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => {
            Err(Error::io(format!("removing {}", dir.display()), e))
        }
        _ => Ok(()),
    }
}

pub(crate) fn validate(
    layout: &Layout,
    req: &ValidateVolumeCapabilitiesRequest,
) -> Result<ValidateVolumeCapabilitiesResponse, Error> {
    required(&req.volume_id, "the volume id")?;
    if req.volume_capabilities.is_empty() {
        return Err(Error::InvalidArgument(
            "volume capabilities are required".into(),
        ));
    }
    let handle = req.volume_id.as_str();
    let not_found = || Error::NotFound(format!("volume {handle} does not exist"));
    validate_handle(handle).map_err(|_| not_found())?;
    if volume::load(layout, handle)?.is_none() {
        return Err(not_found());
    }
    Ok(match volume::check_capabilities(&req.volume_capabilities) {
        Ok(()) => ValidateVolumeCapabilitiesResponse {
            confirmed: Some(Confirmed {
                volume_context: req.volume_context.clone(),
                volume_capabilities: req.volume_capabilities.clone(),
                parameters: req.parameters.clone(),
                mutable_parameters: req.mutable_parameters.clone(),
            }),
            message: String::new(),
        },
        Err(e) => ValidateVolumeCapabilitiesResponse {
            confirmed: None,
            message: e.to_string(),
        },
    })
}

#[tonic::async_trait]
impl Controller for ControllerService {
    async fn create_volume(
        &self,
        request: Request<CreateVolumeRequest>,
    ) -> Result<Response<CreateVolumeResponse>, Status> {
        let req = request.into_inner();
        self.run(move |l, node, _| create_volume(l, node, &req))
            .await
            .map(Response::new)
    }

    async fn delete_volume(
        &self,
        request: Request<DeleteVolumeRequest>,
    ) -> Result<Response<DeleteVolumeResponse>, Status> {
        let req = request.into_inner();
        self.run(move |l, _, release| delete_volume(l, release, &req))
            .await?;
        Ok(Response::new(DeleteVolumeResponse {}))
    }

    async fn controller_publish_volume(
        &self,
        _request: Request<ControllerPublishVolumeRequest>,
    ) -> Result<Response<ControllerPublishVolumeResponse>, Status> {
        Err(Status::unimplemented(
            "agentfs-csi has no attach step (attachRequired: false): a worktree's pods run on its \
             node",
        ))
    }

    async fn controller_unpublish_volume(
        &self,
        _request: Request<ControllerUnpublishVolumeRequest>,
    ) -> Result<Response<ControllerUnpublishVolumeResponse>, Status> {
        Err(Status::unimplemented(
            "agentfs-csi has no attach step (attachRequired: false)",
        ))
    }

    async fn validate_volume_capabilities(
        &self,
        request: Request<ValidateVolumeCapabilitiesRequest>,
    ) -> Result<Response<ValidateVolumeCapabilitiesResponse>, Status> {
        let req = request.into_inner();
        self.run(move |l, _, _| validate(l, &req))
            .await
            .map(Response::new)
    }

    async fn controller_get_capabilities(
        &self,
        _request: Request<ControllerGetCapabilitiesRequest>,
    ) -> Result<Response<ControllerGetCapabilitiesResponse>, Status> {
        Ok(Response::new(ControllerGetCapabilitiesResponse {
            capabilities: [
                rpc::Type::CreateDeleteVolume,
                rpc::Type::SingleNodeMultiWriter,
                rpc::Type::CloneVolume,
            ]
            .into_iter()
            .map(capability)
            .collect(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::csi::v1::TopologyRequirement;
    use crate::csi::v1::VolumeCapability;
    use crate::csi::v1::volume_capability::access_mode::Mode;
    use crate::csi::v1::volume_capability::{AccessMode, AccessType, MountVolume};

    fn request(requisite: &[&str]) -> CreateVolumeRequest {
        CreateVolumeRequest {
            name: "v".into(),
            volume_capabilities: vec![VolumeCapability {
                access_mode: Some(AccessMode {
                    mode: Mode::SingleNodeWriter as i32,
                }),
                access_type: Some(AccessType::Mount(MountVolume::default())),
            }],
            accessibility_requirements: Some(TopologyRequirement {
                requisite: requisite.iter().map(|n| topology(n)).collect(),
                preferred: Vec::new(),
            }),
            ..Default::default()
        }
    }

    #[test]
    fn only_this_nodes_volumes_are_provisioned_here() {
        check_accessible(&request(&[]), "node-a").unwrap();
        check_accessible(&request(&["node-a"]), "node-a").unwrap();
        check_accessible(&request(&["node-b", "node-a"]), "node-a").unwrap();
        let err = check_accessible(&request(&["node-b"]), "node-a").unwrap_err();
        let Error::ResourceExhausted(message) = err else {
            panic!("{err:?}");
        };
        assert!(message.contains("node node-b"), "{message}");
        // A segment without this driver's key does not constrain.
        let mut req = request(&[]);
        req.accessibility_requirements
            .as_mut()
            .unwrap()
            .requisite
            .push(Topology {
                segments: [("zone".to_owned(), "z1".to_owned())].into(),
            });
        check_accessible(&req, "node-a").unwrap();
    }
}
