//! The state area as the Volume domain reaches it (docs/trees.md, "Volume
//! identity"): the [`StateArea`] port over the tree actor. `inspect` reads
//! `format` and the identity file only; `create` makes a volume with a new id;
//! `open` recovers the tree actor and keeps it, served, for the other adapters.
//! Once open, it is the Volume domain's row store and queue facts too, through
//! the [`TreeStore`] it makes over that actor (docs/trees.md, "Volume
//! lifecycle").

use std::path::{Path, PathBuf};
use std::sync::Arc;

use agentfs_contracts::audit::{Audited, Principal};
use agentfs_contracts::name::VolumeName;
use agentfs_contracts::pod::PodContext;
use agentfs_contracts::ports::{Found, QueueFacts, StateArea, StoreError, VolumeStore};
use agentfs_contracts::tree::TreeId;
use agentfs_contracts::volume::{
    EnsureOutcome, VolumeError, VolumeEvent, VolumeId, VolumeIdentity, VolumeState,
};

use crate::store::{TreeStore, io_kind};
use crate::tree_actor::{self, Recording};

/// A volume's state area at one path.
pub struct TreeStateArea {
    root: PathBuf,
    /// The volume `open` recovered, and the store over it.
    served: Option<(Arc<Recording>, TreeStore)>,
}

impl TreeStateArea {
    pub fn at(root: &Path) -> Self {
        Self {
            root: root.to_owned(),
            served: None,
        }
    }

    /// The volume `open` recovered, served; `None` before.
    pub fn recording(&self) -> Option<Arc<Recording>> {
        self.served
            .as_ref()
            .map(|(recording, _)| Arc::clone(recording))
    }

    fn store(&self) -> Option<&TreeStore> {
        self.served.as_ref().map(|(_, store)| store)
    }
}

/// The Volume domain's word for a tree actor's failure; its kind from the
/// one mapping (`store::io_kind`).
fn volume_error(error: &tree_actor::Error) -> VolumeError {
    match error {
        tree_actor::Error::Busy => VolumeError::Busy,
        tree_actor::Error::NotCreated => VolumeError::NotCreated,
        tree_actor::Error::Format => VolumeError::OtherFormat,
        other => VolumeError::NotDurable(io_kind(other)),
    }
}

impl StateArea for TreeStateArea {
    fn inspect(&self) -> Result<Found, VolumeError> {
        tree_actor::inspect(&self.root)
            .map_err(|e| VolumeError::NotDurable(io_kind(&tree_actor::Error::Io(e))))
    }

    /// [`Recording::create`] takes the state area's lock and inspects again
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
        match Recording::create(&self.root, &identity, at) {
            Ok(recording) => {
                // Every record durable before the call returns.
                drop(recording);
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
        let recording = Recording::open(&self.root, at).map_err(|e| volume_error(&e))?;
        let identity = recording
            .actor()
            .map_err(|e| volume_error(&e))?
            .identity()
            .clone();
        let store = TreeStore::new(Arc::clone(&recording));
        self.served = Some((recording, store));
        Ok(identity)
    }
}

/// The volume's row, on the store `open` made: before it, `Stopped` (no row
/// was read), and no write.
impl VolumeStore for TreeStateArea {
    fn load(&self) -> Result<Option<Audited<VolumeState>>, StoreError> {
        self.store().ok_or(StoreError::Stopped)?.load()
    }

    fn record(
        &mut self,
        state: VolumeState,
        event: &VolumeEvent,
        by: &Principal,
        at: Option<&PodContext>,
    ) -> Result<(), StoreError> {
        match self.served.as_mut() {
            Some((_, store)) => store.record(state, event, by, at),
            None => Err(StoreError::Stopped),
        }
    }
}

/// The queues, on the store `open` made: before it, `Stopped` (no queue was
/// read).
impl QueueFacts for TreeStateArea {
    fn queued(&self) -> Result<Vec<TreeId>, StoreError> {
        self.store().ok_or(StoreError::Stopped)?.queued()
    }
}
