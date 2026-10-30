//! Finishing and publishing a fork (design §19.1, "Forked volumes"): the
//! finisher is the one owner of `finished` and `published`.
//!
//! [`Finisher::finish`] runs once, with the state-area lock held. It
//! commits what is journaled and records what repairs left, then writes
//! the change (one commit whose parent is base and whose tree is the
//! fork's final tree; none when that tree is base's) and the finish
//! commit: the last commit of the fork's history, the single home of
//! base, change and fork remote, after which the fork takes no changes.
//! [`Finisher::publish`] pushes the history, the change and the finish
//! commit to the fork remote, with §11.2 backoff, until the remote holds
//! the change and finish refs at exactly their ids, and only then writes
//! the published mark. A fork of a local-only source publishes into the
//! source's own history instead ([`super::receive`], F8 S3), through its
//! [`Receiver`], and is published once the source holds the same refs. A remote that refuses for good (see
//! [`refused_reason`], the single decider) is recorded as refused and not
//! retried until the operator clears it ([`Finisher::retry_refused`]).
//! [`abandon`] records the operator's discard, after which nothing agentfs
//! runs pushes the fork. [`status`] reads, without a lock or credentials,
//! what DeleteVolume asks: whether the fork is published or abandoned.
//! An unlinked fork (design §11.5, [`ForkLink::Unlinked`]) delivers
//! nothing: it never finishes ([`FinishError::Unlinked`]) and has no
//! status, so DeleteVolume and the release scan treat it as any worktree.
//!
//! A fork of a fork ([`Settings::parent_fork`], bead agentfs-mia.7) rolls
//! its change up one level instead of publishing a change ref:
//! through the same [`RemotePusher`] (the fork remote's credentials), it
//! pushes its history, lists `refs/agentfs/<parent>/`, and queues the
//! change create-only as the parent's next inbox number
//! ([`PublishOutcome::Submitted`]), unless the parent already queued or
//! applied it (at most once) or the parent has finished, when it publishes
//! as a top-level fork and raises [`Event::ForkParentFinished`] before it
//! writes the published mark, which records the parent: a crash before
//! the mark leaves the fork finished, so the next publish, by whatever
//! runs it, raises the alarm again (bead agentfs-rrm). After its
//! push it lists the parent again, so a parent that finished in between is
//! seen. It then pushes its own finish ref, which its own forks read as
//! "finished". The parent's queue lands it like any patch.
//!
//! Every fork published to a fork remote, once its finish ref is held
//! there, lists its own inbox on that remote and raises
//! [`Event::ForkInboxNotCarried`] for each entry its finished history did
//! not land (design §12 "Forks of forks"): a fork's change rolled up into
//! it after its last queue run, which its own change does not carry. With
//! the child's listing after its push, one of the two always sees the
//! other: a change is never stranded unannounced.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use git2::{Oid, Repository};
use opentelemetry::KeyValue;
use serde::{Deserialize, Serialize};

use crate::config::{ForkRemote, Settings};
use crate::events::{Event, Events, PushFailureKind};
use crate::history::{HistoryError, finish_from_message};
use crate::identity::Context;
use crate::lock::StateLock;
use crate::patch::submit::{DEFAULT_ATTEMPTS, listed_as, next_seq};
use crate::patch::{applied_prefix, inbox_prefix, inbox_ref, seq_of_ref};
use crate::paths::RepoPath;
use crate::push::{
    CreateOnlyOutcome, CreateOnlyRef, PUSH_BACKOFF_CAP, PushOutcome, RemotePusher, backoff,
};
use crate::record::{Caller, Trigger, read_repairs};
use crate::telemetry::{Telemetry, spans};
use crate::volume::{self, Committer, Input, Recorder, StateDir, VolumeError};

use super::identity::{ForkIdentity, ForkLink, change_ref, finish_ref};
use super::receive::{ReceiveError, Receiver, ReceiverError, SOURCE_HISTORY, UnderLock};

/// `Agentfs-Trigger` of a finish commit.
pub const FINISH_TRIGGER: &str = "finish";
/// The fork's name, on the change and the finish commit.
pub const FORK_TRAILER: &str = "Agentfs-Fork";
/// The source's name, on the change and the finish commit.
pub const SOURCE_TRAILER: &str = "Agentfs-Source";
/// Base, on the change and the finish commit.
pub const BASE_TRAILER: &str = "Agentfs-Base";
/// The change, on the finish commit; absent when nothing changed.
pub const CHANGE_TRAILER: &str = "Agentfs-Change";
/// The fork remote's name, on the finish commit.
pub const FORK_REMOTE_TRAILER: &str = "Agentfs-Fork-Remote";
/// How many paths the finish could not record, on the finish commit.
pub const UNRECORDED_TRAILER: &str = "Agentfs-Unrecorded";

/// What a finish recorded: read back from the finish commit's last
/// paragraph, which only agentfs writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FinishRecord {
    pub fork: String,
    pub source: String,
    pub base: Oid,
    /// The finish commit: the last commit of the fork's history.
    pub finish: Oid,
    /// The change; `None` when the fork's final tree is base's.
    pub change: Option<Oid>,
    /// The fork remote, by name, as the fork's settings named it at
    /// finish, or [`SOURCE_HISTORY`] for a fork of a local-only source.
    /// "Published" is judged against it.
    pub remote: String,
    /// Paths the finish could not record, each alarmed.
    pub unrecorded: u64,
}

/// Where a fork is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForkState {
    /// Taking changes. Restarts of agentfs, the node plugin or the kubelet
    /// leave a fork here.
    Active,
    /// The finish commit is written; publishing is pending.
    Finished,
    /// The fork remote holds the change and finish refs at exactly their
    /// ids, as the finisher saw after pushing.
    Published,
    /// The fork remote refuses for good; waits for the operator
    /// (`agentfs fork finish --retry`, or [`abandon`]).
    Refused,
    /// The operator's recorded discard ([`abandon`]).
    Abandoned,
}

impl ForkState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Finished => "finished",
            Self::Published => "published",
            Self::Refused => "refused",
            Self::Abandoned => "abandoned",
        }
    }

    /// Whether DeleteVolume may apply `onDelete`: only once the fork is
    /// published or abandoned.
    pub fn may_delete(self) -> bool {
        matches!(self, Self::Published | Self::Abandoned)
    }
}

