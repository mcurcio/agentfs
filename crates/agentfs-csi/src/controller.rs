//! The CSI Controller service (design §14.2).
//!
//! - CreateVolume makes `<base>/<pv-name>/{backing,state,csi.json}` and
//!   initialises the state area in-process. With a fork class
//!   (`fork: "true"`) and a `dataSource` claim, it makes a fork of that
//!   volume (design §11.5) from its local history, without its lock or
//!   any credentials; nothing is fetched, and nothing else is cloned.
//! - DeleteVolume of a fork asks for it to be finished (`finish_requested`
//!   in `csi.json`) and answers `UNAVAILABLE` until the node plugin's
//!   publisher has published it; then it applies `onDelete`.
//! - DeleteVolume removes the directory (`onDelete: delete`) or keeps it
//!   for a static PV (`retain`, the default), renaming `csi.json` to
//!   `csi.retained.json`. It refuses while the volume is attached to a
//!   node or its state area is locked, and `delete` refuses while a
//!   remote lacks commits of the branch (the history service's final
//!   push at unstage failed): removing the directory would lose them.
//!   `delete` renames the directory to `.deleting.<handle>` under the
//!   state lock and removes it after releasing the lock, so that on NFS
//!   the held lock file is never unlinked (a silly rename would keep the
//!   directory from being removed).
//! - ControllerPublish only records the node in `csi.json`: with
//!   `attachRequired: true`, Kubernetes' attach/detach controller then
//!   refuses to use a ReadWriteOnce volume from a second node
//!   (Multi-Attach), and so does this service (`FAILED_PRECONDITION`).
//!   A static PV is adopted (created) here, since attach precedes
//!   NodeStage.
//!
//! Calls are serialised: each is a few local file operations.

use std::fs;
use std::io;
use std::sync::Arc;

use tokio::sync::Mutex;
use tonic::{Request, Response, Status};

use agentfs_core::config::Settings;
use agentfs_core::push;
use agentfs_core::volume::StateDir;

use crate::csi::v1::controller_server::Controller;
use crate::csi::v1::controller_service_capability::{self, rpc};
use crate::csi::v1::volume_content_source;
use crate::csi::v1::validate_volume_capabilities_response::Confirmed;
use crate::csi::v1::{
    ControllerGetCapabilitiesRequest, ControllerGetCapabilitiesResponse,
    ControllerPublishVolumeRequest, ControllerPublishVolumeResponse, ControllerServiceCapability,
    ControllerUnpublishVolumeRequest, ControllerUnpublishVolumeResponse, CreateVolumeRequest,
    CreateVolumeResponse, DeleteVolumeRequest, DeleteVolumeResponse,
    ValidateVolumeCapabilitiesRequest, ValidateVolumeCapabilitiesResponse, Volume,
};
use crate::error::Error;
use crate::layout::{Layout, validate_handle, validate_node};
use crate::params::{ForkClass, OnDelete, VolumeParams};
use crate::volume;

#[derive(Clone)]
pub struct ControllerService {
    layout: Arc<Layout>,
    serial: Arc<Mutex<()>>,
}

impl ControllerService {
    pub fn new(layout: Layout) -> Self {
        Self {
            layout: Arc::new(layout),
            serial: Arc::new(Mutex::new(())),
        }
    }

    /// Runs `f` on the blocking pool, one call at a time.
    async fn run<T: Send + 'static>(
        &self,
        f: impl FnOnce(&Layout) -> Result<T, Error> + Send + 'static,
    ) -> Result<T, Status> {
        let _serial = self.serial.lock().await;
        let layout = self.layout.clone();
        tokio::task::spawn_blocking(move || f(&layout))
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

pub(crate) fn create_volume(
    layout: &Layout,
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
            return Err(Error::InvalidArgument(
                "snapshots are not supported".into(),
            ));
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
    if let Some((class, source)) = fork {
        validate_handle(&source).map_err(|e| Error::NotFound(e.to_string()))?;
        let record = match volume::load(layout, handle)? {
            Some(existing) => existing,
            None => volume::create_fork(layout, handle, &class, &source, capacity_bytes)?,
        };
        return Ok(CreateVolumeResponse {
            volume: Some(Volume {
                capacity_bytes: record.capacity_bytes,
                volume_id: handle.to_owned(),
                volume_context: record.params.to_context(),
                content_source: req.volume_content_source.clone(),
                accessible_topology: Vec::new(),
            }),
        });
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
            let record = volume::create(layout, handle, &params, capacity_bytes, false)?;
            eprintln!(
                "created volume {handle} (agentfs volume {}, branch {}, profile {}, onDelete {})",
                params.volume,
                params.branch_name(),
                params.profile,
                params.on_delete
            );
            record
        }
    };
    Ok(CreateVolumeResponse {
        volume: Some(Volume {
            capacity_bytes: record.capacity_bytes,
            volume_id: handle.to_owned(),
            volume_context: record.params.to_context(),
            content_source: None,
            accessible_topology: Vec::new(),
        }),
    })
}

