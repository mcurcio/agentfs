//! The one place the history service's wire messages meet agentfs types
//! (design §14).
//!
//! [`pb`] is `agentfs.history.v2`, generated at build time from
//! `proto/agentfs/history/v2/history.proto`. Every conversion to the wire is
//! total (`From<&T>`); every conversion from it validates (`TryFrom`, with
//! [`WireError`]): a path that is not a [`RepoPath`], an enum value this
//! build does not know (`*_UNSPECIFIED` included) and a missing required
//! message are refused, never defaulted.
//!
//! Every match over an agentfs or wire enum here lists each variant, with
//! no wildcard: a new variant does not compile until it has a wire form.

use super::{Answer, Call, FrontInfo, PathStamp};
use crate::checks::Refusal;
use crate::lfs::Store;
use crate::nested::{NestedKind, View};
use crate::paths::RepoPath;
use crate::pending::PendingId;
use crate::record::{
    Caller, Change, Draft, Operation, OperationKind, Origin, PatchAuthor, Trigger,
};
use crate::rules::{RuleSource, SkipReason};
use crate::stamps::FreezeStamp;
use crate::volume::Skipped;

/// `agentfs.history.v2`: the messages, the `History` client
/// ([`pb::history_client`]) and server ([`pb::history_server`]).
#[allow(
    clippy::all,
    clippy::pedantic,
    missing_docs,
    rustdoc::all,
    unreachable_pub
)]
pub mod pb {
    tonic::include_proto!("agentfs.history.v2");
}

use pb::{front_answer, front_call};

