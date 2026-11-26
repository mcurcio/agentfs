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
use agentfs_contracts::clients::FileOwner;
use agentfs_contracts::name::VolumeName;
use agentfs_contracts::pod::PodContext;
use agentfs_contracts::ports::{Found, QueueFacts, StateArea, StoreError, VolumeStore};
use agentfs_contracts::tree::TreeId;
use agentfs_contracts::volume::{
    EnsureOutcome, VolumeError, VolumeEvent, VolumeId, VolumeIdentity, VolumeState,
};
use agentfs_os::file_lock::{FileLock, Mode};

use crate::store::{TreeStore, io_kind};
use crate::tree_actor::{self, Recording, Upkeep};

/// A volume's state area at one path.
pub struct TreeStateArea {
    root: PathBuf,
    /// How `create` and `open` serve the volume's journal.
    upkeep: Upkeep,
    /// The volume `open` recovered, and the store over it.
    served: Option<(Arc<Recording>, TreeStore)>,
}

impl TreeStateArea {
    /// The state area at `root`, served with `Upkeep::default()`.
    pub fn at(root: &Path) -> Self {
        Self {
            root: root.to_owned(),
            upkeep: Upkeep::default(),
            served: None,
        }
    }

    /// The upkeep `create` and `open` pass on.
    #[must_use]
    pub fn with_upkeep(mut self, upkeep: Upkeep) -> Self {
        self.upkeep = upkeep;
        self
    }

    /// Returns once no process holds the state area's lease: taking it,
    /// blocking, is the event that every holder is gone (a killed server's
    /// git children included); it is let go at once. What a launcher waits
    /// on before it starts a server again on a volume whose last start
    /// exited 75, and what a harness waits on after a kill. Blocks its
    /// thread: an async caller runs it on a thread of its own.
    ///
    /// # Errors
    ///
    /// A failed open or lock of the lease file (the state area gone).
    pub fn wait_for_lease(&self) -> std::io::Result<()> {
        let file = FileLock::open(&self.root.join(tree_actor::LOCK))?;
        drop(FileLock::wait(file, Mode::Exclusive)?);
        Ok(())
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

/// The Volume domain's word for a tree actor's failure at the state area
/// `root`; its kind from the one mapping (`store::io_kind`). The Volume
/// domain's `NotDurable` carries the kind alone, so the failure itself, and
/// the state area it is on, are said here (a full filesystem is named).
fn volume_error(root: &Path, error: &tree_actor::Error) -> VolumeError {
    match error {
        tree_actor::Error::LeaseHeld => VolumeError::Busy,
        tree_actor::Error::NotCreated => VolumeError::NotCreated,
        tree_actor::Error::Format => VolumeError::OtherFormat,
        tree_actor::Error::Rules(refusal) => VolumeError::RulesRefused(refusal.to_string()),
        other => {
            eprintln!(
                "tree NotDurable: the state area {}: {other}",
                root.display()
            );
            VolumeError::NotDurable(io_kind(other))
        }
    }
}

impl StateArea for TreeStateArea {
    fn inspect(&self) -> Result<Found, VolumeError> {
        tree_actor::inspect(&self.root)
            .map_err(|e| VolumeError::Unreadable(format!("{}: {e}", self.root.display())))
    }

    /// `Recording::create_with` takes the state area's lock and inspects again
    /// under it; a volume there by now (another launcher raced) is answered
    /// `Found`, with nothing written.
    fn create(
        &mut self,
        name: &VolumeName,
        root_owner: Option<FileOwner>,
        at: &PodContext,
    ) -> Result<(VolumeIdentity, EnsureOutcome), VolumeError> {
        let identity = VolumeIdentity {
            name: name.clone(),
            id: VolumeId(uuid::Uuid::new_v4()),
        };
        match Recording::create_with(&self.root, &identity, root_owner, at, self.upkeep.clone()) {
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
            Err(e) => Err(volume_error(&self.root, &e)),
        }
    }

    fn open(&mut self, at: &PodContext) -> Result<VolumeIdentity, VolumeError> {
        let recording = Recording::open_with(&self.root, at, self.upkeep.clone())
            .map_err(|e| volume_error(&self.root, &e))?;

        let identity = recording
            .actor()
            .map_err(|e| volume_error(&self.root, &e))?
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
        let result = match self.served.as_mut() {
            Some((_, store)) => store.record(state, event, by, at),
            None => Err(StoreError::Stopped),
        };
        if let Err(StoreError::NotDurable(kind)) = &result {
            eprintln!(
                "tree NotDurable: the state area {}: the volume's row was not written: {kind:?}",
                self.root.display()
            );
        }
        result
    }
}

/// The queues, on the store `open` made: before it, `Stopped` (no queue was
/// read).
impl QueueFacts for TreeStateArea {
    fn queued(&self) -> Result<Vec<TreeId>, StoreError> {
        self.store().ok_or(StoreError::Stopped)?.queued()
    }
}