/// A fork as [`status`] reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForkStatus {
    pub identity: ForkIdentity,
    pub state: ForkState,
    /// The finish record, once the fork is finished.
    pub record: Option<FinishRecord>,
    /// `meta/fork.json`, when there is one.
    pub mark: Option<ForkMark>,
}

/// Why a publish cannot succeed without the operator ([`refused_reason`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefusedReason {
    /// A create-only ref on the fork remote holds another object.
    AnotherWriter,
    /// The remote's size limit refuses a single commit (§11.3).
    TooLarge,
    /// The remote refuses the refs or their content.
    Rejected,
    /// The fork's settings no longer define the remote the finish named.
    RemoteGone,
    /// A local-only source refuses what the fork brings: a ref that is
    /// the source's own, a base off the source's branch, or an LFS object
    /// that is not its name (`fork::receive`).
    Unsafe,
}

impl RefusedReason {
    /// Every reason, for what must cover each (the runbook, its hints).
    pub const ALL: [Self; 5] = [
        Self::AnotherWriter,
        Self::TooLarge,
        Self::Rejected,
        Self::RemoteGone,
        Self::Unsafe,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::AnotherWriter => "another_writer",
            Self::TooLarge => "too_large",
            Self::Rejected => "rejected",
            Self::RemoteGone => "remote_gone",
            Self::Unsafe => "unsafe",
        }
    }
}

/// What a publish attempt met, for [`refused_reason`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Met {
    /// The history push (the fork's branch) failed so.
    Branch(PushFailureKind),
    /// The create-only push of the change and finish refs failed so.
    CreateOnly(PushFailureKind),
    /// A create-only ref on the remote holds another object.
    AnotherWriter,
    /// The finish names a remote that is not the fork's fork remote now,
    /// or the local-only source whose history it names is gone.
    RemoteGone,
    /// The local-only source refused what the fork brings
    /// ([`RefusedReason::Unsafe`]).
    Unsafe,
}

/// The single decider of "permanent" (design §11.5): what makes a publish
/// refused, alarmed once and never retried by the node plugin's release
/// scan. Everything else (unreachable, auth, a moved branch, a busy
/// remote) is pending and retried with backoff (§11.3).
pub fn refused_reason(met: Met) -> Option<RefusedReason> {
    match met {
        Met::AnotherWriter => Some(RefusedReason::AnotherWriter),
        Met::RemoteGone => Some(RefusedReason::RemoteGone),
        Met::Unsafe => Some(RefusedReason::Unsafe),
        // One commit over agentfsMaxPushSize, or one LFS object over
        // agentfsMaxLfsObjectSize (§11.3): no retry can send it.
        Met::Branch(PushFailureKind::TooLarge) | Met::CreateOnly(PushFailureKind::TooLarge) => {
            Some(RefusedReason::TooLarge)
        }
        Met::CreateOnly(PushFailureKind::Refused) => Some(RefusedReason::Rejected),
        Met::Branch(_) | Met::CreateOnly(_) => None,
    }
}

/// How a publish went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublishOutcome {
    /// The fork remote holds both refs at exactly their ids; the
    /// published mark is written.
    Published,
    /// A fork of a fork: its change is queued in its parent's inbox as
    /// patch `inbox` on the fork remote (or was already queued or applied
    /// there); the published mark is written.
    Submitted { inbox: u64 },
    /// Not yet: the deadline passed while the last attempt failed in a way
    /// that is retried (§11.3).
    Pending {
        kind: PushFailureKind,
        message: String,
    },
    /// The remote refuses for good: alarmed, and not retried until the
    /// operator acts.
    Refused {
        reason: RefusedReason,
        message: String,
    },
}

/// The version of `meta/fork.json`.
pub const MARK_VERSION: u32 = 1;

/// `meta/fork.json` (design §2.2): the finisher's marks, written only
/// after the remote confirmed what they say, and the operator's discard.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForkMark {
    pub version: u32,
    pub published: Option<PublishedMark>,
    #[serde(default)]
    pub refused: Option<RefusedMark>,
    #[serde(default)]
    pub abandoned: Option<AbandonedMark>,
}

impl ForkMark {
    /// A mark with nothing recorded yet.
    pub fn empty() -> Self {
        Self {
            version: MARK_VERSION,
            published: None,
            refused: None,
            abandoned: None,
        }
    }
}

/// The fork remote refused the finish `finish` for good ([`refused_reason`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RefusedMark {
    pub finish: String,
    pub remote: String,
    pub reason: RefusedReason,
    pub message: String,
    /// When it was refused (milliseconds since the Unix epoch).
    pub at_ms: u64,
}

/// The operator's recorded discard ([`abandon`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AbandonedMark {
    pub reason: String,
    /// When (milliseconds since the Unix epoch).
    pub at_ms: u64,
    /// The operator's uid.
    pub uid: u32,
    /// [`ForkState::as_str`] when it was abandoned.
    pub state: String,
}

/// The fork remote held the change and finish refs at exactly these ids.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublishedMark {
    pub finish: String,
    pub change: Option<String>,
    pub remote: String,
    /// The parent's inbox number of a rolled-up change; `None` for a
    /// top-level publish. Optional in the file: marks without it read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inbox: Option<u64>,
    /// A fork of a fork whose parent had finished, so it published as a
    /// top-level fork; `None` otherwise. Optional in the file: marks
    /// without it read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_finished: Option<ParentFinishedMark>,
    /// When the remote confirmed it (milliseconds since the Unix epoch).
    pub at_ms: u64,
}

/// The parent fork had finished when its child published
/// ([`Event::ForkParentFinished`]), recorded with the published mark,
/// which is written only after the alarm was raised: a fork whose mark
/// says published has raised it, and one that crashed before the mark is
/// still finished, so its next publish raises it again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParentFinishedMark {
    /// The parent fork's name.
    pub parent: String,
}

