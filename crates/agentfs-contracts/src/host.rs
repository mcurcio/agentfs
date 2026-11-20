//! The view host's control messages (docs/library/filesystem.md): what a
//! parent that runs `agentfs host` writes to the host's standard input and
//! reads back from it. Pure values, no I/O.
//!
//! The wire is one JSON object per line (`'\n'`). The host writes [`Hello`]
//! once it serves, then exactly one [`Reply`] per [`Request`], in order. The
//! parent sends a request only after the previous reply has arrived.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::name::{ClientLabel, ForkKey, VolumeName};
use crate::pod::PodContext;
use crate::tree::TreeId;
use crate::trees::ForkSettings;
use crate::volume::VolumeId;

/// The protocol's version, in [`Hello::v`].
pub const HOST_PROTOCOL: u32 = 1;

/// The host's first line: written once the volume is staged, its domains
/// restored, its view host started and its not-built settings logged; before
/// any request is read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub v: u32,
    pub volume: VolumeName,
    pub id: VolumeId,
}

/// A tree as the parent names it: the root, or the open fork under a key.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TreeRef {
    Root,
    Fork(ForkKey),
}

/// One view to open.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewRequest {
    pub tree: TreeRef,
    /// An existing empty directory, or a stale agentfs mount of this view's
    /// own source, which it stacks over.
    pub at: PathBuf,
    /// With the host's launcher, the tree and the access: what a restarted
    /// host rebinds the client by.
    pub label: ClientLabel,
    pub read_only: bool,
    /// The client's pod context: its saves' pod trailers.
    pub pod: PodContext,
}

/// What the parent asks the host, one per line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Open(ViewRequest),
    Close {
        at: PathBuf,
    },
    Reconcile,
    ForkCreate {
        upstream: TreeRef,
        key: ForkKey,
        settings: ForkSettings,
    },
    ForkEnd {
        key: ForkKey,
    },
}

/// The host's answer to one request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reply {
    Opened { tree: TreeId, stacked: bool },
    Closed,
    Reconciled { ended: u32 },
    Forked { tree: TreeId },
    Ended,
    Refused(Refusal),
}

/// A request the host did not carry out: its kind, and what to tell a person.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Refusal {
    pub kind: RefusalKind,
    pub message: String,
}

/// Why a request was refused. Exhaustive on purpose: a parent maps every
/// kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefusalKind {
    /// Clients' `ForkTaken`: the fork has had its one writer.
    ForkTaken,
    /// No fork under the key (open, `fork_create`'s upstream, `fork_end`).
    KeyUnknown,
    /// The key's fork has ended.
    KeyEnded,
    /// Trees' `NestedNotBuilt`.
    NestedNotBuilt,
    /// Trees' `ApprovalNotBuilt`.
    ApprovalNotBuilt,
    /// The view host's `MountFailed`.
    MountFailed,
    /// The view host's `Stale`: another source's stale mount.
    Stale,
    /// A view is open there with another tree, label or access.
    Occupied,
    /// The volume drains: no more requests.
    Draining,
    /// The actor stopped after a recording failure.
    Stopped,
}

/// One line of the control channel after [`Hello`]: a request, or the reply
/// that answers it, with the id that pairs them. The parent picks each
/// request's id, unique among its requests in flight; the host's reply
/// echoes it. Replies may come in any order (JSON-RPC's and LSP's ids).
///
/// ```text
/// {"id":7,"body":{"op":"close","at":"/m"}}
/// {"id":7,"body":"closed"}
/// ```
///
/// Protocol v2 ([`HOST_PROTOCOL`]); wire lines pinned by this module's tests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Envelope<T> {
    pub id: u64,
    pub body: T,
}

/// What a request names, and what a view holds while its close is pending:
/// a request waits only while a pending close holds a resource it names
/// (a lock manager's held resources). Never on the wire: both ends compute
/// it from the request and the views they know.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Resources {
    /// The mount point: an open's `at`, a close's `at`.
    pub mount: Option<PathBuf>,
    /// The client: an open's label.
    pub client: Option<ClientLabel>,
    /// The keyed fork: an open of a fork view, `fork_create`, `fork_end`.
    pub fork: Option<ForkKey>,
}

impl Resources {
    /// What a view of `tree` at `at` for the client `label` holds: its
    /// mount, its client and, for a fork view, its fork. The one rule both
    /// an open's names and a closing view's holdings come from.
    pub fn of_view(at: &std::path::Path, label: &ClientLabel, tree: &TreeRef) -> Self {
        let _ = (at, label, tree);
        todo!("agentfs-ln6.2")
    }

    /// Whether `self` and `held` share a mount, a client or a fork.
    pub fn overlaps(&self, held: &Resources) -> bool {
        let _ = held;
        todo!("agentfs-ln6.2")
    }
}

