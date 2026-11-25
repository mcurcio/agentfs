//! Who opens a volume, and the two calls that open one and return no
//! [`crate::Domains`] (docs/library/service.md): `init`, as `agentfs init`
//! makes or finds a volume, and `end_fork`, as `agentfs fork end` ends a
//! keyed fork. The caller's composition root mints the capabilities a
//! [`Launcher`] carries; the service mints none.

use std::fmt;
use std::path::{Path, PathBuf};

use agentfs_contracts::caps::{LaunchCap, Minter};
use agentfs_contracts::clients::{AttachedLookup, ClientSet};
use agentfs_contracts::name::ForkKey;
use agentfs_contracts::pod::PodContext;
use agentfs_contracts::ports::StateArea as _;
use agentfs_contracts::volume::{Ensured, Volume as _, VolumeSpec};
use agentfs_engine::operator_settings;
use agentfs_engine::state_area::TreeStateArea;
use agentfs_engine::tree_actor::Upkeep;
use agentfs_volume::VolumeService;

use crate::domains::{ClientsSlot, Domains, served};
use crate::error::ServiceError;
use crate::events::DomainEvents;

/// Who opens a volume, as its composition root minted it: the [`Minter`]
/// lent to Clients alone (each read-write client's capability), the
/// launcher's capability every call is made as, the pod context every call
/// is made at, and how the volume keeps its journal.
pub struct Launcher {
    minter: Minter,
    cap: LaunchCap,
    at: PodContext,
    upkeep: Upkeep,
}

impl fmt::Debug for Launcher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Launcher")
            .field("minter", &self.minter)
            .field("cap", &self.cap)
            .field("at", &self.at)
            .finish_non_exhaustive()
    }
}

impl Launcher {
    /// `minter` is lent to Clients alone (each read-write client's
    /// `ClientCap`); every call is made as `cap`, at `at`. The volume keeps
    /// its journal with `Upkeep::default()`.
    pub fn new(minter: Minter, cap: LaunchCap, at: PodContext) -> Self {
        Self {
            minter,
            cap,
            at,
            upkeep: Upkeep::default(),
        }
    }

    /// The same launcher, its volume keeping its journal by `upkeep`.
    #[must_use]
    pub fn with_upkeep(self, upkeep: Upkeep) -> Self {
        Self { upkeep, ..self }
    }

    /// The state area at `state`, served with this launcher's upkeep.
    fn area(&self, state: &Path) -> TreeStateArea {
        TreeStateArea::at(state).with_upkeep(self.upkeep.clone())
    }

    /// The launcher's parts, for [`Domains`].
    pub(crate) fn into_parts(self) -> (Minter, LaunchCap, PodContext, Upkeep) {
        (self.minter, self.cap, self.at, self.upkeep)
    }
}

/// `init`'s Clients: it attaches none and judges no release.
struct NoClients;

impl AttachedLookup for NoClients {
    fn attached_any(&self) -> ClientSet {
        ClientSet::new()
    }
}

/// Volume's `ensure` as `launcher`, the recovery every start makes, then
/// (when not empty) `includes` written as the operator settings' includes
/// under that recovery's lock, released before it returns.
///
/// # Errors
///
/// [`ServiceError::Volume`]: the ensure's refusal (`Busy`, `OtherFormat`,
/// `SpecMismatch`, ...) or a failed recovery; [`ServiceError::Settings`]: the
/// includes not written.
pub fn init(
    state: &Path,
    spec: VolumeSpec,
    launcher: &Launcher,
    includes: &[PathBuf],
) -> Result<Ensured, ServiceError> {
    let mut volume = VolumeService::new(launcher.area(state), NoClients, DomainEvents::default());
    let ensured = volume.ensure(spec, &launcher.at, &launcher.cap)?;
    let mut area = launcher.area(state);
    area.open(&launcher.at)?;
    if !includes.is_empty() {
        operator_settings::write_includes(state, includes)?;
    }
    drop(area);
    Ok(ensured)
}

/// Trees and Clients restored with no Volume (it judges no release); the
/// fork under `key` ends, dropped now if nothing holds it (else at a later
/// stage's settle). Holds the state area's lock while it runs.
///
/// # Errors
///
/// [`ServiceError::Volume`]: the state area not opened (`Busy` among
/// them); [`ServiceError::Forks`] or [`ServiceError::Clients`]: rows not
/// read; [`ServiceError::Fork`]: the end refused (`KeyUnknown`, ...).
pub fn end_fork(state: &Path, launcher: Launcher, key: &ForkKey) -> Result<(), ServiceError> {
    let (minter, cap, at, upkeep) = launcher.into_parts();
    let mut area = TreeStateArea::at(state).with_upkeep(upkeep);
    area.open(&at)?;
    let mut domains = Domains::restore(
        served(&area),
        (minter, cap, at),
        DomainEvents::default(),
        ClientsSlot::new(None),
        None,
    )?;
    let ended = domains.end_keyed(key).map_err(ServiceError::Fork);
    drop(domains);
    drop(area);
    ended
}
