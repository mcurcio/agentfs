//! The state area as the Volume domain reaches it (docs/trees.md, "Volume
//! identity"): the [`StateArea`] port over the tree actor. `inspect` reads
//! `format` and the identity file only; `create` makes a volume with a new id;
//! `open` recovers the tree actor and keeps it, served, for the other adapters.
//! Once open, it is the Volume domain's row store and queue facts too, through
//! the [`TreeStore`] it makes over that actor (docs/trees.md, "Volume
//! lifecycle").

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use agentfs_contracts::audit::{Audited, Principal};
use agentfs_contracts::name::VolumeName;
use agentfs_contracts::pod::PodContext;
use agentfs_contracts::ports::{Found, QueueFacts, StateArea, StoreError, VolumeStore};
use agentfs_contracts::tree::{ForkId, TreeId};
use agentfs_contracts::volume::{
    EnsureOutcome, IoKind, VolumeError, VolumeEvent, VolumeId, VolumeIdentity, VolumeState,
};

use crate::store::TreeStore;
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

/// The volume's row, on the store `open` made: none before it, and no write.
impl VolumeStore for TreeStateArea {
    fn load(&self) -> Result<Option<Audited<VolumeState>>, StoreError> {
        self.store().map_or(Ok(None), VolumeStore::load)
    }

    fn record(
        &mut self,
        state: VolumeState,
        event: &VolumeEvent,
        by: &Principal,
        at: &PodContext,
    ) -> Result<(), StoreError> {
        match self.served.as_mut() {
            Some((_, store)) => store.record(state, event, by, at),
            None => Err(StoreError::Stopped),
        }
    }
}

/// The queues, on the store `open` made: before it, none holds anything.
impl QueueFacts for TreeStateArea {
    fn queued(&self) -> Vec<TreeId> {
        self.store().map_or_else(Vec::new, QueueFacts::queued)
    }

    fn pending_from(&self, fork: ForkId) -> bool {
        self.store().is_some_and(|store| store.pending_from(fork))
    }

    fn has_child(&self, fork: ForkId) -> bool {
        self.store().is_some_and(|store| store.has_child(fork))
    }
}