pub(crate) fn delete_volume(layout: &Layout, req: &DeleteVolumeRequest) -> Result<(), Error> {
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
    // A record of the multi-node build too: it can still be deleted.
    let Some((record, legacy)) = volume::load_any(layout, handle)? else {
        return Ok(());
    };
    let mut attached: Vec<&str> = legacy
        .iter()
        .flat_map(|f| f.published_nodes.iter().map(String::as_str))
        .chain(record.published_node.as_deref())
        .collect();
    attached.sort_unstable();
    attached.dedup();
    if !attached.is_empty() {
        return Err(Error::FailedPrecondition(format!(
            "volume {handle} is still attached to node {}",
            attached.join(", ")
        )));
    }
    let state = StateDir::new(layout.state(handle));
    let lock = if state.root().exists() {
        Some(volume::lock(&state, "agentfs-csi delete")?)
    } else {
        None
    };
    // A fork is deleted only once it is published (design §11.5): its
    // deletion is the request to finish it, which the node plugin carries
    // out.
    let mut is_fork = false;
    if state.git().join("config").exists() {
        match fork_status(&state) {
            Err(why) => {
                return Err(Error::FailedPrecondition(format!(
                    "{why}; nothing deleted"
                )));
            }
            Ok(Some(status)) => {
                is_fork = true;
                if !record.finish_requested {
                    let mut requested = record.clone();
                    requested.finish_requested = true;
                    volume::store_any(layout, handle, &requested, legacy.as_ref())?;
                    eprintln!("fork {handle}: finishing requested by DeleteVolume");
                }
                if !status.state.may_delete() {
                    return Err(Error::Unavailable(format!(
                        "fork {handle} is {}: DeleteVolume completes once it is published or \
                         abandoned (the node plugin publishes it)",
                        status.state.as_str()
                    )));
                }
            }
            Ok(None) => {}
        }
    }
    match record.params.on_delete {
        OnDelete::Delete => {
            let dir = layout.volume_dir(handle);
            // Publishing is a fork's gate: its remote holds its history.
            if !is_fork && state.git().join("config").exists() {
                let lacking = unpushed(&state)?;
                if !lacking.is_empty() {
                    let list: Vec<String> = lacking
                        .iter()
                        .map(|(remote, n)| format!("{remote} lacks {n} commit(s)"))
                        .collect();
                    return Err(Error::FailedPrecondition(format!(
                        "volume {handle} has history no remote holds ({}); nothing deleted. Push \
                         it (`agentfs --state {} push`), or remove {} by hand to discard it \
                         (docs/csi.md §9)",
                        list.join(", "),
                        state.root().display(),
                        dir.display()
                    )));
                }
            }
            // Under the lock, the volume goes aside in one rename (it no
            // longer exists for any call); the files go after the lock is
            // dropped. On NFS, unlinking the lock file this process holds
            // open would leave a silly-rename `.nfs*` file behind, and the
            // directory could never be removed.
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
                eprintln!("deleted volume {handle} ({})", dir.display());
            }
            return Ok(());
        }
        OnDelete::Retain => {
            let to = layout.retained_json(handle);
            fs::rename(layout.csi_json(handle), &to)
                .map_err(|e| Error::io(format!("writing {}", to.display()), e))?;
            eprintln!(
                "deleted volume {handle}; its files and history are retained in {} (onDelete: \
                 retain)",
                layout.volume_dir(handle).display()
            );
        }
    }
    drop(lock);
    Ok(())
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

