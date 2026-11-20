//! Who did what, and when: the principals that act and the audit stamps every
//! row carries, written by the store adapter, never by a domain
//! (docs/target-architecture/system.md, invariant S6).

use serde::{Deserialize, Serialize};

use crate::clients::ClientId;
use crate::name::LauncherId;

/// Who caused a change. Text in an event commit's `Agentfs-By` trailer:
/// `client <ClientId>`, `launcher <id>`, or `agentfs`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Principal {
    Client(ClientId),
    Launcher(LauncherId),
    /// agentfs itself: a reaction to an event (a fork ended at its last detach).
    Agentfs,
}

/// Nanoseconds since the Unix epoch. Audit only: no rule reads it (ledger R2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct UnixNanos(pub u64);

/// When and by whom.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Stamp {
    pub at: UnixNanos,
    pub by: Principal,
}

/// A row with its audit fields. Only the store adapter builds one: `created` is
/// set once, when the row is first written, and kept on every change; `changed`
/// is the latest write's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Audited<T> {
    pub value: T,
    pub created: Stamp,
    pub changed: Stamp,
}