impl Request {
    /// What this request names: an open, its view's resources
    /// ([`Resources::of_view`]); a close, its mount; `fork_create` and
    /// `fork_end`, their key; `reconcile`, nothing (a closing view is still
    /// live, so a reconcile never waits).
    pub fn names(&self) -> Resources {
        todo!("agentfs-ln6.2")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::name::Label;
    use crate::trees::Landing;

    fn key(text: &str) -> ForkKey {
        ForkKey::parse(text).unwrap()
    }

    /// Each value's exact line, and its round trip: the contract a parent
    /// codes against.
    fn pinned<T>(value: &T, line: &str)
    where
        T: Serialize + for<'de> Deserialize<'de> + PartialEq + std::fmt::Debug,
    {
        assert_eq!(serde_json::to_string(value).unwrap(), line);
        assert_eq!(&serde_json::from_str::<T>(line).unwrap(), value);
    }

    #[test]
    fn hello_is_pinned() {
        pinned(
            &Hello {
                v: HOST_PROTOCOL,
                volume: VolumeName::parse("vol-a").unwrap(),
                id: VolumeId(uuid::Uuid::from_u128(
                    0x0123_4567_89ab_4def_8123_4567_89ab_cdef,
                )),
            },
            r#"{"v":1,"volume":"vol-a","id":"01234567-89ab-4def-8123-456789abcdef"}"#,
        );
    }

    #[test]
    fn tree_refs_are_pinned() {
        pinned(&TreeRef::Root, r#""root""#);
        pinned(&TreeRef::Fork(key("k1")), r#"{"fork":"k1"}"#);
    }

    #[test]
    fn requests_are_pinned() {
        pinned(
            &Request::Open(ViewRequest {
                tree: TreeRef::Fork(key("k1")),
                at: PathBuf::from("/var/lib/kubelet/pods/u1/mount"),
                label: ClientLabel::parse("csi-a1").unwrap(),
                read_only: false,
                pod: PodContext {
                    namespace: Some(Label::parse("ns").unwrap()),
                    pod: Some(Label::parse("p1").unwrap()),
                    pod_uid: Some(Label::parse("u1").unwrap()),
                    node: None,
                },
            }),
            r#"{"op":"open","tree":{"fork":"k1"},"at":"/var/lib/kubelet/pods/u1/mount","label":"csi-a1","read_only":false,"pod":{"namespace":"ns","pod":"p1","pod_uid":"u1"}}"#,
        );
        pinned(
            &Request::Close {
                at: PathBuf::from("/m"),
            },
            r#"{"op":"close","at":"/m"}"#,
        );
        pinned(&Request::Reconcile, r#"{"op":"reconcile"}"#);
        pinned(
            &Request::ForkCreate {
                upstream: TreeRef::Root,
                key: key("k1"),
                settings: ForkSettings {
                    landing: Landing::Automatic,
                },
            },
            r#"{"op":"fork_create","upstream":"root","key":"k1","settings":{"landing":"Automatic"}}"#,
        );
        pinned(
            &Request::ForkEnd { key: key("k1") },
            r#"{"op":"fork_end","key":"k1"}"#,
        );
    }

    #[test]
    fn replies_are_pinned() {
        pinned(
            &Reply::Opened {
                tree: TreeId::Root,
                stacked: true,
            },
            r#"{"opened":{"tree":"root","stacked":true}}"#,
        );
        pinned(&Reply::Closed, r#""closed""#);
        pinned(
            &Reply::Reconciled { ended: 2 },
            r#"{"reconciled":{"ended":2}}"#,
        );
        pinned(
            &Reply::Forked {
                tree: TreeId::Fork(1),
            },
            r#"{"forked":{"tree":"fork/1"}}"#,
        );
        pinned(&Reply::Ended, r#""ended""#);
        pinned(
            &Reply::Refused(Refusal {
                kind: RefusalKind::KeyUnknown,
                message: "no fork under that key".into(),
            }),
            r#"{"refused":{"kind":"key_unknown","message":"no fork under that key"}}"#,
        );
    }

    #[test]
    fn refusal_kinds_are_pinned() {
        for (kind, text) in [
            (RefusalKind::ForkTaken, "fork_taken"),
            (RefusalKind::KeyUnknown, "key_unknown"),
            (RefusalKind::KeyEnded, "key_ended"),
            (RefusalKind::NestedNotBuilt, "nested_not_built"),
            (RefusalKind::ApprovalNotBuilt, "approval_not_built"),
            (RefusalKind::MountFailed, "mount_failed"),
            (RefusalKind::Stale, "stale"),
            (RefusalKind::Occupied, "occupied"),
            (RefusalKind::Draining, "draining"),
            (RefusalKind::Stopped, "stopped"),
        ] {
            pinned(&kind, &format!("\"{text}\""));
        }
    }

    /// An unknown op or a missing field is an error, never a default.
    #[test]
    fn malformed_lines_are_refused() {
        assert!(serde_json::from_str::<Request>(r#"{"op":"mount"}"#).is_err());
        assert!(serde_json::from_str::<Request>(r#"{"op":"close"}"#).is_err());
        assert!(serde_json::from_str::<Request>("not json").is_err());
    }
}
