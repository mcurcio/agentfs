//! The Clients domain's contract (docs/target-architecture/domains/clients.md),
//! M1's surface (slice S4): attach, rebind, detach, reconcile, attached.

use std::collections::BTreeSet;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::caps::LaunchCap;
use crate::name::{ClientLabel, LauncherId, NameError, WriterLabel};
use crate::pod::PodContext;
use crate::ports::StoreError;
use crate::tree::TreeId;

/// A client's number, unique per volume and never reused: the journal step
/// number of its attach.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Ordinal(pub u64);

/// A client, and the writer of every commit it saves. Names its writer alone, so
/// a commit needs no lookup. Text, the `Agentfs-Writer` trailer:
/// `<label>#<n>@<launcher>/<tree>`, e.g. `root-client#5@cli/root`; the label and
/// launcher hold no `#`, `@` or `/`, so it parses back.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ClientId {
    pub launcher: LauncherId,
    pub tree: TreeId,
    pub label: ClientLabel,
    pub n: Ordinal,
}

impl ClientId {
    /// The writer every commit of this client names: its text.
    pub fn writer(&self) -> WriterLabel {
        WriterLabel::of_client(self)
    }
}

impl fmt::Display for ClientId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}#{}@{}/{}",
            self.label, self.n.0, self.launcher, self.tree
        )
    }
}

impl FromStr for ClientId {
    type Err = NameError;

    /// The exact inverse of `Display`: split at the first `#`, the first `@`
    /// after it and the first `/` after that.
    fn from_str(text: &str) -> Result<Self, NameError> {
        let bad = || NameError {
            what: "client id",
            value: text.to_owned(),
            rule: "<label>#<n>@<launcher>/<tree>",
        };
        let (label, rest) = text.split_once('#').ok_or_else(bad)?;
        let (n, rest) = rest.split_once('@').ok_or_else(bad)?;
        let (launcher, tree) = rest.split_once('/').ok_or_else(bad)?;
        // The number's canonical text only, so the text is the id's one spelling.
        let n: u64 = n.parse().map_err(|_| bad())?;
        if text != format!("{label}#{n}@{launcher}/{tree}") {
            return Err(bad());
        }
        Ok(Self {
            launcher: LauncherId::parse(launcher)?,
            tree: tree.parse().map_err(|_| bad())?,
            label: ClientLabel::parse(label)?,
            n: Ordinal(n),
        })
    }
}

impl TryFrom<String> for ClientId {
    type Error = NameError;

    fn try_from(text: String) -> Result<Self, NameError> {
        text.parse()
    }
}

impl From<ClientId> for String {
    fn from(id: ClientId) -> Self {
        id.to_string()
    }
}

/// A set of clients, ordered.
pub type ClientSet = BTreeSet<ClientId>;

/// The view host's connection for one client. In M1 sessions are in-process
/// handles; the number is the history service's, unique per process run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SessionId(pub u64);

/// The owner every file of a view is presented as, until M2's file metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FileOwner {
    pub uid: u32,
    pub gid: u32,
}

/// What a client may do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Access {
    ReadWrite,
    ReadOnly,
}

/// How a live client sees its tree (views.md §3). M1 builds the default only.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Consistency {
    PerFile,
}

/// What a client may do and see, and the owner its view presents. Built only by
/// its constructors. Until Rules (M3) every client sees everything, so a grant
/// names no view profile; snapshots wait for pinned snapshots.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Grant {
    access: Access,
    tree: TreeId,
    consistency: Consistency,
    acts_as: FileOwner,
}

impl Grant {
    pub fn live_read_write(tree: TreeId, consistency: Consistency, acts_as: FileOwner) -> Self {
        Self {
            access: Access::ReadWrite,
            tree,
            consistency,
            acts_as,
        }
    }

    pub fn live_read_only(tree: TreeId, consistency: Consistency, acts_as: FileOwner) -> Self {
        Self {
            access: Access::ReadOnly,
            tree,
            consistency,
            acts_as,
        }
    }

    pub fn access(&self) -> Access {
        self.access
    }

    pub fn tree(&self) -> TreeId {
        self.tree
    }

    pub fn consistency(&self) -> Consistency {
        self.consistency
    }

    pub fn acts_as(&self) -> FileOwner {
        self.acts_as
    }
}

/// What `attach` returns and the launcher hands to Views. It holds no
/// capability: the session does, in the history service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    pub client: ClientId,
    pub session: SessionId,
    pub grant: Grant,
}

/// Why an attachment ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AttachmentEndReason {
    Detached,
    /// Absent from its launcher's `reconcile` (every restored attachment in M1:
    /// a view host dies with the service).
    Reconciled,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ClientError {
    /// The grant exceeds the launcher (C2).
    #[error("the grant exceeds the launcher's")]
    NotPermitted,
    #[error("no such client")]
    UnknownClient,
    #[error("the client is another launcher's")]
    NotItsLauncher,
    #[error("no such tree, or the fork has ended")]
    TreeUnknown,
    /// One writer per fork, for life (C4).
    #[error("the fork has had its writer")]
    ForkTaken,
    /// After `VolumeDraining`; attach only.
    #[error("the volume is draining")]
    Draining,
    #[error("the attachment could not be recorded: {0:?}")]
    NotDurable(crate::volume::IoKind),
    /// The tree actor stopped after a recording failure, or its state is
    /// damaged: nothing more is recorded until the volume is reopened.
    #[error("the volume has stopped; reopen it")]
    Stopped,
}