#[derive(Debug, thiserror::Error)]
pub enum StatusError {
    #[error("the fork's published mark cannot be read: {0}")]
    Mark(std::io::Error),
    #[error(transparent)]
    History(#[from] HistoryError),
}

#[derive(Debug, thiserror::Error)]
pub enum AbandonError {
    #[error("this volume is not a fork, or is an unlinked one: it delivers nothing to discard")]
    NotAFork,
    #[error("fork {fork} is published; there is nothing to discard")]
    Published { fork: String },
    #[error("a reason is required")]
    EmptyReason,
    #[error(transparent)]
    Status(#[from] StatusError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

#[derive(Debug, thiserror::Error)]
pub enum FinishError {
    #[error("this volume is not a fork")]
    NotAFork,
    #[error("fork {fork} has no valid fork remote in its settings; it stays active")]
    NoForkRemote { fork: String },
    #[error(
        "fork {fork} is unlinked: it delivers nothing to its source, so it is never finished; \
         its history is pushed like any volume's"
    )]
    Unlinked { fork: String },
    #[error("fork {fork} was abandoned; nothing agentfs runs pushes it")]
    Abandoned { fork: String },
    /// The local-only source is in use and no service of it answered.
    #[error(transparent)]
    SourceBusy(crate::lock::LockError),
    #[error(transparent)]
    Volume(#[from] VolumeError),
    #[error(transparent)]
    History(#[from] HistoryError),
    #[error(transparent)]
    Status(#[from] StatusError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// A failed publish attempt: what it met, and how the push failed.
struct Failed {
    met: Met,
    kind: PushFailureKind,
    message: String,
}

impl Failed {
    fn branch(kind: PushFailureKind, message: String) -> Self {
        Self {
            met: Met::Branch(kind),
            kind,
            message,
        }
    }

    fn create_only(kind: PushFailureKind, message: String) -> Self {
        Self {
            met: Met::CreateOnly(kind),
            kind,
            message,
        }
    }
}

/// How one publish attempt delivered the fork.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Delivered {
    /// Change and finish refs create-only, or into a local-only source.
    Published,
    /// Into the parent fork's inbox as patch `inbox`.
    Submitted { inbox: u64 },
    /// The parent had finished: published as a top-level fork.
    ParentFinished { parent: String },
}

/// What a roll-up does next, from one listing of `refs/agentfs/<parent>/`
/// on the fork remote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RollUp {
    /// `change` is at the parent's `applied/<n>`, or at its `inbox/<n>`
    /// while the parent has not finished
    /// ([`crate::patch::submit::listed_as`]); nothing more is pushed.
    Delivered { inbox: u64 },
    /// The parent's `finish` ref ([`super::identity::finish_ref`]) is
    /// listed.
    ParentFinished,
    /// Push `inbox_ref(parent, inbox) = change` create-only;
    /// `inbox` = [`crate::patch::submit::next_seq`].
    Submit { inbox: u64 },
}

/// [`RollUp`] for `change` against `listed`. Applied wins over
/// ParentFinished, which wins over queued, which wins over Submit: a
/// change only queued when the parent finished is not carried by the
/// parent's change, so it is published as a top-level fork's.
fn roll_up_step(parent: &str, change: Oid, listed: &BTreeMap<String, Oid>) -> RollUp {
    let at = listed_as(
        parent,
        listed
            .iter()
            .filter(|(_, oid)| **oid == change)
            .map(|(name, _)| name),
    );
    if let Some(inbox) = at.applied {
        return RollUp::Delivered { inbox };
    }
    if listed.contains_key(&finish_ref(parent)) {
        return RollUp::ParentFinished;
    }
    if let Some(inbox) = at.inbox {
        return RollUp::Delivered { inbox };
    }
    RollUp::Submit {
        inbox: next_seq(parent, listed.keys()),
    }
}

/// A fork opened for finishing and publishing.
pub struct Finisher {
    state: StateDir,
    tree: std::path::PathBuf,
    identity: ForkIdentity,
    recorder: Recorder,
    committer: Committer,
    events: Events,
    telemetry: Telemetry,
    receiver: Box<dyn Receiver>,
}

impl Finisher {
    /// Opens the fork at `state` for finishing: crash recovery as at every
    /// open. `lock` is the state-area lock this process holds on `state`.
    pub fn open(
        state: &StateDir,
        tree: &Path,
        context: Context,
        events: Events,
        telemetry: &Telemetry,
        lock: &StateLock,
    ) -> Result<Self, FinishError> {
        if lock.path() != state.lock_file() {
            return Err(FinishError::Io(io::Error::other(format!(
                "the lock held is {}, not this state area's {}",
                lock.path().display(),
                state.lock_file().display()
            ))));
        }
        let settings = Settings::load(&state.git()).map_err(HistoryError::from)?;
        let identity = settings.fork.ok_or(FinishError::NotAFork)?;
        if identity.link == ForkLink::Unlinked {
            return Err(FinishError::Unlinked {
                fork: identity.fork,
            });
        }
        refuse_abandoned(state, &identity.fork)?;
        let (recorder, committer, _) =
            volume::open(state, tree, context, events.clone(), telemetry)?;
        Ok(Self {
            state: state.clone(),
            tree: tree.to_path_buf(),
            identity,
            recorder,
            committer,
            events,
            telemetry: telemetry.clone(),
            receiver: Box::new(UnderLock),
        })
    }

    /// How a fork of a local-only source reaches it ([`UnderLock`] by
    /// default; the CLI asks the source's service first).
    pub fn set_receiver(&mut self, receiver: Box<dyn Receiver>) {
        self.receiver = receiver;
    }

    /// Finishes the fork once (see the module docs). A fork already
    /// finished returns its record unchanged.
    pub fn finish(&mut self) -> Result<FinishRecord, FinishError> {
        let fork = self.identity.fork.clone();
        refuse_abandoned(&self.state, &fork)?;
        if let Some(record) = self.finished()? {
            // A crash between the finish commit and its ref.
            self.set_ref(&finish_ref(&fork), record.finish)?;
            return Ok(record);
        }
        let span = self.telemetry.span(spans::FORK_FINISH, || {
            vec![KeyValue::new("agentfs.fork", fork.clone())]
        });
        let finished = self.finish_once(&fork);
        match &finished {
            Ok(record) => {
                if let Some(change) = record.change {
                    span.set(KeyValue::new("agentfs.fork.change", change.to_string()));
                }
                span.set(KeyValue::new(
                    "agentfs.fork.unrecorded",
                    i64::try_from(record.unrecorded).unwrap_or(i64::MAX),
                ));
                self.events.emit(Event::ForkFinished {
                    fork: record.fork.clone(),
                    change: record.change,
                });
            }
            Err(e) => span.error(e),
        }
        span.end();
        finished
    }

    /// Writes the change and the finish commit of a fork not finished yet.
    fn finish_once(&mut self, fork: &str) -> Result<FinishRecord, FinishError> {
        let fork = fork.to_owned();
        let history = self.committer.history();
        let remote = match &history.settings().fork_remote {
            Some(ForkRemote::Remote(r)) => r.name.clone(),
            Some(ForkRemote::SourceHistory) => SOURCE_HISTORY.to_owned(),
            None => return Err(FinishError::NoForkRemote { fork }),
        };
        let unrecorded = self.record_repairs();
        self.committer.commit_now()?;
        let history = self.committer.history();
        let repo = history.repo();
        let head = history
            .head()?
            .ok_or_else(|| git2::Error::from_str("the fork has no history"))
            .map_err(HistoryError::from)?;
        let head_tree = head.tree_id();
        let base = self.identity.base;
        let base_tree = repo
            .find_commit(base)
            .map(|c| c.tree_id())
            .map_err(HistoryError::from)?;
        let signature = self
            .committer
            .committer_signature()
            .map_err(HistoryError::from)?;
        let source = &self.identity.source;
        let change = if head_tree == base_tree {
            None
        } else {
            let name = change_ref(&fork);
            // A change a crash left before the finish commit, of another
            // tree: never published (publishing needs the finish), so it is
            // made again.
            if let Ok(existing) = repo.find_reference(&name)
                && let Ok(commit) = existing.peel_to_commit()
                && (commit.tree_id() != head_tree || !commit.parent_ids().eq([base]))
            {
                repo.find_reference(&name)
                    .and_then(|mut r| r.delete())
                    .map_err(HistoryError::from)?;
            }
            let message = format!(
                "change of fork {fork}\n\n{FORK_TRAILER}: {fork}\n{SOURCE_TRAILER}: {source}\n\
                 {BASE_TRAILER}: {base}\n"
            );
            Some(history.commit_detached(&name, head_tree, base, &message, &signature)?)
        };
        let seq = history.head_seq()?.unwrap_or_default();
        let mut message = format!(
            "finish fork {fork}\n\n{}: {seq}\nAgentfs-Trigger: {FINISH_TRIGGER}\n\
             {FORK_TRAILER}: {fork}\n{SOURCE_TRAILER}: {source}\n{BASE_TRAILER}: {base}\n",
            crate::history::SEQ_TRAILER
        );
        if let Some(change) = change {
            message.push_str(&format!("{CHANGE_TRAILER}: {change}\n"));
        }
        message.push_str(&format!(
            "{FORK_REMOTE_TRAILER}: {remote}\n{UNRECORDED_TRAILER}: {unrecorded}\n"
        ));
        let finish = history.commit_marker(&message, &signature)?;
        self.set_ref(&finish_ref(&fork), finish)?;
        Ok(FinishRecord {
            fork,
            source: source.clone(),
            base,
            finish,
            change,
            remote,
            unrecorded,
        })
    }

    /// The finish record of the branch head, if it is a finish commit.
    fn finished(&self) -> Result<Option<FinishRecord>, FinishError> {
        let Some(head) = self.committer.history().head()? else {
            return Ok(None);
        };
        Ok(finish_from_message(
            head.message_raw().unwrap_or_default(),
            head.id(),
        ))
    }

    /// The local ref `name` at `oid`.
    fn set_ref(&self, name: &str, oid: Oid) -> Result<(), FinishError> {
        let repo = self.committer.history().repo();
        if repo.refname_to_id(name).ok() != Some(oid) {
            repo.reference(name, oid, true, "agentfs: fork finished")
                .map_err(HistoryError::from)?;
        }
        Ok(())
    }

    /// Records each path the FUSE layer could not record (design §3.5) as
    /// the tree holds it: `meta/repairs`, and every front slot's queue
    /// (`meta/fronts/slot-<n>.repairs`), which would otherwise wait for a
    /// next front in that slot that a finished fork never gets. Each entry
    /// is committed as the pod that queued it. Every slot is read, a live
    /// lease or not: a repair is idempotent, and a front still running
    /// could no longer record into a finished fork. Returns how many could
    /// not be recorded, each alarmed; a queue that cannot be read counts as
    /// one.
    fn record_repairs(&self) -> u64 {
        let mut unrecorded = 0;
        let alarm = |paths: Vec<RepoPath>, error: String| {
            self.events.emit(Event::RecordFailed {
                trigger: Trigger::Sweep,
                paths,
                error,
            });
        };
        for store in self.repair_stores() {
            let saved = match store.and_then(|store| {
                read_repairs(&store).map_err(|e| format!("reading {}: {e}", store.display()))
            }) {
                Ok(saved) => saved,
                Err(error) => {
                    unrecorded += 1;
                    alarm(Vec::new(), format!("a repair queue of the fork: {error}"));
                    continue;
                }
            };
            for (raw, caller, origin) in saved.entries {
                let path = match String::from_utf8(raw)
                    .map_err(|e| e.to_string())
                    .and_then(|p| RepoPath::new(p).map_err(|e| e.to_string()))
                {
                    Ok(path) => path,
                    Err(error) => {
                        unrecorded += 1;
                        alarm(
                            Vec::new(),
                            format!("a repair of {}: {error}", self.tree.display()),
                        );
                        continue;
                    }
                };
                let input = Input::Tree { path: path.clone() };
                if let Err(e) = self
                    .recorder
                    .record_as(Trigger::Sweep, caller, vec![input], origin)
                {
                    unrecorded += 1;
                    alarm(vec![path], e.to_string());
                }
            }
        }
        unrecorded
    }

    /// The repair queues of the fork's state area: `meta/repairs`, then
    /// each front slot with a queue or the temporary file of one, in slot
    /// order. A fronts directory that cannot be listed is an error entry.
    fn repair_stores(&self) -> Vec<Result<std::path::PathBuf, String>> {
        let mut stores = vec![Ok(self.state.repairs())];
        let dir = self.state.fronts_dir();
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return stores,
            Err(e) => {
                stores.push(Err(format!("listing {}: {e}", dir.display())));
                return stores;
            }
        };
        let mut slots: Vec<u16> = entries
            .flatten()
            .filter_map(|entry| {
                let name = entry.file_name();
                let rest = name.to_str()?.strip_prefix("slot-")?;
                let slot = rest
                    .strip_suffix(".repairs")
                    .or_else(|| rest.strip_suffix(".tmp"))?;
                slot.parse::<u16>().ok()
            })
            .collect();
        slots.sort_unstable();
        slots.dedup();
        stores.extend(
            slots
                .into_iter()
                .map(|slot| Ok(self.state.front_repairs(slot))),
        );
        stores
    }

    /// Clears a refused mark so publish tries again (`fork finish
    /// --retry`).
    pub fn retry_refused(&mut self) -> Result<(), FinishError> {
        refuse_abandoned(&self.state, &self.identity.fork)?;
        update_mark(&self.state, |mark| mark.refused = None)?;
        Ok(())
    }

    /// Publishes the finished fork: attempts with §11.2 backoff until it
    /// is published or refused, or, with a `deadline`, until then. Safe to
    /// run again; a fork already published returns at once, and one
    /// refused returns its refusal without pushing. Finishes the fork
    /// first if it is not finished yet. Before every attempt the mark is
    /// read again: an abandoned fork stops with [`FinishError::Abandoned`].
    pub fn publish(&mut self, deadline: Option<Instant>) -> Result<PublishOutcome, FinishError> {
        let record = self.finish()?;
        let span = self.telemetry.span(spans::FORK_PUBLISH, || {
            vec![
                KeyValue::new("agentfs.fork", record.fork.clone()),
                KeyValue::new("agentfs.remote", record.remote.clone()),
            ]
        });
        let outcome = self.publish_record(&record, deadline);
        match &outcome {
            Ok(outcome) => {
                let label = match outcome {
                    PublishOutcome::Published => "published",
                    PublishOutcome::Submitted { .. } => "submitted",
                    PublishOutcome::Pending { .. } => "pending",
                    PublishOutcome::Refused { reason, .. } => {
                        span.set(KeyValue::new(
                            "agentfs.fork.refused.reason",
                            reason.as_str(),
                        ));
                        "refused"
                    }
                };
                span.set(KeyValue::new("agentfs.fork.publish.outcome", label));
            }
            Err(e) => span.error(e),
        }
        span.end();
        outcome
    }

    /// [`Finisher::publish`] of the finished fork `record`.
    fn publish_record(
        &mut self,
        record: &FinishRecord,
        deadline: Option<Instant>,
    ) -> Result<PublishOutcome, FinishError> {
        let finish = record.finish.to_string();
        let mut pusher = None;
        let mut failures = 0u32;
        loop {
            let mark = read_mark(&self.state)?.unwrap_or_else(ForkMark::empty);
            if mark.abandoned.is_some() {
                return Err(FinishError::Abandoned {
                    fork: record.fork.clone(),
                });
            }
            if let Some(published) = mark.published.as_ref().filter(|p| p.finish == finish) {
                return Ok(match published.inbox {
                    Some(inbox) => PublishOutcome::Submitted { inbox },
                    None => PublishOutcome::Published,
                });
            }
            if let Some(refused) = mark.refused.as_ref().filter(|r| r.finish == finish) {
                return Ok(PublishOutcome::Refused {
                    reason: refused.reason,
                    message: refused.message.clone(),
                });
            }
            let failed = match self.publish_once(record, &mut pusher)? {
                Ok(delivered) => {
                    let inbox = match &delivered {
                        Delivered::Submitted { inbox } => Some(*inbox),
                        Delivered::Published | Delivered::ParentFinished { .. } => None,
                    };
                    let parent_finished = match &delivered {
                        Delivered::ParentFinished { parent } => {
                            // Before the mark: a crash in between leaves the
                            // fork finished, and its next publish raises it
                            // again (at least once; twice across a crash).
                            self.events.emit(Event::ForkParentFinished {
                                fork: record.fork.clone(),
                                parent: parent.clone(),
                            });
                            Some(ParentFinishedMark {
                                parent: parent.clone(),
                            })
                        }
                        Delivered::Published | Delivered::Submitted { .. } => None,
                    };
                    let published = PublishedMark {
                        finish: finish.clone(),
                        change: record.change.map(|c| c.to_string()),
                        remote: record.remote.clone(),
                        inbox,
                        parent_finished,
                        at_ms: now_ms(),
                    };
                    update_mark(&self.state, |mark| {
                        mark.published = Some(published.clone());
                        mark.refused = None;
                    })?;
                    self.published(record);
                    return Ok(match inbox {
                        Some(inbox) => PublishOutcome::Submitted { inbox },
                        None => PublishOutcome::Published,
                    });
                }
                Err(failed) => failed,
            };
            if let Some(reason) = refused_reason(failed.met) {
                let refused = RefusedMark {
                    finish: finish.clone(),
                    remote: record.remote.clone(),
                    reason,
                    message: failed.message.clone(),
                    at_ms: now_ms(),
                };
                update_mark(&self.state, |mark| mark.refused = Some(refused))?;
                self.events.emit(Event::ForkRefused {
                    fork: record.fork.clone(),
                    remote: record.remote.clone(),
                    reason: reason.as_str(),
                    message: failed.message.clone(),
                });
                return Ok(PublishOutcome::Refused {
                    reason,
                    message: failed.message,
                });
            }
            failures = failures.saturating_add(1);
            let now = Instant::now();
            let interval = self.committer.history().settings().push_interval;
            let delay = backoff(interval, failures, PUSH_BACKOFF_CAP);
            let delay = match deadline {
                Some(deadline) if now >= deadline => {
                    return Ok(PublishOutcome::Pending {
                        kind: failed.kind,
                        message: failed.message,
                    });
                }
                Some(deadline) => delay.min(deadline - now),
                None => delay,
            };
            std::thread::sleep(delay.max(Duration::from_millis(10)));
        }
    }

    /// What the published mark, just written, is told by: the
    /// `agentfs.fork.publish.duration` point, from the finish commit's time,
    /// and [`Event::ForkPublished`].
    fn published(&self, record: &FinishRecord) {
        let repo = self.committer.history().repo();
        if let Ok(finish) = repo.find_commit(record.finish) {
            let at = UNIX_EPOCH + Duration::from_secs(finish.time().seconds().max(0) as u64);
            let elapsed = SystemTime::now().duration_since(at).unwrap_or_default();
            self.telemetry.fork_published(&record.remote, elapsed);
        }
        self.events.emit(Event::ForkPublished {
            fork: record.fork.clone(),
            remote: record.remote.clone(),
        });
    }

    /// One attempt: the history, then the change and finish refs,
    /// create-only, or for a fork of a fork the roll-up into its parent's
    /// inbox. `Ok` once the remote holds both refs at exactly their ids,
    /// or the parent's queue holds the change. The settings are read again
    /// first, so a fork remote removed meanwhile is seen at once.
    /// `Err` stops publishing: the local-only source is busy.
    fn publish_once(
        &self,
        record: &FinishRecord,
        pusher: &mut Option<RemotePusher>,
    ) -> Result<Result<Delivered, Failed>, FinishError> {
        let settings = match Settings::load(&self.state.git()) {
            Ok(settings) => settings,
            Err(e) => {
                return Ok(Err(Failed::branch(
                    PushFailureKind::Local,
                    format!("the fork's settings cannot be read, so nothing is pushed: {e}"),
                )));
            }
        };
        match &settings.fork_remote {
            Some(ForkRemote::SourceHistory) if record.remote == SOURCE_HISTORY => {
                self.receive(&settings)
            }
            Some(ForkRemote::Remote(remote)) if remote.name == record.remote => {
                Ok(self.push_once(record, &settings, remote, pusher))
            }
            _ => Ok(Err(Failed {
                met: Met::RemoteGone,
                kind: PushFailureKind::Local,
                message: format!(
                    "the fork's settings no longer define a valid fork remote {:?}, which its \
                     finish names",
                    record.remote
                ),
            })),
        }
    }

    /// One attempt into the local-only source's history, through the
    /// [`Receiver`] ([`Finisher::set_receiver`]).
    fn receive(&self, settings: &Settings) -> Result<Result<Delivered, Failed>, FinishError> {
        let failed = |met: Met, kind: PushFailureKind, message: String| {
            Ok(Err(Failed { met, kind, message }))
        };
        let Some(source) = settings.source_state.as_deref().map(StateDir::new) else {
            return failed(
                Met::RemoteGone,
                PushFailureKind::Local,
                "the fork records no source state area to publish into".into(),
            );
        };
        match self.receiver.receive(&source, &self.identity, &self.state) {
            Ok(_) => Ok(Ok(Delivered::Published)),
            Err(ReceiverError::Busy(busy)) => Err(FinishError::SourceBusy(busy)),
            // Every receive error decided here, so a new one is a compile
            // error until it is.
            Err(ReceiverError::Receive(e)) => match e {
                ReceiveError::SourceRemoved => failed(
                    Met::RemoteGone,
                    PushFailureKind::Local,
                    format!("{e}: {}", source.root().display()),
                ),
                ReceiveError::Diverged { .. } => {
                    failed(Met::AnotherWriter, PushFailureKind::Refused, e.to_string())
                }
                ReceiveError::SourcesRef { .. }
                | ReceiveError::BaseNotOnBranch { .. }
                | ReceiveError::LfsMismatch { .. } => {
                    failed(Met::Unsafe, PushFailureKind::Refused, e.to_string())
                }
                ReceiveError::NotPublished | ReceiveError::History(_) | ReceiveError::Io(_) => {
                    failed(
                        Met::Branch(PushFailureKind::Local),
                        PushFailureKind::Local,
                        e.to_string(),
                    )
                }
            },
            Err(e) => failed(
                Met::Branch(PushFailureKind::Local),
                PushFailureKind::Local,
                e.to_string(),
            ),
        }
    }

    /// One attempt to the fork remote `remote`: the history, then the
    /// change and finish refs, or the roll-up into the parent fork.
    fn push_once(
        &self,
        record: &FinishRecord,
        settings: &Settings,
        remote: &crate::config::RemoteSettings,
        pusher: &mut Option<RemotePusher>,
    ) -> Result<Delivered, Failed> {
        let local = |message: String| Failed::branch(PushFailureKind::Local, message);
        if pusher.is_none() {
            let opened = RemotePusher::open(
                &self.state,
                settings,
                remote,
                self.events.clone(),
                &self.telemetry,
            )
            .map_err(|e| local(e.to_string()))?;
            *pusher = Some(opened);
        }
        let pusher = pusher.as_mut().expect("opened just above");
        // The history: held once pushed, or saved as diverged when a forge
        // moved the fork's branch (§11.3, alarmed; it never blocks).
        loop {
            match pusher.push_once() {
                outcome if outcome.is_limited() => continue,
                PushOutcome::Pushed { .. } | PushOutcome::UpToDate { .. } => break,
                PushOutcome::Diverged {
                    diverged_ref: Some(_),
                    ..
                } => break,
                PushOutcome::Diverged { .. } => {
                    return Err(Failed::branch(
                        PushFailureKind::NonFastForward,
                        "the fork's branch diverged on the remote and the head is not saved yet"
                            .into(),
                    ));
                }
                PushOutcome::Failed { kind, message, .. } => {
                    return Err(Failed::branch(kind, message));
                }
                PushOutcome::Busy => {
                    return Err(local("another process is pushing the fork remote".into()));
                }
                // The finisher never cancels its pusher.
                PushOutcome::Cancelled => {
                    return Err(local("the push was cancelled".into()));
                }
                PushOutcome::NothingToPush => {
                    return Err(local("the fork has no history to push".into()));
                }
            }
        }
        let delivered = match settings.parent_fork() {
            None => {
                push_finish(record, pusher, true)?;
                Delivered::Published
            }
            Some(parent) => {
                let delivered = self.roll_up(record, parent, pusher)?;
                if !matches!(delivered, Delivered::ParentFinished { .. }) {
                    // Its own forks read this as "finished" (no change ref:
                    // the change is the parent's to carry).
                    push_finish(record, pusher, false)?;
                }
                delivered
            }
        };
        self.announce_uncarried(record, pusher)?;
        Ok(delivered)
    }

    /// With the finish ref held on the fork remote: lists the fork's own
    /// inbox there and raises [`Event::ForkInboxNotCarried`] for every
    /// entry the finished history did not land (by `Agentfs-Inbox`
    /// trailer, or an `applied/<n>` ref the queue wrote). A listing that
    /// fails fails the attempt, which is retried: the check is part of
    /// publishing.
    fn announce_uncarried(
        &self,
        record: &FinishRecord,
        pusher: &RemotePusher,
    ) -> Result<(), Failed> {
        let local = |message: String| Failed::branch(PushFailureKind::Local, message);
        let prefix = inbox_prefix(&record.fork);
        let listed = pusher
            .list_refs(&prefix)
            .map_err(|e| Failed::create_only(e.kind, e.message))?;
        let history = self.committer.history();
        let mut carried: BTreeSet<u64> = history
            .find_inbox_commits(None)
            .map_err(|e| local(e.to_string()))?
            .into_keys()
            .collect();
        let applied = applied_prefix(&record.fork);
        let mut refs = history
            .repo()
            .references_glob(&format!("{applied}*"))
            .map_err(|e| local(e.to_string()))?;
        for name in refs.names() {
            let name = name.map_err(|e| local(e.to_string()))?;
            carried.extend(seq_of_ref(&applied, name));
        }
        let inboxes: Vec<u64> = listed
            .keys()
            .filter_map(|name| seq_of_ref(&prefix, name))
            .filter(|n| !carried.contains(n))
            .collect();
        if !inboxes.is_empty() {
            self.events.emit(Event::ForkInboxNotCarried {
                fork: record.fork.clone(),
                remote: record.remote.clone(),
                inboxes,
            });
        }
        Ok(())
    }

    /// A fork of a fork's change into its parent's inbox on the fork
    /// remote, through `pusher` (see the module docs): at most
    /// [`DEFAULT_ATTEMPTS`] numbers lost to other writers per attempt,
    /// then the attempt fails and is retried. A fork with no change is
    /// published once its history is.
    fn roll_up(
        &self,
        record: &FinishRecord,
        parent: &str,
        pusher: &mut RemotePusher,
    ) -> Result<Delivered, Failed> {
        let Some(change) = record.change else {
            return Ok(Delivered::Published);
        };
        let prefix = format!("refs/agentfs/{parent}/");
        let mut lost = 0;
        let mut pushed = None;
        loop {
            let listed = pusher
                .list_refs(&prefix)
                .map_err(|e| Failed::create_only(e.kind, e.message))?;
            let inbox = match roll_up_step(parent, change, &listed) {
                RollUp::Delivered { inbox } => return Ok(Delivered::Submitted { inbox }),
                RollUp::ParentFinished => {
                    push_finish(record, pusher, true)?;
                    return Ok(Delivered::ParentFinished {
                        parent: parent.to_owned(),
                    });
                }
                RollUp::Submit { inbox } => inbox,
            };
            if let Some(held) = pushed {
                return Err(Failed::create_only(
                    PushFailureKind::Local,
                    format!(
                        "{} was pushed, but the fork remote does not list it; tried again \
                         later",
                        inbox_ref(parent, held)
                    ),
                ));
            }
            if lost == DEFAULT_ATTEMPTS {
                return Err(Failed::create_only(
                    PushFailureKind::NonFastForward,
                    format!(
                        "other writers took each of {DEFAULT_ATTEMPTS} inbox numbers of fork \
                         {parent} this attempt tried; tried again later"
                    ),
                ));
            }
            let refs = [CreateOnlyRef {
                name: inbox_ref(parent, inbox),
                oid: change,
            }];
            match pusher.push_create_only(&refs) {
                // Listed again: a parent that finished after the listing
                // above did not see this entry (design §12 "Forks of
                // forks").
                CreateOnlyOutcome::Held => pushed = Some(inbox),
                // Someone took the number: list again and decide again.
                CreateOnlyOutcome::AnotherWriter { .. } => lost += 1,
                CreateOnlyOutcome::Failed { kind, message } => {
                    return Err(Failed::create_only(kind, message));
                }
            }
        }
    }
}

/// The finish ref of `record`, and with `with_change` its change ref,
/// create-only: a top-level fork's publish, and a fork of a fork's whose
/// parent finished first, with the change; a roll-up's, without.
fn push_finish(
    record: &FinishRecord,
    pusher: &mut RemotePusher,
    with_change: bool,
) -> Result<(), Failed> {
    let mut refs = Vec::new();
    if let Some(change) = record.change.filter(|_| with_change) {
        refs.push(CreateOnlyRef {
            name: change_ref(&record.fork),
            oid: change,
        });
    }
    refs.push(CreateOnlyRef {
        name: finish_ref(&record.fork),
        oid: record.finish,
    });
    match pusher.push_create_only(&refs) {
        CreateOnlyOutcome::Held => Ok(()),
        CreateOnlyOutcome::Failed { kind, message } => Err(Failed::create_only(kind, message)),
        CreateOnlyOutcome::AnotherWriter { name, found } => Err(Failed {
            met: Met::AnotherWriter,
            kind: PushFailureKind::Refused,
            message: format!(
                "{name} on the remote holds {found}, another writer's; it is never moved"
            ),
        }),
    }
}

/// `meta/fork.json`, `None` when there is none.
fn read_mark(state: &StateDir) -> Result<Option<ForkMark>, StatusError> {
    match std::fs::read(state.fork_mark()) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| StatusError::Mark(io::Error::other(e))),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(StatusError::Mark(e)),
    }
}