/// Why a wire message does not convert.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WireError {
    #[error("{field} is missing")]
    Missing { field: &'static str },
    /// Includes `*_UNSPECIFIED`.
    #[error("{field}: {value} is not a known value")]
    UnknownEnum { field: &'static str, value: i32 },
    /// A bad [`RepoPath`], a slot over `u16::MAX`, a count over `usize`.
    #[error("{field}: {message}")]
    Invalid {
        field: &'static str,
        message: String,
    },
}

impl From<WireError> for tonic::Status {
    fn from(e: WireError) -> Self {
        tonic::Status::invalid_argument(e.to_string())
    }
}

fn required<T>(field: &'static str, value: Option<T>) -> Result<T, WireError> {
    value.ok_or(WireError::Missing { field })
}

fn repo_path(field: &'static str, path: String) -> Result<RepoPath, WireError> {
    RepoPath::new(path).map_err(|e| WireError::Invalid {
        field,
        message: e.to_string(),
    })
}

/// Each of `paths` as a [`RepoPath`]; the first that is not one fails.
pub fn repo_paths(field: &'static str, paths: Vec<String>) -> Result<Vec<RepoPath>, WireError> {
    paths.into_iter().map(|p| repo_path(field, p)).collect()
}

fn strings(paths: &[RepoPath]) -> Vec<String> {
    paths.iter().map(|p| p.as_str().to_owned()).collect()
}

fn count(field: &'static str, n: u64) -> Result<usize, WireError> {
    usize::try_from(n).map_err(|e| WireError::Invalid {
        field,
        message: e.to_string(),
    })
}

// ---- who a front is ----

impl From<&Origin> for pb::Origin {
    fn from(o: &Origin) -> Self {
        Self {
            namespace: o.namespace.clone(),
            pod: o.pod.clone(),
            pod_uid: o.pod_uid.clone(),
            node: o.node.clone(),
            replica: o.replica.clone(),
        }
    }
}

impl TryFrom<pb::Origin> for Origin {
    type Error = WireError;

    fn try_from(o: pb::Origin) -> Result<Self, WireError> {
        Ok(Self {
            namespace: o.namespace,
            pod: o.pod,
            pod_uid: o.pod_uid,
            node: o.node,
            replica: o.replica,
        })
    }
}

impl From<&View> for pb::View {
    fn from(v: &View) -> Self {
        Self {
            at: v.at.clone(),
            root: v.root.clone(),
        }
    }
}

impl TryFrom<pb::View> for View {
    type Error = WireError;

    fn try_from(v: pb::View) -> Result<Self, WireError> {
        Ok(Self {
            at: v.at,
            root: v.root,
        })
    }
}

impl From<&FrontInfo> for pb::FrontInfo {
    fn from(f: &FrontInfo) -> Self {
        Self {
            origin: Some((&f.origin).into()),
            slot: u32::from(f.slot),
            instance: f.instance,
            pid: f.pid,
            version: f.version.clone(),
            views: f.views.iter().map(Into::into).collect(),
        }
    }
}

impl TryFrom<pb::FrontInfo> for FrontInfo {
    type Error = WireError;

    fn try_from(f: pb::FrontInfo) -> Result<Self, WireError> {
        Ok(Self {
            origin: required("front.origin", f.origin)?.try_into()?,
            slot: u16::try_from(f.slot).map_err(|e| WireError::Invalid {
                field: "front.slot",
                message: format!("{}: {e}", f.slot),
            })?,
            instance: f.instance,
            pid: f.pid,
            version: f.version,
            views: f
                .views
                .into_iter()
                .map(View::try_from)
                .collect::<Result<_, _>>()?,
        })
    }
}

// ---- records ----

impl From<&FreezeStamp> for pb::FreezeStamp {
    fn from(s: &FreezeStamp) -> Self {
        Self {
            dev: s.dev,
            ino: s.ino,
            size: s.size,
            mtime_ns: s.mtime_ns,
            ctime_ns: s.ctime_ns,
        }
    }
}

impl From<pb::FreezeStamp> for FreezeStamp {
    fn from(s: pb::FreezeStamp) -> Self {
        Self {
            dev: s.dev,
            ino: s.ino,
            size: s.size,
            mtime_ns: s.mtime_ns,
            ctime_ns: s.ctime_ns,
        }
    }
}

impl From<&PathStamp> for pb::PathStamp {
    fn from(s: &PathStamp) -> Self {
        Self {
            path: s.path.as_str().to_owned(),
            stamp: Some((&s.stamp).into()),
        }
    }
}

impl TryFrom<pb::PathStamp> for PathStamp {
    type Error = WireError;

    fn try_from(s: pb::PathStamp) -> Result<Self, WireError> {
        Ok(Self {
            path: repo_path("stamp.path", s.path)?,
            stamp: required("stamp.stamp", s.stamp)?.into(),
        })
    }
}

impl From<&Caller> for pb::Caller {
    fn from(c: &Caller) -> Self {
        Self {
            uid: c.uid,
            gid: c.gid,
            pid: c.pid,
        }
    }
}

impl From<pb::Caller> for Caller {
    fn from(c: pb::Caller) -> Self {
        Self {
            uid: c.uid,
            gid: c.gid,
            pid: c.pid,
        }
    }
}

fn trigger_to_wire(t: Trigger) -> pb::Trigger {
    match t {
        Trigger::Close => pb::Trigger::Close,
        Trigger::Rename => pb::Trigger::Rename,
        Trigger::Unlink => pb::Trigger::Unlink,
        Trigger::Rmdir => pb::Trigger::Rmdir,
        Trigger::Link => pb::Trigger::Link,
        Trigger::Symlink => pb::Trigger::Symlink,
        Trigger::Setattr => pb::Trigger::Setattr,
        Trigger::Fsync => pb::Trigger::Fsync,
        Trigger::Release => pb::Trigger::Release,
        Trigger::Sweep => pb::Trigger::Sweep,
        Trigger::Interval => pb::Trigger::Interval,
        Trigger::Restore => pb::Trigger::Restore,
        Trigger::Patch => pb::Trigger::Patch,
    }
}

fn trigger_from_wire(value: i32) -> Result<Trigger, WireError> {
    let unknown = WireError::UnknownEnum {
        field: "draft.trigger",
        value,
    };
    Ok(
        match pb::Trigger::try_from(value).map_err(|_| unknown.clone())? {
            pb::Trigger::Unspecified => return Err(unknown),
            pb::Trigger::Close => Trigger::Close,
            pb::Trigger::Rename => Trigger::Rename,
            pb::Trigger::Unlink => Trigger::Unlink,
            pb::Trigger::Rmdir => Trigger::Rmdir,
            pb::Trigger::Link => Trigger::Link,
            pb::Trigger::Symlink => Trigger::Symlink,
            pb::Trigger::Setattr => Trigger::Setattr,
            pb::Trigger::Fsync => Trigger::Fsync,
            pb::Trigger::Release => Trigger::Release,
            pb::Trigger::Sweep => Trigger::Sweep,
            pb::Trigger::Interval => Trigger::Interval,
            pb::Trigger::Restore => Trigger::Restore,
            pb::Trigger::Patch => Trigger::Patch,
        },
    )
}

fn store_to_wire(s: Store) -> pb::Store {
    match s {
        Store::Blob => pb::Store::Blob,
        Store::Lfs => pb::Store::Lfs,
    }
}

fn store_from_wire(value: i32) -> Result<Store, WireError> {
    let unknown = WireError::UnknownEnum {
        field: "write.store",
        value,
    };
    Ok(
        match pb::Store::try_from(value).map_err(|_| unknown.clone())? {
            pb::Store::Unspecified => return Err(unknown),
            pb::Store::Blob => Store::Blob,
            pb::Store::Lfs => Store::Lfs,
        },
    )
}

fn kind_to_wire(k: OperationKind) -> pb::OperationKind {
    match k {
        OperationKind::Restore => pb::OperationKind::Restore,
        OperationKind::Revert => pb::OperationKind::Revert,
        OperationKind::Patch => pb::OperationKind::Patch,
    }
}

fn kind_from_wire(value: i32) -> Result<OperationKind, WireError> {
    let unknown = WireError::UnknownEnum {
        field: "operation.kind",
        value,
    };
    Ok(
        match pb::OperationKind::try_from(value).map_err(|_| unknown.clone())? {
            pb::OperationKind::Unspecified => return Err(unknown),
            pb::OperationKind::Restore => OperationKind::Restore,
            pb::OperationKind::Revert => OperationKind::Revert,
            pb::OperationKind::Patch => OperationKind::Patch,
        },
    )
}

fn nested_to_wire(k: NestedKind) -> pb::NestedKind {
    match k {
        NestedKind::GitDir => pb::NestedKind::GitDir,
        NestedKind::GitFile => pb::NestedKind::GitFile,
        NestedKind::GitSymlink => pb::NestedKind::GitSymlink,
        NestedKind::Bare => pb::NestedKind::Bare,
    }
}

fn nested_from_wire(value: i32) -> Result<NestedKind, WireError> {
    let unknown = WireError::UnknownEnum {
        field: "nested.kind",
        value,
    };
    Ok(
        match pb::NestedKind::try_from(value).map_err(|_| unknown.clone())? {
            pb::NestedKind::Unspecified => return Err(unknown),
            pb::NestedKind::GitDir => NestedKind::GitDir,
            pb::NestedKind::GitFile => NestedKind::GitFile,
            pb::NestedKind::GitSymlink => NestedKind::GitSymlink,
            pb::NestedKind::Bare => NestedKind::Bare,
        },
    )
}

impl From<&Change> for pb::Change {
    fn from(c: &Change) -> Self {
        use pb::change::Op;
        let op = match c {
            Change::Write {
                path,
                pending,
                executable,
                store,
            } => Op::Write(pb::Write {
                path: path.as_str().to_owned(),
                pending: pending.0,
                executable: *executable,
                store: store_to_wire(*store).into(),
            }),
            Change::Symlink { path, target } => Op::Symlink(pb::Symlink {
                path: path.as_str().to_owned(),
                target: target.clone(),
            }),
            Change::Remove { path } => Op::Remove(pb::Remove {
                path: path.as_str().to_owned(),
            }),
            Change::Move { from, to } => Op::DirMove(pb::DirMove {
                from: from.as_str().to_owned(),
                to: to.as_str().to_owned(),
            }),
        };
        Self { op: Some(op) }
    }
}

impl TryFrom<pb::Change> for Change {
    type Error = WireError;

    fn try_from(c: pb::Change) -> Result<Self, WireError> {
        use pb::change::Op;
        Ok(match required("change.op", c.op)? {
            Op::Write(w) => Change::Write {
                path: repo_path("write.path", w.path)?,
                pending: PendingId(w.pending),
                executable: w.executable,
                store: store_from_wire(w.store)?,
            },
            Op::Symlink(s) => Change::Symlink {
                path: repo_path("symlink.path", s.path)?,
                target: s.target,
            },
            Op::Remove(r) => Change::Remove {
                path: repo_path("remove.path", r.path)?,
            },
            Op::DirMove(m) => Change::Move {
                from: repo_path("dir_move.from", m.from)?,
                to: repo_path("dir_move.to", m.to)?,
            },
        })
    }
}

impl From<&PatchAuthor> for pb::PatchAuthor {
    fn from(a: &PatchAuthor) -> Self {
        Self {
            name: a.name.clone(),
            email: a.email.clone(),
            time_s: a.time_s,
            offset_min: a.offset_min,
        }
    }
}

impl From<pb::PatchAuthor> for PatchAuthor {
    fn from(a: pb::PatchAuthor) -> Self {
        Self {
            name: a.name,
            email: a.email,
            time_s: a.time_s,
            offset_min: a.offset_min,
        }
    }
}

impl From<&Operation> for pb::Operation {
    fn from(o: &Operation) -> Self {
        Self {
            id: o.id,
            kind: kind_to_wire(o.kind).into(),
            source: o.source.clone(),
            summary: o.summary.clone(),
            caller: Some((&o.caller).into()),
            inbox: o.inbox,
            author: o.author.as_ref().map(Into::into),
        }
    }
}

impl TryFrom<pb::Operation> for Operation {
    type Error = WireError;

    fn try_from(o: pb::Operation) -> Result<Self, WireError> {
        Ok(Self {
            id: o.id,
            kind: kind_from_wire(o.kind)?,
            source: o.source,
            summary: o.summary,
            caller: required("operation.caller", o.caller)?.into(),
            inbox: o.inbox,
            author: o.author.map(Into::into),
        })
    }
}

impl From<&Draft> for pb::Draft {
    fn from(d: &Draft) -> Self {
        Self {
            trigger: trigger_to_wire(d.trigger).into(),
            caller: Some((&d.caller).into()),
            time_ms: d.time_ms,
            changes: d.changes.iter().map(Into::into).collect(),
            operation: d.operation.as_ref().map(Into::into),
            atomic: d.atomic,
            origin: d.origin.as_ref().map(Into::into),
        }
    }
}

impl TryFrom<pb::Draft> for Draft {
    type Error = WireError;

    fn try_from(d: pb::Draft) -> Result<Self, WireError> {
        Ok(Self {
            trigger: trigger_from_wire(d.trigger)?,
            caller: required("draft.caller", d.caller)?.into(),
            time_ms: d.time_ms,
            changes: d
                .changes
                .into_iter()
                .map(Change::try_from)
                .collect::<Result<_, _>>()?,
            operation: d.operation.map(Operation::try_from).transpose()?,
            atomic: d.atomic,
            origin: d.origin.map(Origin::try_from).transpose()?,
        })
    }
}

// ---- what the service skipped ----

impl From<&Skipped> for pb::Skip {
    /// `Recorder::sequence` only skips for the rules and group refusals.
    /// Any other refusal still reaches the front, as a group member
    /// skipped with the refusal's message: that mapping does not
    /// round-trip.
    fn from(s: &Skipped) -> Self {
        use pb::skip::Reason;
        let kind = |k: pb::SkipKind| Reason::Kind(k.into());
        let reason = match &s.reason {
            SkipReason::NeverRecord => kind(pb::SkipKind::NeverRecord),
            SkipReason::Untracked => kind(pb::SkipKind::Untracked),
            SkipReason::SharedMemory => kind(pb::SkipKind::SharedMemory),
            SkipReason::Gone => kind(pb::SkipKind::Gone),
            SkipReason::Superseded => kind(pb::SkipKind::Superseded),
            SkipReason::NfsSillyRename => kind(pb::SkipKind::NfsSillyRename),
            SkipReason::Ignored { source, pattern } => Reason::Ignored(pb::Ignored {
                file: match source {
                    RuleSource::OperatorExclude => None,
                    RuleSource::TreeIgnore(file) => Some(file.as_str().to_owned()),
                },
                pattern: pattern.clone(),
            }),
            SkipReason::Nested { repo, kind } => Reason::Nested(pb::Nested {
                repo: repo.as_str().to_owned(),
                kind: nested_to_wire(*kind).into(),
            }),
            SkipReason::GroupRefused { member } => {
                Reason::GroupRefusedMember(member.as_str().to_owned())
            }
            SkipReason::Refused(Refusal::GroupMemberSkipped { reason }) => {
                Reason::GroupMemberSkipped(reason.clone())
            }
            SkipReason::Refused(other) => Reason::GroupMemberSkipped(other.to_string()),
        };
        Self {
            path: s.path.as_str().to_owned(),
            reason: Some(reason),
        }
    }
}

impl TryFrom<pb::Skip> for Skipped {
    type Error = WireError;

    fn try_from(s: pb::Skip) -> Result<Self, WireError> {
        use pb::skip::Reason;
        let reason = match required("skip.reason", s.reason)? {
            Reason::Kind(value) => {
                let unknown = WireError::UnknownEnum {
                    field: "skip.kind",
                    value,
                };
                match pb::SkipKind::try_from(value).map_err(|_| unknown.clone())? {
                    pb::SkipKind::Unspecified => return Err(unknown),
                    pb::SkipKind::NeverRecord => SkipReason::NeverRecord,
                    pb::SkipKind::Untracked => SkipReason::Untracked,
                    pb::SkipKind::SharedMemory => SkipReason::SharedMemory,
                    pb::SkipKind::Gone => SkipReason::Gone,
                    pb::SkipKind::Superseded => SkipReason::Superseded,
                    pb::SkipKind::NfsSillyRename => SkipReason::NfsSillyRename,
                }
            }
            Reason::Ignored(i) => SkipReason::Ignored {
                source: match i.file {
                    None => RuleSource::OperatorExclude,
                    Some(file) => RuleSource::TreeIgnore(repo_path("ignored.file", file)?),
                },
                pattern: i.pattern,
            },
            Reason::Nested(n) => SkipReason::Nested {
                repo: repo_path("nested.repo", n.repo)?,
                kind: nested_from_wire(n.kind)?,
            },
            Reason::GroupRefusedMember(member) => SkipReason::GroupRefused {
                member: repo_path("skip.group_refused_member", member)?,
            },
            Reason::GroupMemberSkipped(reason) => {
                SkipReason::Refused(Refusal::GroupMemberSkipped { reason })
            }
        };
        Ok(Self {
            path: repo_path("skip.path", s.path)?,
            reason,
        })
    }
}

// ---- the reversed channel ----

impl From<&Call> for front_call::Call {
    fn from(c: &Call) -> Self {
        match c {
            Call::Settle => Self::Settle(pb::Settle {}),
            Call::OpenForWriting { paths } => Self::OpenForWriting(pb::Paths {
                paths: strings(paths),
            }),
            Call::Invalidate { paths } => Self::Invalidate(pb::Paths {
                paths: strings(paths),
            }),
            Call::Ping => Self::Ping(pb::Ping {}),
        }
    }
}

impl TryFrom<front_call::Call> for Call {
    type Error = WireError;

    fn try_from(c: front_call::Call) -> Result<Self, WireError> {
        Ok(match c {
            front_call::Call::Settle(pb::Settle {}) => Call::Settle,
            front_call::Call::OpenForWriting(p) => Call::OpenForWriting {
                paths: repo_paths("open_for_writing.paths", p.paths)?,
            },
            front_call::Call::Invalidate(p) => Call::Invalidate {
                paths: repo_paths("invalidate.paths", p.paths)?,
            },
            front_call::Call::Ping(pb::Ping {}) => Call::Ping,
        })
    }
}

impl From<&Answer> for front_answer::Answer {
    fn from(a: &Answer) -> Self {
        match a {
            Answer::Settled { recorded, failed } => Self::Settled(pb::Settled {
                recorded: *recorded as u64,
                failed: *failed as u64,
            }),
            Answer::Open { open } => Self::Open(pb::OpenPaths { open: open.clone() }),
            Answer::Invalidated => Self::Invalidated(pb::Invalidated {}),
            Answer::Pong => Self::Pong(pb::Pong {}),
            Answer::Error { message } => Self::Error(message.clone()),
        }
    }
}

impl TryFrom<front_answer::Answer> for Answer {
    type Error = WireError;

    fn try_from(a: front_answer::Answer) -> Result<Self, WireError> {
        Ok(match a {
            front_answer::Answer::Settled(s) => Answer::Settled {
                recorded: count("settled.recorded", s.recorded)?,
                failed: count("settled.failed", s.failed)?,
            },
            front_answer::Answer::Open(o) => Answer::Open { open: o.open },
            front_answer::Answer::Invalidated(pb::Invalidated {}) => Answer::Invalidated,
            front_answer::Answer::Pong(pb::Pong {}) => Answer::Pong,
            front_answer::Answer::Error(message) => Answer::Error { message },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(p: &str) -> RepoPath {
        RepoPath::new(p).unwrap()
    }

    fn origin(pod: &str) -> Origin {
        Origin {
            namespace: "ns".into(),
            pod: pod.into(),
            pod_uid: format!("{pod}-uid"),
            node: "n1".into(),
            replica: pod.into(),
        }
    }

    const CALLER: Caller = Caller {
        uid: 1,
        gid: 2,
        pid: 3,
    };

    fn draft(changes: Vec<Change>) -> Draft {
        Draft {
            trigger: Trigger::Close,
            caller: CALLER,
            time_ms: 9,
            changes,
            operation: None,
            atomic: false,
            origin: None,
        }
    }

    fn round_trip_draft(d: &Draft) -> Draft {
        Draft::try_from(pb::Draft::from(d)).unwrap()
    }

    #[test]
    fn every_trigger_round_trips() {
        let all = [
            Trigger::Close,
            Trigger::Rename,
            Trigger::Unlink,
            Trigger::Rmdir,
            Trigger::Link,
            Trigger::Symlink,
            Trigger::Setattr,
            Trigger::Fsync,
            Trigger::Release,
            Trigger::Sweep,
            Trigger::Interval,
            Trigger::Restore,
            Trigger::Patch,
        ];
        let mut seen = std::collections::HashSet::new();
        for trigger in all {
            let d = Draft {
                trigger,
                ..draft(Vec::new())
            };
            let wire = pb::Draft::from(&d);
            assert!(seen.insert(wire.trigger), "{trigger:?} shares a value");
            assert_eq!(round_trip_draft(&d), d);
        }
    }

    #[test]
    fn every_change_and_store_round_trips() {
        let changes = vec![
            Change::Write {
                path: path("a/b"),
                pending: PendingId::in_slot(3, 7),
                executable: true,
                store: Store::Blob,
            },
            Change::Write {
                path: path("big.bin"),
                pending: PendingId(42),
                executable: false,
                store: Store::Lfs,
            },
            Change::Symlink {
                path: path("l"),
                target: "../t".into(),
            },
            Change::Remove { path: path("gone") },
            Change::Move {
                from: path("d"),
                to: path("m"),
            },
        ];
        for change in &changes {
            assert_eq!(Change::try_from(pb::Change::from(change)).unwrap(), *change);
        }
        let d = draft(changes);
        assert_eq!(round_trip_draft(&d), d);
    }

    #[test]
    fn drafts_round_trip_with_and_without_operation_origin_and_author() {
        let plain = draft(vec![Change::Remove { path: path("x") }]);
        assert_eq!(round_trip_draft(&plain), plain);
        for kind in [
            OperationKind::Restore,
            OperationKind::Revert,
            OperationKind::Patch,
        ] {
            for author in [
                None,
                Some(PatchAuthor {
                    name: "A".into(),
                    email: "a@x".into(),
                    time_s: -5,
                    offset_min: -120,
                }),
            ] {
                for inbox in [None, Some(0), Some(9)] {
                    let d = Draft {
                        trigger: kind.trigger(),
                        operation: Some(Operation {
                            id: 4,
                            kind,
                            source: "abc".into(),
                            summary: "restore x from abc".into(),
                            caller: CALLER,
                            inbox,
                            author: author.clone(),
                        }),
                        atomic: true,
                        origin: Some(origin("pod-a")),
                        ..plain.clone()
                    };
                    assert_eq!(round_trip_draft(&d), d);
                }
            }
        }
        // An origin of empty strings is still an origin, not None.
        let empty = Draft {
            origin: Some(Origin {
                namespace: String::new(),
                pod: String::new(),
                pod_uid: String::new(),
                node: String::new(),
                replica: String::new(),
            }),
            ..plain
        };
        assert_eq!(round_trip_draft(&empty), empty);
    }

    #[test]
    fn every_skip_reason_round_trips() {
        let reasons = [
            SkipReason::NeverRecord,
            SkipReason::Ignored {
                source: RuleSource::OperatorExclude,
                pattern: "*.o".into(),
            },
            SkipReason::Ignored {
                source: RuleSource::TreeIgnore(path("d/.gitignore")),
                pattern: "x".into(),
            },
            SkipReason::Nested {
                repo: path("r"),
                kind: NestedKind::GitDir,
            },
            SkipReason::Nested {
                repo: path("r"),
                kind: NestedKind::GitFile,
            },
            SkipReason::Nested {
                repo: path("r"),
                kind: NestedKind::GitSymlink,
            },
            SkipReason::Nested {
                repo: path("r"),
                kind: NestedKind::Bare,
            },
            SkipReason::Untracked,
            SkipReason::SharedMemory,
            SkipReason::Gone,
            SkipReason::Superseded,
            SkipReason::NfsSillyRename,
            SkipReason::GroupRefused { member: path("m") },
            SkipReason::Refused(Refusal::GroupMemberSkipped { reason: "r".into() }),
        ];
        for reason in reasons {
            let skipped = Skipped {
                path: path("p/q"),
                reason,
            };
            assert_eq!(
                Skipped::try_from(pb::Skip::from(&skipped)).unwrap(),
                skipped
            );
        }
    }

    #[test]
    fn other_refusals_reach_the_front_as_a_group_member_skipped_and_do_not_round_trip() {
        let refusal = Refusal::PathTooLong { bytes: 5000 };
        let skipped = Skipped {
            path: path("p"),
            reason: SkipReason::Refused(refusal.clone()),
        };
        let back = Skipped::try_from(pb::Skip::from(&skipped)).unwrap();
        assert_ne!(back, skipped, "the mapping is lossy");
        assert_eq!(
            back.reason,
            SkipReason::Refused(Refusal::GroupMemberSkipped {
                reason: refusal.to_string()
            })
        );
    }

    #[test]
    fn front_info_and_stamps_round_trip() {
        let front = FrontInfo {
            origin: origin("pod-a"),
            slot: u16::MAX,
            instance: u64::MAX,
            pid: 7,
            version: "0.1.0".into(),
            views: vec![
                View {
                    at: "/work".into(),
                    root: String::new(),
                },
                View {
                    at: "/sub".into(),
                    root: "a/b".into(),
                },
            ],
        };
        assert_eq!(
            FrontInfo::try_from(pb::FrontInfo::from(&front)).unwrap(),
            front
        );
        let stamp = PathStamp {
            path: path("a/b"),
            stamp: FreezeStamp {
                dev: 1,
                ino: 2,
                size: 3,
                mtime_ns: -4,
                ctime_ns: 5,
            },
        };
        assert_eq!(
            PathStamp::try_from(pb::PathStamp::from(&stamp)).unwrap(),
            stamp
        );
    }

    #[test]
    fn every_call_and_answer_round_trips() {
        let calls = [
            Call::Settle,
            Call::OpenForWriting {
                paths: vec![path("a"), path("b/c")],
            },
            Call::Invalidate {
                paths: vec![path("a")],
            },
            Call::Ping,
        ];
        for call in calls {
            assert_eq!(Call::try_from(front_call::Call::from(&call)).unwrap(), call);
        }
        let answers = [
            Answer::Settled {
                recorded: 2,
                failed: 1,
            },
            Answer::Open {
                open: vec![true, false],
            },
            Answer::Invalidated,
            Answer::Pong,
            Answer::Error {
                message: "m".into(),
            },
        ];
        for answer in answers {
            assert_eq!(
                Answer::try_from(front_answer::Answer::from(&answer)).unwrap(),
                answer
            );
        }
    }

    #[test]
    fn unspecified_enums_and_missing_messages_are_refused() {
        let good = pb::Draft::from(&draft(vec![Change::Write {
            path: path("f"),
            pending: PendingId(1),
            executable: false,
            store: Store::Blob,
        }]));

        let bad = pb::Draft {
            trigger: pb::Trigger::Unspecified.into(),
            ..good.clone()
        };
        assert_eq!(
            Draft::try_from(bad).unwrap_err(),
            WireError::UnknownEnum {
                field: "draft.trigger",
                value: 0
            }
        );
        let bad = pb::Draft {
            trigger: 99,
            ..good.clone()
        };
        assert!(matches!(
            Draft::try_from(bad),
            Err(WireError::UnknownEnum { value: 99, .. })
        ));
        let bad = pb::Draft {
            caller: None,
            ..good.clone()
        };
        assert_eq!(
            Draft::try_from(bad).unwrap_err(),
            WireError::Missing {
                field: "draft.caller"
            }
        );
        let mut bad = good.clone();
        bad.changes[0] = pb::Change { op: None };
        assert_eq!(
            Draft::try_from(bad).unwrap_err(),
            WireError::Missing { field: "change.op" }
        );
        let mut bad = good.clone();
        if let Some(pb::change::Op::Write(w)) = &mut bad.changes[0].op {
            w.store = pb::Store::Unspecified.into();
        }
        assert!(matches!(
            Draft::try_from(bad),
            Err(WireError::UnknownEnum {
                field: "write.store",
                ..
            })
        ));
        let mut bad = good.clone();
        if let Some(pb::change::Op::Write(w)) = &mut bad.changes[0].op {
            w.path = "a/../../etc".into();
        }
        assert!(matches!(
            Draft::try_from(bad),
            Err(WireError::Invalid {
                field: "write.path",
                ..
            })
        ));

        let operation = pb::Operation {
            id: 1,
            kind: pb::OperationKind::Unspecified.into(),
            source: String::new(),
            summary: String::new(),
            caller: Some((&CALLER).into()),
            inbox: None,
            author: None,
        };
        assert!(matches!(
            Operation::try_from(operation.clone()),
            Err(WireError::UnknownEnum {
                field: "operation.kind",
                ..
            })
        ));
        let no_caller = pb::Operation {
            kind: pb::OperationKind::Revert.into(),
            caller: None,
            ..operation
        };
        assert_eq!(
            Operation::try_from(no_caller).unwrap_err(),
            WireError::Missing {
                field: "operation.caller"
            }
        );

        let skip = |reason| pb::Skip {
            path: "p".into(),
            reason,
        };
        assert_eq!(
            Skipped::try_from(skip(None)).unwrap_err(),
            WireError::Missing {
                field: "skip.reason"
            }
        );
        assert!(matches!(
            Skipped::try_from(skip(Some(pb::skip::Reason::Kind(0)))),
            Err(WireError::UnknownEnum {
                field: "skip.kind",
                ..
            })
        ));
        assert!(matches!(
            Skipped::try_from(skip(Some(pb::skip::Reason::Nested(pb::Nested {
                repo: "r".into(),
                kind: pb::NestedKind::Unspecified.into(),
            })))),
            Err(WireError::UnknownEnum {
                field: "nested.kind",
                ..
            })
        ));

        let front = pb::FrontInfo::from(&FrontInfo {
            origin: origin("a"),
            slot: 1,
            instance: 1,
            pid: 1,
            version: String::new(),
            views: Vec::new(),
        });
        assert_eq!(
            FrontInfo::try_from(pb::FrontInfo {
                origin: None,
                ..front.clone()
            })
            .unwrap_err(),
            WireError::Missing {
                field: "front.origin"
            }
        );
        assert!(matches!(
            FrontInfo::try_from(pb::FrontInfo {
                slot: u32::from(u16::MAX) + 1,
                ..front
            }),
            Err(WireError::Invalid {
                field: "front.slot",
                ..
            })
        ));
        assert_eq!(
            PathStamp::try_from(pb::PathStamp {
                path: "a".into(),
                stamp: None
            })
            .unwrap_err(),
            WireError::Missing {
                field: "stamp.stamp"
            }
        );
        assert!(matches!(
            repo_paths("paths", vec!["ok".into(), "/abs".into()]),
            Err(WireError::Invalid { field: "paths", .. })
        ));
        let status = tonic::Status::from(WireError::Missing { field: "f" });
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
        assert_eq!(status.message(), "f is missing");
    }
}