/// Remotes that lack commits of the volume's branch, with how many: what
/// `onDelete: delete` would destroy. The history service pushes once more
/// when it stops, but that push can fail, and nothing pushes an unstaged
/// volume.
fn unpushed(state: &StateDir) -> Result<Vec<(String, usize)>, Error> {
    let what = |e: &dyn std::error::Error| {
        Error::FailedPrecondition(format!(
            "reading the push state of {}: {}; nothing deleted",
            state.root().display(),
            volume::chain(e)
        ))
    };
    let settings = Settings::load(&state.git()).map_err(|e| what(&e))?;
    let status = push::status(state, &settings).map_err(|e| what(&e))?;
    Ok(status
        .into_iter()
        .filter_map(|r| match r.unpushed {
            Some(n) if n > 0 => Some((r.name, n)),
            _ => None,
        })
        .collect())
}

pub(crate) fn publish(layout: &Layout, req: &ControllerPublishVolumeRequest) -> Result<(), Error> {
    required(&req.volume_id, "the volume id")?;
    required(&req.node_id, "the node id")?;
    let cap = req
        .volume_capability
        .as_ref()
        .ok_or_else(|| Error::InvalidArgument("the volume capability is required".into()))?;
    volume::check_capability(cap)?;
    let handle = req.volume_id.as_str();
    let not_found = || Error::NotFound(format!("volume {handle} does not exist"));
    validate_handle(handle).map_err(|_| not_found())?;
    let mut record = match volume::load(layout, handle)? {
        Some(record) => record,
        // A static PV: its volumeAttributes name a profile.
        None if req.volume_context.contains_key("profile") => {
            let params = VolumeParams::from_context(handle, &req.volume_context)
                .map_err(|e| Error::InvalidArgument(e.to_string()))?;
            let record = volume::create(layout, handle, &params, 0, true)?;
            eprintln!(
                "adopted static volume {handle} (agentfs volume {})",
                params.volume
            );
            record
        }
        None => return Err(not_found()),
    };
    let node = req.node_id.as_str();
    let unknown_node = || {
        Error::NotFound(format!(
            "node {node} has no agentfs-csi node plugin (no {})",
            layout.node_marker(node).display()
        ))
    };
    validate_node(node).map_err(|_| unknown_node())?;
    if !layout.node_marker(node).exists() {
        return Err(unknown_node());
    }
    match &record.published_node {
        Some(n) if n == node => Ok(()),
        Some(other) => Err(Error::FailedPrecondition(format!(
            "volume {handle} is attached to node {other}; one node per volume (design §14.2)"
        ))),
        None => {
            record.published_node = Some(node.to_owned());
            volume::store(layout, handle, &record)?;
            eprintln!("attached volume {handle} to node {node}");
            Ok(())
        }
    }
}