impl From<StoreError> for ClientError {
    /// A write that changed nothing for want of room or I/O is `NotDurable`;
    /// every other store failure leaves the volume unable to record.
    fn from(error: StoreError) -> Self {
        match error {
            StoreError::NotDurable(kind) => Self::NotDurable(kind),
            StoreError::Stopped | StoreError::Damaged(_) | StoreError::Refused(_) => Self::Stopped,
        }
    }
}

/// Published by Clients, in this order, in one step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientEvent {
    Attached(ClientId),
    AttachmentEnded {
        client: ClientId,
        why: AttachmentEndReason,
    },
    /// `attached(tree)` became empty: exactly once each time (C6).
    LastDetached {
        tree: TreeId,
    },
}

/// Volume's one question to Clients (volume.md §3, "Asked").
pub trait AttachedLookup {
    fn attached_any(&self) -> ClientSet;
}

/// The Clients domain (clients.md §3, M1's surface).
pub trait Clients: AttachedLookup {
    /// One step: the client's row, a fork's writer claim for a read-write grant
    /// on a fork, and `Attached`; then its `ClientCap`, for a read-write grant,
    /// is bound to `session`.
    ///
    /// # Errors
    ///
    /// `NotPermitted`, `TreeUnknown`, `ForkTaken`, `Draining`, `NotDurable`,
    /// `Stopped`.
    fn attach(
        &mut self,
        grant: Grant,
        label: ClientLabel,
        at: PodContext,
        session: SessionId,
        by: &LaunchCap,
    ) -> Result<Binding, ClientError>;

    /// Only a client restored at boot, not yet bound this run (its view host
    /// died with the service). One step: its row's `at` (`Rebound`); then its
    /// `ClientCap`, for a read-write grant, is bound to `session` as `attach`
    /// binds it. Its id, writer and a fork's claim stay its own.
    ///
    /// # Errors
    ///
    /// `UnknownClient` (never attached, or ended), `NotItsLauncher`,
    /// `NotPermitted` (bound this run), `Stopped`, `NotDurable`.
    fn rebind(
        &mut self,
        client: &ClientId,
        at: PodContext,
        session: SessionId,
        by: &LaunchCap,
    ) -> Result<Binding, ClientError>;

    /// Idempotent: a client already ended answers `Ok`.
    ///
    /// # Errors
    ///
    /// `UnknownClient` for one never attached; `NotItsLauncher`; `Stopped`,
    /// `NotDurable` when its end could not be recorded.
    fn detach(&mut self, client: &ClientId, by: &LaunchCap) -> Result<(), ClientError>;

    /// Ends this launcher's attachments not in `live`; answers the ended ones.
    /// An end that could not be recorded ends none: the answer is empty and
    /// the next boot's reconcile ends them.
    fn reconcile(&mut self, live: &ClientSet, by: &LaunchCap) -> Vec<ClientId>;

    fn attached(&self, tree: TreeId) -> ClientSet;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_id_text_round_trips() {
        let id = ClientId {
            launcher: LauncherId::parse("cli").unwrap(),
            tree: TreeId::Fork(70),
            label: ClientLabel::parse("sandbox-client").unwrap(),
            n: Ordinal(5),
        };
        let text = id.to_string();
        assert_eq!(text, "sandbox-client#5@cli/fork/70");
        assert_eq!(text.parse::<ClientId>().unwrap(), id);
        assert_eq!(id.writer().as_str(), text);
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, format!("\"{text}\""));
        assert_eq!(serde_json::from_str::<ClientId>(&json).unwrap(), id);
        let root = "root-client#1@cli/root".parse::<ClientId>().unwrap();
        assert_eq!(root.tree, TreeId::Root);
        for bad in [
            "",
            "root-client",
            "root-client#1",
            "root-client#1@cli",
            "root-client#x@cli/root",
            "root-client#01@cli/root",
            "root-client#1@cli/main",
            "a#b#1@cli/root",
            "root-client#1@c/l/root",
            "-a#1@cli/root",
        ] {
            assert!(bad.parse::<ClientId>().is_err(), "{bad}");
        }
    }

    #[test]
    fn each_store_failure_has_its_clients_word() {
        use crate::volume::IoKind;
        assert_eq!(
            ClientError::from(StoreError::NotDurable(IoKind::NoSpace)),
            ClientError::NotDurable(IoKind::NoSpace)
        );
        for stopped in [
            StoreError::Stopped,
            StoreError::Damaged("x".into()),
            StoreError::Refused("x".into()),
        ] {
            assert_eq!(ClientError::from(stopped), ClientError::Stopped);
        }
    }
}
