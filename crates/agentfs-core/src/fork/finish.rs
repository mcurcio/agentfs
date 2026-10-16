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
//! the published mark. A remote that refuses for good (see
//! [`refused_reason`], the single decider) is recorded as refused and not
//! retried until the operator clears it ([`Finisher::retry_refused`]).
//! [`abandon`] records the operator's discard, after which nothing agentfs
//! runs pushes the fork. [`status`] reads, without a lock or credentials,
//! what DeleteVolume asks: whether the fork is published or abandoned.

use std::io;
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use git2::{Oid, Repository};
use serde::{Deserialize, Serialize};

use crate::config::Settings;
use crate::events::{Event, Events, PushFailureKind};
use crate::history::{HistoryError, finish_from_message};
use crate::identity::Context;
use crate::lock::StateLock;
use crate::paths::RepoPath;
use crate::push::{
    CreateOnlyOutcome, CreateOnlyRef, PUSH_BACKOFF_CAP, PushOutcome, RemotePusher, backoff,
};
use crate::record::{Caller, Trigger};
use crate::telemetry::Telemetry;
use crate::volume::{self, Committer, Input, Recorder, StateDir, VolumeError};

use super::identity::{ForkIdentity, change_ref, finish_ref};

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
    /// finish. "Published" is judged against this remote.
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
}

impl RefusedReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AnotherWriter => "another_writer",
            Self::TooLarge => "too_large",
            Self::Rejected => "rejected",
            Self::RemoteGone => "remote_gone",
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
    /// The finish names a remote that is not the fork's fork remote now.
    RemoteGone,
}

