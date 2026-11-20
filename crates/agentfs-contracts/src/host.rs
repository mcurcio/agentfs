//! The view host's control messages (docs/library/filesystem.md): what a
//! parent that runs `agentfs host` writes to the host's standard input and
//! reads back from it. Pure values, no I/O.
//!
//! The wire is one JSON object per line (`'\n'`). The host writes [`Hello`]
//! once it serves. After it, the parent writes each [`Request`] in an
//! [`Envelope`] with an id of its choosing, unique among its requests in
//! flight, and need not wait for a reply before the next; the host answers
//! each with one [`Reply`] in an envelope that echoes the id, in any order
//! (JSON-RPC's and LSP's ids). A request waits at the host only while a
//! `close` that has not ended holds a resource it names ([`Resources`]), or
//! behind an earlier waiting request that names one of them; every other
//! request is answered at once. A duplicate id in flight is a malformed
//! line.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::name::{ClientLabel, ForkKey, VolumeName};
use crate::pod::PodContext;
use crate::tree::TreeId;
use crate::trees::ForkSettings;
use crate::volume::VolumeId;

/// The protocol's version, in [`Hello::v`].
pub const HOST_PROTOCOL: u32 = 2;

/// The host's first line: written once the volume is staged, its domains
/// restored, its view host started and its not-built settings logged; before
/// any request is read. Unknown fields are allowed, so a parent reads a
/// newer host's `v` and refuses it by version rather than as a malformed
/// line.
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
#[serde(deny_unknown_fields)]
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

/// What the parent asks the host, one per line. An unknown field is an
/// error, never ignored (but for `reconcile`, which has none to check).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
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
    pub fn of_view(at: &Path, label: &ClientLabel, tree: &TreeRef) -> Self {
        Self {
            mount: Some(at.to_path_buf()),
            client: Some(label.clone()),
            fork: match tree {
                TreeRef::Root => None,
                TreeRef::Fork(key) => Some(key.clone()),
            },
        }
    }

    /// Whether `self` and `held` share a mount, a client or a fork.
    pub fn overlaps(&self, held: &Resources) -> bool {
        fn same<T: PartialEq>(a: Option<&T>, b: Option<&T>) -> bool {
            a.is_some() && a == b
        }
        same(self.mount.as_ref(), held.mount.as_ref())
            || same(self.client.as_ref(), held.client.as_ref())
            || same(self.fork.as_ref(), held.fork.as_ref())
    }
}