pub(crate) fn unpublish(
    layout: &Layout,
    req: &ControllerUnpublishVolumeRequest,
) -> Result<(), Error> {
    required(&req.volume_id, "the volume id")?;
    let handle = req.volume_id.as_str();
    if validate_handle(handle).is_err() {
        return Ok(());
    }
    // A volume that is gone is not attached anywhere. A record of the
    // multi-node build is detached node by node, and written in this
    // build's format once no node is left.
    let Some((mut record, mut legacy)) = volume::load_any(layout, handle)? else {
        return Ok(());
    };
    // No node id: every node.
    let detaches = |n: &String| req.node_id.is_empty() || *n == req.node_id;
    let mut detached: Vec<String> = record
        .published_node
        .take_if(|n| detaches(n))
        .into_iter()
        .collect();
    if let Some(fields) = &mut legacy {
        detached.extend(
            fields
                .published_nodes
                .iter()
                .filter(|n| detaches(n))
                .cloned(),
        );
        fields.published_nodes.retain(|n| !detaches(n));
    }
    if !detached.is_empty() || legacy.is_some() {
        volume::store_any(layout, handle, &record, legacy.as_ref())?;
    }
    detached.sort_unstable();
    detached.dedup();
    for n in detached {
        eprintln!("detached volume {handle} from node {n}");
    }
    Ok(())
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
        self.run(move |l| create_volume(l, &req))
            .await
            .map(Response::new)
    }

    async fn delete_volume(
        &self,
        request: Request<DeleteVolumeRequest>,
    ) -> Result<Response<DeleteVolumeResponse>, Status> {
        let req = request.into_inner();
        self.run(move |l| delete_volume(l, &req)).await?;
        Ok(Response::new(DeleteVolumeResponse {}))
    }

    async fn controller_publish_volume(
        &self,
        request: Request<ControllerPublishVolumeRequest>,
    ) -> Result<Response<ControllerPublishVolumeResponse>, Status> {
        let req = request.into_inner();
        self.run(move |l| publish(l, &req)).await?;
        Ok(Response::new(ControllerPublishVolumeResponse::default()))
    }

    async fn controller_unpublish_volume(
        &self,
        request: Request<ControllerUnpublishVolumeRequest>,
    ) -> Result<Response<ControllerUnpublishVolumeResponse>, Status> {
        let req = request.into_inner();
        self.run(move |l| unpublish(l, &req)).await?;
        Ok(Response::new(ControllerUnpublishVolumeResponse {}))
    }

    async fn validate_volume_capabilities(
        &self,
        request: Request<ValidateVolumeCapabilitiesRequest>,
    ) -> Result<Response<ValidateVolumeCapabilitiesResponse>, Status> {
        let req = request.into_inner();
        self.run(move |l| validate(l, &req))
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
                rpc::Type::PublishUnpublishVolume,
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
    use crate::csi::v1::VolumeCapability;
    use crate::csi::v1::volume_capability::access_mode::Mode;
    use crate::csi::v1::volume_capability::{AccessMode, AccessType, MountVolume};
    use crate::records::{MultiNodeFields, VolumeRecord};

    /// A volume whose `csi.json` the multi-node build wrote, attached to
    /// `nodes`.
    fn multi_node_volume(nodes: &[&str]) -> (tempfile::TempDir, Layout) {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout {
            base: dir.path().join("base"),
            node_state: dir.path().join("node"),
            mount_dir: dir.path().join("mnt"),
            profiles: dir.path().join("profiles"),
        };
        let record = VolumeRecord {
            params: VolumeParams {
                profile: "p".into(),
                volume: "v".into(),
                branch: None,
                on_delete: OnDelete::Delete,
                workload_mounts: Vec::new(),
            },
            capacity_bytes: 0,
            created: 1,
            published_node: None,
            static_volume: false,
            finish_requested: false,
        };
        let fields = MultiNodeFields {
            multi_node: true,
            published_nodes: nodes.iter().map(|n| (*n).to_owned()).collect(),
        };
        volume::store_any(&layout, "v", &record, Some(&fields)).unwrap();
        (dir, layout)
    }

    fn delete(layout: &Layout) -> Result<(), Error> {
        delete_volume(
            layout,
            &DeleteVolumeRequest {
                volume_id: "v".into(),
                ..Default::default()
            },
        )
    }

    fn detach(layout: &Layout, node: &str) -> Result<(), Error> {
        unpublish(
            layout,
            &ControllerUnpublishVolumeRequest {
                volume_id: "v".into(),
                node_id: node.into(),
                ..Default::default()
            },
        )
    }

    #[test]
    fn a_multi_node_volume_can_be_detached_and_deleted() {
        let (_dir, layout) = multi_node_volume(&["n1", "n2"]);
        let err = delete(&layout).unwrap_err();
        assert_eq!(
            err,
            Error::FailedPrecondition("volume v is still attached to node n1, n2".into())
        );
        detach(&layout, "n1").unwrap();
        let text = fs::read_to_string(layout.csi_json("v")).unwrap();
        assert!(
            text.contains("\"n2\"") && !text.contains("\"n1\""),
            "{text}"
        );
        assert!(delete(&layout).is_err(), "n2 is still attached");
        detach(&layout, "n2").unwrap();
        // No node left: written in this build's format.
        let text = fs::read_to_string(layout.csi_json("v")).unwrap();
        assert!(!text.contains("multi_node"), "{text}");
        assert!(volume::load(&layout, "v").unwrap().is_some());
        delete(&layout).unwrap();
        assert!(!layout.volume_dir("v").exists());
    }

    #[test]
    fn a_multi_node_volume_is_not_published() {
        let (_dir, layout) = multi_node_volume(&["n1"]);
        let err = publish(
            &layout,
            &ControllerPublishVolumeRequest {
                volume_id: "v".into(),
                node_id: "n1".into(),
                volume_capability: Some(VolumeCapability {
                    access_mode: Some(AccessMode {
                        mode: Mode::SingleNodeWriter as i32,
                    }),
                    access_type: Some(AccessType::Mount(MountVolume::default())),
                }),
                ..Default::default()
            },
        )
        .unwrap_err();
        let Error::FailedPrecondition(message) = err else {
            panic!("{err:?}");
        };
        assert!(message.contains(volume::MULTI_NODE), "{message}");
    }
}