/// The single decider of "permanent" (design §11.5): what makes a publish
/// refused, alarmed once and never retried by the node plugin's
/// publisher. Everything else (unreachable, auth, a moved branch, a busy
/// remote) is pending and retried with backoff (§11.3).
pub fn refused_reason(met: Met) -> Option<RefusedReason> {
    match met {
        Met::AnotherWriter => Some(RefusedReason::AnotherWriter),
        Met::RemoteGone => Some(RefusedReason::RemoteGone),
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
    /// When the remote confirmed it (milliseconds since the Unix epoch).
    pub at_ms: u64,
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
    #[error("this volume is not a fork")]
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
    #[error("fork {fork} was abandoned; nothing agentfs runs pushes it")]
    Abandoned { fork: String },
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
        })
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
        let history = self.committer.history();
        let Some(remote) = history
            .settings()
            .fork_remote
            .as_ref()
            .map(|r| r.name.clone())
        else {
            return Err(FinishError::NoForkRemote { fork });
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

    /// Records each path the FUSE layer could not record (`meta/repairs`,
    /// design §3.5) as the tree holds it. Returns how many could not be
    /// recorded, each alarmed.
    fn record_repairs(&self) -> u64 {
        let Ok(bytes) = std::fs::read(self.state.repairs()) else {
            return 0;
        };
        let mut unrecorded = 0;
        for (raw, caller) in parse_repairs(&bytes) {
            let path = match String::from_utf8(raw)
                .map_err(|e| e.to_string())
                .and_then(|p| RepoPath::new(p).map_err(|e| e.to_string()))
            {
                Ok(path) => path,
                Err(error) => {
                    unrecorded += 1;
                    self.events.emit(Event::RecordFailed {
                        trigger: Trigger::Sweep,
                        paths: Vec::new(),
                        error: format!("a repair of {}: {error}", self.tree.display()),
                    });
                    continue;
                }
            };
            let input = Input::Tree { path: path.clone() };
            if let Err(e) = self.recorder.record(Trigger::Sweep, caller, vec![input]) {
                unrecorded += 1;
                self.events.emit(Event::RecordFailed {
                    trigger: Trigger::Sweep,
                    paths: vec![path],
                    error: e.to_string(),
                });
            }
        }
        unrecorded
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
            if mark.published.as_ref().is_some_and(|p| p.finish == finish) {
                return Ok(PublishOutcome::Published);
            }
            if let Some(refused) = mark.refused.as_ref().filter(|r| r.finish == finish) {
                return Ok(PublishOutcome::Refused {
                    reason: refused.reason,
                    message: refused.message.clone(),
                });
            }
            let failed = match self.publish_once(&record, &mut pusher) {
                Ok(()) => {
                    let published = PublishedMark {
                        finish: finish.clone(),
                        change: record.change.map(|c| c.to_string()),
                        remote: record.remote.clone(),
                        at_ms: now_ms(),
                    };
                    update_mark(&self.state, |mark| {
                        mark.published = Some(published);
                        mark.refused = None;
                    })?;
                    return Ok(PublishOutcome::Published);
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

    /// One attempt: the history, then the change and finish refs,
    /// create-only. `Ok` once the remote holds both refs at exactly their
    /// ids. The settings are read again first, so a fork remote removed
    /// meanwhile is seen at once.
    fn publish_once(
        &self,
        record: &FinishRecord,
        pusher: &mut Option<RemotePusher>,
    ) -> Result<(), Failed> {
        let local = |message: String| Failed::branch(PushFailureKind::Local, message);
        let settings = Settings::load(&self.state.git()).map_err(|e| {
            local(format!(
                "the fork's settings cannot be read, so nothing is pushed: {e}"
            ))
        })?;
        let Some(remote) = settings
            .fork_remote
            .as_ref()
            .filter(|r| r.name == record.remote)
        else {
            return Err(Failed {
                met: Met::RemoteGone,
                kind: PushFailureKind::Local,
                message: format!(
                    "the fork's settings no longer define a valid fork remote {:?}, which its \
                     finish names",
                    record.remote
                ),
            });
        };
        if pusher.is_none() {
            let opened = RemotePusher::open(
                &self.state,
                &settings,
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
                PushOutcome::NothingToPush => {
                    return Err(local("the fork has no history to push".into()));
                }
            }
        }
        let mut refs = Vec::new();
        if let Some(change) = record.change {
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
            CreateOnlyOutcome::Failed { kind, message } => Err(Failed {
                met: Met::CreateOnly(kind),
                kind,
                message,
            }),
            CreateOnlyOutcome::AnotherWriter { name, found } => Err(Failed {
                met: Met::AnotherWriter,
                kind: PushFailureKind::Refused,
                message: format!(
                    "{name} on the remote holds {found}, another writer's; it is never moved"
                ),
            }),
        }
    }
}

/// The repair queue's entries: `<uid> <gid> <pid> <hex path>` lines, the
/// FUSE layer's format (`agentfs-fuse` boundary), `@origin` lines skipped.
fn parse_repairs(bytes: &[u8]) -> Vec<(Vec<u8>, Caller)> {
    let text = String::from_utf8_lossy(bytes);
    let mut out = Vec::new();
    for line in text.lines() {
        out.extend(parse_repair(line));
    }
    out
}

fn parse_repair(line: &str) -> Option<(Vec<u8>, Caller)> {
    let mut f = line.split(' ');
    let uid = f.next()?.parse().ok()?;
    let gid = f.next()?.parse().ok()?;
    let pid = f.next()?.parse().ok()?;
    let hex = f.next()?;
    if f.next().is_some() || hex.len() % 2 != 0 {
        return None;
    }
    let path = (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok())
        .collect::<Option<Vec<u8>>>()?;
    Some((path, Caller { uid, gid, pid }))
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
/// credentials: `Ok(None)` for a volume that is not a fork. What cannot be
/// read is an error, never a state, so a caller deciding whether to
/// delete waits. Abandoned wins, then published, then refused (each
/// naming the branch head's finish commit), then finished or active.
pub fn status(state: &StateDir) -> Result<Option<ForkStatus>, StatusError> {
    crate::history::init_libgit2();
    let settings = Settings::load(&state.git()).map_err(HistoryError::from)?;
    let Some(identity) = settings.fork.clone() else {
        return Ok(None);
    };
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