impl Request {
    /// What this request names: an open, its view's resources
    /// ([`Resources::of_view`]); a close, its mount; `fork_create` and
    /// `fork_end`, their key; `reconcile`, nothing (a closing view is still
    /// live, so a reconcile never waits on a close). A host still answers
    /// a reconcile only after every open that came before it: it ends the
    /// clients no live view uses, and a waiting open is about to bind one.
    pub fn names(&self) -> Resources {
        match self {
            Self::Open(view) => Resources::of_view(&view.at, &view.label, &view.tree),
            Self::Close { at } => Resources {
                mount: Some(at.clone()),
                ..Resources::default()
            },
            Self::ForkCreate { key, .. } | Self::ForkEnd { key } => Resources {
                fork: Some(key.clone()),
                ..Resources::default()
            },
            Self::Reconcile => Resources::default(),
        }
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
            r#"{"v":2,"volume":"vol-a","id":"01234567-89ab-4def-8123-456789abcdef"}"#,
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

    /// An unknown op or a missing field is an error, never a default; so
    /// is an unknown field of a request or of an open's view. (serde reads
    /// `reconcile`, a variant with no fields, without looking at the rest.)
    #[test]
    fn malformed_lines_are_refused() {
        for line in [
            r#"{"op":"mount"}"#,
            r#"{"op":"close"}"#,
            "not json",
            r#"{"op":"close","at":"/m","x":1}"#,
            r#"{"op":"open","tree":"root","at":"/m","label":"l","read_only":false,"pod":{},"x":1}"#,
        ] {
            assert!(serde_json::from_str::<Request>(line).is_err(), "{line}");
        }
        let open =
            r#"{"op":"open","tree":"root","at":"/m","label":"l","read_only":false,"pod":{}}"#;
        assert!(serde_json::from_str::<Request>(open).is_ok());
    }

    /// A newer host's hello is read, extra fields and all, so a parent
    /// refuses its `v` by version.
    #[test]
    fn a_hello_with_more_fields_is_read() {
        let hello: Hello = serde_json::from_str(
            r#"{"v":3,"volume":"t","id":"01234567-89ab-4def-8123-456789abcdef","more":true}"#,
        )
        .unwrap();
        assert_eq!(hello.v, 3);
    }

    /// Protocol v2's envelope: a request and its reply, with the same id.
    #[test]
    fn envelopes_are_pinned() {
        pinned(
            &Envelope {
                id: 7,
                body: Request::Close {
                    at: PathBuf::from("/m"),
                },
            },
            r#"{"id":7,"body":{"op":"close","at":"/m"}}"#,
        );
        pinned(
            &Envelope {
                id: 7,
                body: Reply::Closed,
            },
            r#"{"id":7,"body":"closed"}"#,
        );
    }

    /// An unknown envelope field and a missing id are errors.
    #[test]
    fn malformed_envelopes_are_refused() {
        for line in [
            r#"{"id":7,"body":{"op":"reconcile"},"x":1}"#,
            r#"{"body":{"op":"reconcile"}}"#,
            r#"{"id":7}"#,
            r#"{"op":"reconcile"}"#,
        ] {
            assert!(
                serde_json::from_str::<Envelope<Request>>(line).is_err(),
                "{line}"
            );
        }
        let reply = r#"{"id":1,"body":"closed","x":0}"#;
        assert!(serde_json::from_str::<Envelope<Reply>>(reply).is_err());
    }

    fn label(text: &str) -> ClientLabel {
        ClientLabel::parse(text).unwrap()
    }

    fn view(tree: TreeRef, at: &str, l: &str) -> Request {
        Request::Open(ViewRequest {
            tree,
            at: PathBuf::from(at),
            label: label(l),
            read_only: false,
            pod: PodContext::default(),
        })
    }

    /// What each request names.
    #[test]
    fn each_request_names_its_resources() {
        assert_eq!(
            view(TreeRef::Root, "/a", "la").names(),
            Resources {
                mount: Some("/a".into()),
                client: Some(label("la")),
                fork: None,
            }
        );
        assert_eq!(
            view(TreeRef::Fork(key("k")), "/b", "lb").names(),
            Resources {
                mount: Some("/b".into()),
                client: Some(label("lb")),
                fork: Some(key("k")),
            },
            "a fork open names its key"
        );
        assert_eq!(
            Request::Close { at: "/a".into() }.names(),
            Resources {
                mount: Some("/a".into()),
                ..Resources::default()
            }
        );
        let fork_only = Resources {
            fork: Some(key("k")),
            ..Resources::default()
        };
        assert_eq!(
            Request::ForkCreate {
                upstream: TreeRef::Root,
                key: key("k"),
                settings: ForkSettings {
                    landing: Landing::Automatic,
                },
            }
            .names(),
            fork_only
        );
        assert_eq!(Request::ForkEnd { key: key("k") }.names(), fork_only);
        assert_eq!(
            Request::Reconcile.names(),
            Resources::default(),
            "reconcile names nothing"
        );
    }

    /// Two sets overlap only on an equal named resource; nothing overlaps
    /// nothing.
    #[test]
    fn overlap_is_an_equal_named_resource() {
        let held = Resources::of_view(Path::new("/a"), &label("la"), &TreeRef::Fork(key("k")));
        for (request, overlaps) in [
            (view(TreeRef::Root, "/a", "other"), true),
            (view(TreeRef::Root, "/c", "la"), true),
            (view(TreeRef::Fork(key("k")), "/c", "lc"), true),
            (view(TreeRef::Fork(key("j")), "/c", "lc"), false),
            (view(TreeRef::Root, "/c", "lc"), false),
            (Request::Close { at: "/a".into() }, true),
            (Request::Close { at: "/b".into() }, false),
            (Request::ForkEnd { key: key("k") }, true),
            (Request::ForkEnd { key: key("j") }, false),
            (Request::Reconcile, false),
        ] {
            assert_eq!(request.names().overlaps(&held), overlaps, "{request:?}");
        }
        assert!(!Resources::default().overlaps(&Resources::default()));
        let root = Resources::of_view(Path::new("/r"), &label("lr"), &TreeRef::Root);
        assert!(
            !Request::ForkEnd { key: key("k") }.names().overlaps(&root),
            "an absent fork is no resource"
        );
    }
}
