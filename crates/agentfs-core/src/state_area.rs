//! The state area as the Volume domain reaches it (docs/trees.md, "Volume
//! identity"): the [`StateArea`] port over the tree actor. `inspect` reads
//! `format` and the identity file only; `create` makes a volume with a new id;
//! `open` recovers the tree actor and keeps it for the other adapters.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use agentfs_contracts::audit::{Audited, Principal};
use agentfs_contracts::name::VolumeName;
use agentfs_contracts::pod::PodContext;
use agentfs_contracts::ports::{Found, QueueFacts, StateArea, StoreError, VolumeStore};
use agentfs_contracts::tree::{ForkId, TreeId};
use agentfs_contracts::volume::{
    EnsureOutcome, IoKind, VolumeError, VolumeEvent, VolumeId, VolumeIdentity, VolumeState,
};

use crate::tree_actor::{self, TreeActor};

/// A volume's state area at one path.
pub struct TreeStateArea {
    root: PathBuf,
    actor: Option<Arc<Mutex<TreeActor>>>,
}

impl TreeStateArea {
    pub fn at(root: &Path) -> Self {
        Self {
            root: root.to_owned(),
            actor: None,
        }
    }

    /// The actor `open` recovered; `None` before.
    pub fn actor(&self) -> Option<Arc<Mutex<TreeActor>>> {
        self.actor.clone()
    }
}

fn io_kind(error: &io::Error) -> IoKind {
    match error.raw_os_error() {
        Some(libc::ENOSPC) => IoKind::NoSpace,
        Some(errno) => IoKind::Other(errno),
        None => IoKind::Other(libc::EIO),
    }
}

/// The Volume domain's word for a tree actor's failure.
fn volume_error(error: &tree_actor::Error) -> VolumeError {
    match error {
        tree_actor::Error::Busy => VolumeError::Busy,
        tree_actor::Error::NotCreated => VolumeError::NotCreated,
        tree_actor::Error::Format => VolumeError::OtherFormat,
        tree_actor::Error::Io(e) => VolumeError::NotDurable(io_kind(e)),
        other => VolumeError::NotDurable(match other.errno() {
            libc::ENOSPC => IoKind::NoSpace,
            errno => IoKind::Other(errno),
        }),
    }
}

impl StateArea for TreeStateArea {
    fn inspect(&self) -> Result<Found, VolumeError> {
        tree_actor::inspect(&self.root).map_err(|e| VolumeError::NotDurable(io_kind(&e)))
    }

    /// [`TreeActor::create`] takes the state area's lock and inspects again
    /// under it; a volume there by now (another launcher raced) is answered
    /// `Found`, with nothing written.
    fn create(
        &mut self,
        name: &VolumeName,
        at: &PodContext,
    ) -> Result<(VolumeIdentity, EnsureOutcome), VolumeError> {
        let identity = VolumeIdentity {
            name: name.clone(),
            id: VolumeId(uuid::Uuid::new_v4()),
        };
        match TreeActor::create(&self.root, &identity, at) {
            Ok(actor) => {
                drop(actor);
                Ok((identity, EnsureOutcome::Created))
            }
            Err(tree_actor::Error::Exists) => match self.inspect()? {
                Found::Volume(found) => Ok((found, EnsureOutcome::Found)),
                Found::Other => Err(VolumeError::OtherFormat),
                // Another create is under way.
                Found::Nothing => Err(VolumeError::Busy),
            },
            Err(e) => Err(volume_error(&e)),
        }
    }

    fn open(&mut self, at: &PodContext) -> Result<VolumeIdentity, VolumeError> {
        let actor = TreeActor::open(&self.root, at).map_err(|e| volume_error(&e))?;
        let identity = actor.identity().clone();
        self.actor = Some(Arc::new(Mutex::new(actor)));
        Ok(identity)
    }
}

/// Slice S6 builds the volume's row. `ensure` reads none.
impl VolumeStore for TreeStateArea {
    fn load(&self) -> Result<Option<Audited<VolumeState>>, StoreError> {
        todo!("S6")
    }

    fn record(
        &mut self,
        state: VolumeState,
        event: &VolumeEvent,
        by: &Principal,
        at: &PodContext,
    ) -> Result<(), StoreError> {
        let _ = (state, event, by, at);
        todo!("S6")
    }
}

/// Slice S6 asks these. `ensure` asks none.
impl QueueFacts for TreeStateArea {
    fn queued(&self) -> Vec<TreeId> {
        todo!("S6")
    }

    fn pending_from(&self, fork: ForkId) -> bool {
        let _ = fork;
        todo!("S6")
    }

    fn has_child(&self, fork: ForkId) -> bool {
        let _ = fork;
        todo!("S6")
    }
}
