//! The CSI Controller service (design §14.2).
//!
//! - CreateVolume makes `<base>/<pv-name>/{backing,state,csi.json}` and
//!   initialises the state area in-process. Nothing is cloned or fetched.
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
use crate::params::{OnDelete, VolumeParams};
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
    if req.volume_content_source.is_some() {
        return Err(Error::InvalidArgument(
            "volume content sources (snapshots, clones) are not supported".into(),
        ));
    }
    let params = VolumeParams::from_create(handle, &req.parameters)
        .map_err(|e| Error::InvalidArgument(e.to_string()))?;
    let capacity_bytes = capacity(req)?;
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
    let Some(record) = volume::load(layout, handle)? else {
        return Ok(());
    };
    if let Some(node) = &record.published_node {
        return Err(Error::FailedPrecondition(format!(
            "volume {handle} is still attached to node {node}"
        )));
    }
    let state = StateDir::new(layout.state(handle));
    let lock = if state.root().exists() {
        Some(volume::lock(&state, "agentfs-csi delete")?)
    } else {
        None
    };
    match record.params.on_delete {
        OnDelete::Delete => {
            let dir = layout.volume_dir(handle);
            if state.git().join("config").exists() {
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
    // A volume that is gone is not attached anywhere.
    let Some(mut record) = volume::load(layout, handle)? else {
        return Ok(());
    };
    match record.published_node.clone() {
        Some(n) if req.node_id.is_empty() || n == req.node_id => {
            record.published_node = None;
            volume::store(layout, handle, &record)?;
            eprintln!("detached volume {handle} from node {n}");
        }
        _ => {}
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
            ]
            .into_iter()
            .map(capability)
            .collect(),
        }))
    }
}