/// Reads `meta/fork.json`, changes it with `change` and writes it back
/// atomically. A mark that cannot be read is an error, never replaced.
fn update_mark(state: &StateDir, change: impl FnOnce(&mut ForkMark)) -> Result<(), StatusError> {
    let mut mark = read_mark(state)?.unwrap_or_else(ForkMark::empty);
    change(&mut mark);
    mark.version = MARK_VERSION;
    crate::fsutil::write_json_atomic(&state.fork_mark(), &mark).map_err(StatusError::Mark)
}

/// [`FinishError::Abandoned`] once the operator has abandoned `fork`.
fn refuse_abandoned(state: &StateDir, fork: &str) -> Result<(), FinishError> {
    match read_mark(state)? {
        Some(mark) if mark.abandoned.is_some() => Err(FinishError::Abandoned {
            fork: fork.to_owned(),
        }),
        _ => Ok(()),
    }
}

/// The operator's recorded discard, in any state but published; with the
/// state lock held. After it returns, nothing agentfs runs pushes the
/// fork: [`Finisher::open`], [`Finisher::finish`] and every publish
/// attempt re-read the mark first and stop with [`FinishError::Abandoned`].
/// A push already running finishes or times out (libgit2 cannot cancel
/// one, §11.2): abandon does not wait for it, and a fork remote may then
/// still receive it. Abandoning an abandoned fork returns its status
/// unchanged.
pub fn abandon(
    state: &StateDir,
    lock: &StateLock,
    reason: &str,
    caller: Caller,
) -> Result<ForkStatus, AbandonError> {
    if lock.path() != state.lock_file() {
        return Err(AbandonError::Io(io::Error::other(format!(
            "the lock held is {}, not this state area's {}",
            lock.path().display(),
            state.lock_file().display()
        ))));
    }
    let reason = reason.trim();
    if reason.is_empty() {
        return Err(AbandonError::EmptyReason);
    }
    let Some(current) = status(state)? else {
        return Err(AbandonError::NotAFork);
    };
    match current.state {
        ForkState::Published => {
            return Err(AbandonError::Published {
                fork: current.identity.fork,
            });
        }
        ForkState::Abandoned => return Ok(current),
        ForkState::Active | ForkState::Finished | ForkState::Refused => {}
    }
    let abandoned = AbandonedMark {
        reason: reason.to_owned(),
        at_ms: now_ms(),
        uid: caller.uid,
        state: current.state.as_str().to_owned(),
    };
    update_mark(state, |mark| mark.abandoned = Some(abandoned))?;
    status(state)?.ok_or(AbandonError::NotAFork)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// The fork status of the state area at `state`, read without a lock or
/// credentials: `Ok(None)` for a volume that is not a fork, and for an
/// unlinked fork, which delivers nothing (design §11.5). What cannot be
/// read is an error, never a state, so a caller deciding whether to
/// delete waits. Abandoned wins, then published, then refused (each
/// naming the branch head's finish commit), then finished or active.
/// Whether the volume is a fork is decided from its own config first
/// ([`crate::config::declares_fork`]), so a volume that is not one is
/// `Ok(None)` even when its settings cannot be loaded.
pub fn status(state: &StateDir) -> Result<Option<ForkStatus>, StatusError> {
    crate::history::init_libgit2();
    if !crate::config::declares_fork(&state.git()).map_err(HistoryError::from)? {
        return Ok(None);
    }
    let settings = Settings::load(&state.git()).map_err(HistoryError::from)?;
    let Some(identity) = settings.fork.clone() else {
        return Ok(None);
    };
    if identity.link == ForkLink::Unlinked {
        return Ok(None);
    }
    let repo = Repository::open_bare(state.git()).map_err(HistoryError::from)?;
    let record = match repo.find_reference(&settings.branch_ref) {
        Ok(r) => {
            let head = r.peel_to_commit().map_err(HistoryError::from)?;
            finish_from_message(head.message_raw().unwrap_or_default(), head.id())
        }
        Err(e) if e.code() == git2::ErrorCode::NotFound => None,
        Err(e) => return Err(HistoryError::from(e).into()),
    };
    let mark = read_mark(state)?;
    let finish = record.as_ref().map(|r| r.finish.to_string());
    let names_finish = |f: &str| finish.as_deref() == Some(f);
    let fork_state = match &mark {
        Some(m) if m.abandoned.is_some() => ForkState::Abandoned,
        Some(m)
            if m.published
                .as_ref()
                .is_some_and(|p| names_finish(&p.finish)) =>
        {
            ForkState::Published
        }
        Some(m) if m.refused.as_ref().is_some_and(|r| names_finish(&r.finish)) => {
            ForkState::Refused
        }
        _ if record.is_some() => ForkState::Finished,
        _ => ForkState::Active,
    };
    Ok(Some(ForkStatus {
        identity,
        state: fork_state,
        record,
        mark,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oid(n: u8) -> Oid {
        Oid::from_bytes(&[n; 20]).unwrap()
    }

    fn listing(refs: &[(&str, Oid)]) -> BTreeMap<String, Oid> {
        refs.iter().map(|(n, o)| ((*n).to_owned(), *o)).collect()
    }

    /// One listing decides the roll-up: already queued or applied wins,
    /// then a finished parent, else the next number.
    #[test]
    fn roll_up_step_over_listings() {
        let change = oid(1);
        let other = oid(2);
        assert_eq!(
            roll_up_step("p", change, &listing(&[])),
            RollUp::Submit { inbox: 1 }
        );
        let inbox_hit = listing(&[
            ("refs/agentfs/p/inbox/0000000001", other),
            ("refs/agentfs/p/inbox/0000000002", change),
        ]);
        assert_eq!(
            roll_up_step("p", change, &inbox_hit),
            RollUp::Delivered { inbox: 2 }
        );
        let applied_hit = listing(&[
            ("refs/agentfs/p/applied/0000000004", change),
            ("refs/agentfs/p/inbox/0000000005", other),
        ]);
        assert_eq!(
            roll_up_step("p", change, &applied_hit),
            RollUp::Delivered { inbox: 4 }
        );
        let finished = listing(&[
            ("refs/agentfs/p/finish", other),
            ("refs/agentfs/p/inbox/0000000001", other),
        ]);
        assert_eq!(roll_up_step("p", change, &finished), RollUp::ParentFinished);
        let finished_and_queued = listing(&[
            ("refs/agentfs/p/finish", other),
            ("refs/agentfs/p/inbox/0000000003", change),
        ]);
        // Queued but not landed when the parent finished: its change does
        // not carry it, so the child publishes itself.
        assert_eq!(
            roll_up_step("p", change, &finished_and_queued),
            RollUp::ParentFinished
        );
        let finished_and_applied = listing(&[
            ("refs/agentfs/p/finish", other),
            ("refs/agentfs/p/applied/0000000003", change),
        ]);
        assert_eq!(
            roll_up_step("p", change, &finished_and_applied),
            RollUp::Delivered { inbox: 3 }
        );
        // Other numbers, the parent's change ref and malformed names: the
        // next number past the highest well-formed one.
        let taken = listing(&[
            ("refs/agentfs/p/inbox/0000000001", other),
            ("refs/agentfs/p/applied/0000000002", other),
            ("refs/agentfs/p/change", change),
            ("refs/agentfs/p/inbox/9", change),
        ]);
        assert_eq!(
            roll_up_step("p", change, &taken),
            RollUp::Submit { inbox: 3 }
        );
    }

    /// A mark written before rolled-up changes existed reads, and a
    /// top-level publish writes no `inbox`.
    #[test]
    fn a_published_mark_without_inbox_reads() {
        let old =
            r#"{"version":1,"published":{"finish":"f","change":null,"remote":"r","at_ms":1}}"#;
        let mark: ForkMark = serde_json::from_str(old).unwrap();
        assert_eq!(mark.published.as_ref().unwrap().inbox, None);
        let written = serde_json::to_string(&mark).unwrap();
        assert!(!written.contains("inbox"), "{written}");
        let mut rolled = mark.clone();
        rolled.published.as_mut().unwrap().inbox = Some(3);
        let back: ForkMark =
            serde_json::from_str(&serde_json::to_string(&rolled).unwrap()).unwrap();
        assert_eq!(back, rolled);
    }
}
