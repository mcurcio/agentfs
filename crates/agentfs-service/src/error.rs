//! What the service's calls refuse with (docs/library/service.md): the
//! failures its caller decides on. A busy state area is always
//! [`ServiceError::Volume`] with `VolumeError::Busy`, from every call, as
//! every call opens through the engine's state area.

use agentfs_contracts::ports::StoreError;
use agentfs_contracts::trees::TreeError;
use agentfs_contracts::volume::VolumeError;
use agentfs_engine::operator_settings::SettingsError;

#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    /// Volume's refusal or failure: `Busy` while another holds the state
    /// area, `OtherFormat`, `NotCreated`, ...
    #[error(transparent)]
    Volume(#[from] VolumeError),
    /// [`crate::init`]'s includes not written, or [`crate::Domains::stage`]'s
    /// overlay limit refused.
    #[error(transparent)]
    Settings(#[from] SettingsError),
    #[error("reading the forks' rows: {0}")]
    Forks(StoreError),
    #[error("reading the clients' rows: {0}")]
    Clients(StoreError),
    /// [`crate::end_fork`]'s refusal: `KeyUnknown`, ...
    #[error(transparent)]
    Fork(TreeError),
}
