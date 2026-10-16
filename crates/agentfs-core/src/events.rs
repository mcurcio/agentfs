//! Alarm and event signals (design §13).
//!
//! The engine reports what operators need to see (refused paths, failed
//! records and commits, push failures and divergence) as [`Event`]s handed
//! to an [`EventSink`]. [`crate::telemetry::Telemetry::sink`] wraps a sink so
//! events also become OpenTelemetry metrics and logs; the CLI also prints
//! alarms to stderr.
//! This module holds no telemetry or IO policy of its own.
//!
//! [`EventSink::emit`] is called from the synchronous step of a record
//! (design §3.2), on the FUSE threads, so a sink must never block: count,
//! enqueue or drop, but do not wait on IO or locks held for long.

use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use git2::Oid;
use serde::{Deserialize, Serialize};

use crate::checks::Refusal;
use crate::paths::RepoPath;
use crate::record::Trigger;

/// Where a path was refused (design §3.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckStage {
    /// In the synchronous step: the path was never journaled.
    Record,
    /// While committing a record journaled before the limits changed.
    Commit,
}

impl CheckStage {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Record => "record",
            Self::Commit => "commit",
        }
    }
}

impl fmt::Display for CheckStage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why a push failed (design §11.3), as a telemetry label.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PushFailureKind {
    /// The remote could not be reached: network, timeouts, or a missing
    /// path.
    Unreachable,
    /// The remote rejected the credentials, or none were available.
    Auth,
    /// The remote host is not trusted: its SSH host key is unknown, changed
    /// or revoked in `agentfsKnownHosts`, or its TLS certificate failed
    /// verification. Possibly an attack, so it is told apart from
    /// `unreachable`.
    UntrustedHost,
    /// Someone else wrote one of the volume's refs on the remote.
    NonFastForward,
    /// The remote refused a ref update for another reason (a hook, a lock,
    /// a size limit).
    Refused,
    /// A problem on this side: the history repository or the push state.
    Local,
    /// A single commit's git pack is larger than the remote's
    /// `agentfsMaxPushSize` (design §11.3), so it can never be pushed in
    /// steps; nothing was sent.
    TooLarge,
}

impl PushFailureKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unreachable => "unreachable",
            Self::Auth => "auth",
            Self::UntrustedHost => "untrusted_host",
            Self::NonFastForward => "non_fast_forward",
            Self::Refused => "refused",
            Self::Local => "local",
            Self::TooLarge => "too_large",
        }
    }
}

impl fmt::Display for PushFailureKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Which part of a push attempt failed (design §7, §13).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PushStage {
    /// The git push itself.
    #[default]
    Git,
    /// Uploading the LFS objects the unpushed commits reference, before
    /// the git push; no ref was pushed.
    Lfs,
    /// Pushing the patch queue's refs (status, applied, inbox deletions)
    /// to the patch remote, after the branch push (design §12). Never
    /// affects the branch's backoff or divergence.
    Queue,
}

impl PushStage {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Git => "git",
            Self::Lfs => "lfs",
            Self::Queue => "queue",
        }
    }
}

impl fmt::Display for PushStage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One signal. Every variant except [`Event::PushSucceeded`],
/// [`Event::PatchApplied`], [`Event::PatchDuplicate`] and
/// [`Event::HistoryServiceRestored`] is an alarm condition (design §13).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A path was not recorded, or dropped from a commit, because git's
    /// fsck or a remote's limit would refuse it (design §3.4, §5).
    PathRefused {
        /// The path as given; for [`Refusal::InvalidPath`] it may not be a
        /// valid [`RepoPath`].
        path: String,
        reason: Refusal,
        stage: CheckStage,
    },
    /// The synchronous step failed; the triggering call gets an error
    /// (design §3.5).
    RecordFailed {
        trigger: Trigger,
        paths: Vec<RepoPath>,
        error: String,
    },
    /// Turning records into a commit failed; the records stay in the
    /// journal and are retried.
    CommitFailed { error: String, backlog: usize },
    /// The periodic repack of the state area's git directory failed
    /// (design §8); it is retried one `agentfs.repackInterval` later.
    /// History is intact, only less compact.
    RepackFailed { error: String },
    /// A moved tree could not be reconciled (design §3.1) after
    /// [`crate::reconcile::MAX_FAILURES`] attempts over at least
    /// [`crate::reconcile::GIVE_UP_AFTER`]: the move commits as
    /// journaled, so history may hold files the rules exclude at `to`,
    /// or miss files beneath it that were never tracked, until their next
    /// change.
    ReconcileFailed {
        from: RepoPath,
        to: RepoPath,
        error: String,
    },
    /// A push attempt failed (design §11.3).
    PushFailed {
        remote: String,
        kind: PushFailureKind,
        message: String,
        /// The local branch head the attempt tried to push.
        head: Option<Oid>,
        /// The last commit known to be on the remote.
        last_pushed: Option<Oid>,
        consecutive_failures: u32,
        retry_in: Duration,
        /// Where it failed: the git push, or the LFS upload before it.
        stage: PushStage,
    },
    /// The remote branch was written by someone else; the local head was
    /// saved under `diverged_ref` (design §11.3).
    Diverged {
        remote: String,
        local_head: Oid,
        remote_head: Option<Oid>,
        diverged_ref: String,
    },
    /// The backlog was larger than the remote's `agentfsMaxPushSize`, so
    /// only part of it was pushed (design §11.3): the branch now stands at
    /// `pushed`, a first-parent ancestor of `head`, with `commits_left`
    /// first-parent commits to go. The next attempt follows at once. A
    /// warning.
    PushLimited {
        remote: String,
        limit: u64,
        pushed: Oid,
        head: Oid,
        commits_left: usize,
    },
    /// A `[remote "<name>"]` section is invalid, so the remote is never
    /// pushed; recording and other remotes carry on (design §11.1).
    RemoteMisconfigured { remote: String, error: String },
    /// A push attempt succeeded, including one with nothing new to send.
    PushSucceeded {
        remote: String,
        head: Oid,
        previous: Option<Oid>,
    },
    /// A queued patch was applied at mount (design §12). Not an alarm.
    PatchApplied {
        inbox: u64,
        patch: Oid,
        /// The branch commit that applies it.
        commit: Oid,
    },
    /// A queued patch commit was already applied under an earlier inbox
    /// number, so it was consumed with a marker commit and not applied
    /// again (design §12). Not an alarm.
    PatchDuplicate {
        inbox: u64,
        patch: Oid,
        /// The inbox number that applied the same patch commit first.
        applied_as: u64,
    },
    /// The patch queue stopped at a patch it could not apply; it and the
    /// later ones stay queued (design §12).
    PatchBlocked {
        inbox: u64,
        patch: Option<Oid>,
        /// [`crate::patch::BlockReason::as_str`].
        reason: &'static str,
        message: String,
        /// Patches still queued, the blocked one included.
        queued: usize,
    },
    /// An inbox entry the queue does not apply and that does not block it
    /// (design §12): `stale` (numbered at or below the last applied patch,
    /// with another commit; the queue push deletes it on the remote) or
    /// `malformed`. A warning: a producer's change was not applied.
    PatchIgnored {
        ref_name: String,
        inbox: Option<u64>,
        patch: Option<Oid>,
        why: String,
    },
    /// Fetching the patch queue from its remote failed; the queue ran with
    /// what was already fetched (design §12).
    PatchFetchFailed {
        remote: String,
        kind: PushFailureKind,
        message: String,
    },
    /// A front could not reach its volume's history service, or the
    /// service did not answer in time or refused it (design §14, §3.5):
    /// the boundary failed with `EIO`. Raised once per outage, at its
    /// first failure; [`Event::HistoryServiceRestored`] ends it.
    HistoryServiceUnavailable { address: String, error: String },
    /// A front reached its history service again after an outage. Not an
    /// alarm.
    HistoryServiceRestored { address: String },
    /// Records from a front whose pod no longer renders with the identity
    /// templates in force (they changed across a restart of the history
    /// service) are committed with the service's own committer, keeping
    /// the pod's trailers (design §9, §14). A warning, once per pod.
    IdentityFallback { replica: String, error: String },
}

impl Event {
    /// True for alarm conditions: everything except a successful push, an
    /// applied patch and a restored history service.
    pub fn is_alarm(&self) -> bool {
        !matches!(
            self,
            Self::PushSucceeded { .. }
                | Self::PatchApplied { .. }
                | Self::PatchDuplicate { .. }
                | Self::HistoryServiceRestored { .. }
        )
    }

    /// A stable name for logs and metrics.
    pub fn name(&self) -> &'static str {
        match self {
            Self::PathRefused { .. } => "path_refused",
            Self::RecordFailed { .. } => "record_failed",
            Self::CommitFailed { .. } => "commit_failed",
            Self::RepackFailed { .. } => "repack_failed",
            Self::ReconcileFailed { .. } => "reconcile_failed",
            Self::PushFailed { .. } => "push_failed",
            Self::Diverged { .. } => "diverged",
            Self::PushLimited { .. } => "push_limited",
            Self::RemoteMisconfigured { .. } => "remote_misconfigured",
            Self::PushSucceeded { .. } => "push_succeeded",
            Self::PatchApplied { .. } => "patch_applied",
            Self::PatchDuplicate { .. } => "patch_duplicate",
            Self::PatchBlocked { .. } => "patch_blocked",
            Self::PatchIgnored { .. } => "patch_ignored",
            Self::PatchFetchFailed { .. } => "patch_fetch_failed",
            Self::HistoryServiceUnavailable { .. } => "history_service_unavailable",
            Self::HistoryServiceRestored { .. } => "history_service_restored",
            Self::IdentityFallback { .. } => "identity_fallback",
        }
    }
}

fn oid_or(oid: Option<Oid>, none: &str) -> String {
    oid.map_or_else(|| none.to_owned(), |o| o.to_string())
}

impl fmt::Display for Event {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PathRefused {
                path,
                reason,
                stage,
            } => write!(f, "path refused at {stage}: {path}: {reason}"),
            Self::RecordFailed {
                trigger,
                paths,
                error,
            } => {
                let paths: Vec<&str> = paths.iter().map(RepoPath::as_str).collect();
                write!(
                    f,
                    "record failed ({}) for {}: {error}",
                    trigger.as_str(),
                    paths.join(", ")
                )
            }
            Self::CommitFailed { error, backlog } => {
                write!(f, "commit failed, {backlog} record(s) waiting: {error}")
            }
            Self::RepackFailed { error } => write!(f, "repack failed: {error}"),
            Self::ReconcileFailed { from, to, error } => {
                write!(f, "reconciling the move of {from} to {to} failed: {error}")
            }
            Self::PushFailed {
                remote,
                kind,
                message,
                head,
                last_pushed,
                consecutive_failures,
                retry_in,
                stage,
            } => write!(
                f,
                "push to {remote} failed ({kind}{}): {message}; head {}, last pushed {}, \
                 {consecutive_failures} consecutive failure(s), retry in {retry_in:?}",
                match stage {
                    PushStage::Git => "",
                    PushStage::Lfs => ", stage lfs",
                    PushStage::Queue => ", stage queue",
                },
                oid_or(*head, "none"),
                oid_or(*last_pushed, "never"),
            ),
            Self::Diverged {
                remote,
                local_head,
                remote_head,
                diverged_ref,
            } => write!(
                f,
                "{remote} diverged: remote branch is at {}, local head {local_head} \
                 saved as {diverged_ref}",
                oid_or(*remote_head, "unknown"),
            ),
            Self::PushLimited {
                remote,
                limit,
                pushed,
                head,
                commits_left,
            } => write!(
                f,
                "push to {remote} limited to {limit} bytes (agentfsMaxPushSize): pushed up to \
                 {pushed}, {commits_left} commit(s) left before head {head}; the next step \
                 follows at once"
            ),
            Self::RemoteMisconfigured { remote, error } => {
                write!(
                    f,
                    "remote {remote:?} is misconfigured and is not pushed: {error}"
                )
            }
            Self::PushSucceeded {
                remote,
                head,
                previous,
            } => write!(
                f,
                "pushed {}..{head} to {remote}",
                oid_or(*previous, "none")
            ),
            Self::PatchApplied {
                inbox,
                patch,
                commit,
            } => write!(f, "patch {inbox} ({patch}) applied as {commit}"),
            Self::PatchDuplicate {
                inbox,
                patch,
                applied_as,
            } => write!(
                f,
                "patch {inbox} ({patch}) was already applied as patch {applied_as}; it is not \
                 applied again"
            ),
            Self::PatchBlocked {
                inbox,
                patch,
                reason,
                message,
                queued,
            } => write!(
                f,
                "patch queue blocked at {inbox} ({}, {reason}): {message}; {queued} patch(es) \
                 queued",
                oid_or(*patch, "no patch"),
            ),
            Self::PatchIgnored {
                ref_name,
                inbox: _,
                patch,
                why,
            } => write!(
                f,
                "patch queue: ignoring {ref_name} ({}, {why}); it is never applied",
                oid_or(*patch, "no commit"),
            ),
            Self::PatchFetchFailed {
                remote,
                kind,
                message,
            } => write!(
                f,
                "fetching the patch queue from {remote} failed ({kind}): {message}"
            ),
            Self::HistoryServiceUnavailable { address, error } => write!(
                f,
                "history service {address} is unavailable: {error}; changes fail with EIO \
                 until it is back"
            ),
            Self::HistoryServiceRestored { address } => {
                write!(f, "history service {address} is reachable again")
            }
            Self::IdentityFallback { replica, error } => write!(
                f,
                "the committer of front {replica} no longer renders ({error}); its records are \
                 committed with the history service's own committer"
            ),
        }
    }
}

/// Receives events. Implementations must not block (see the module docs).
pub trait EventSink: Send + Sync {
    fn emit(&self, event: Event);
}

impl<F: Fn(Event) + Send + Sync> EventSink for F {
    fn emit(&self, event: Event) {
        self(event)
    }
}

/// How the engine holds its sink.
pub type Events = Arc<dyn EventSink>;

/// Drops every event.
#[derive(Debug, Default, Clone, Copy)]
pub struct NullSink;

impl EventSink for NullSink {
    fn emit(&self, _: Event) {}
}

/// Keeps every event in memory, in order. For tests and for callers that
/// poll.
#[derive(Debug, Default)]
pub struct MemorySink {
    events: Mutex<Vec<Event>>,
}

impl MemorySink {
    pub fn new() -> Self {
        Self::default()
    }

    /// Removes and returns every event so far.
    pub fn take(&self) -> Vec<Event> {
        std::mem::take(&mut *self.lock())
    }

    /// A copy of every event so far.
    pub fn snapshot(&self) -> Vec<Event> {
        self.lock().clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Event>> {
        // A panic while pushing cannot leave the Vec inconsistent.
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl EventSink for MemorySink {
    fn emit(&self, event: Event) {
        self.lock().push(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn failed(n: u32) -> Event {
        Event::PushFailed {
            remote: "origin".into(),
            kind: PushFailureKind::Unreachable,
            message: "no route".into(),
            head: None,
            last_pushed: None,
            consecutive_failures: n,
            retry_in: Duration::from_secs(120),
            stage: PushStage::Git,
        }
    }

    #[test]
    fn the_stage_shows_only_for_lfs_and_defaults_to_git() {
        assert!(
            failed(1)
                .to_string()
                .starts_with("push to origin failed (unreachable): ")
        );
        let mut lfs = failed(1);
        if let Event::PushFailed { stage, .. } = &mut lfs {
            *stage = PushStage::Lfs;
        }
        assert!(
            lfs.to_string()
                .starts_with("push to origin failed (unreachable, stage lfs): "),
            "{lfs}"
        );
        assert_eq!(PushStage::default(), PushStage::Git);
        assert_eq!(serde_json::to_string(&PushStage::Lfs).unwrap(), "\"lfs\"");
    }

    #[test]
    fn memory_sink_collects_in_order() {
        let sink = MemorySink::new();
        sink.emit(failed(1));
        sink.emit(failed(2));
        assert_eq!(sink.snapshot().len(), 2);
        assert_eq!(sink.take(), [failed(1), failed(2)]);
        assert!(sink.take().is_empty());
    }

    #[test]
    fn closure_sink_works() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let events: Events = {
            let seen = seen.clone();
            Arc::new(move |e: Event| seen.lock().unwrap().push(e.name()))
        };
        events.emit(failed(1));
        Arc::new(NullSink).emit(failed(1));
        assert_eq!(*seen.lock().unwrap(), ["push_failed"]);
    }

    #[test]
    fn only_success_is_not_an_alarm() {
        let ok = Event::PushSucceeded {
            remote: "r".into(),
            head: Oid::ZERO_SHA1,
            previous: None,
        };
        assert!(!ok.is_alarm());
        assert!(failed(1).is_alarm());
        let commit = Event::CommitFailed {
            error: "x".into(),
            backlog: 3,
        };
        assert!(commit.is_alarm());
        assert_eq!(commit.name(), "commit_failed");
    }

    #[test]
    fn push_failed_display_names_remote_kind_and_retry() {
        let text = failed(2).to_string();
        assert!(text.contains("origin"), "{text}");
        assert!(text.contains("(unreachable)"), "{text}");
        assert!(text.contains("2 consecutive"), "{text}");
        assert!(text.contains("retry in 120s"), "{text}");
    }

    #[test]
    fn failure_kind_serializes_as_its_label() {
        for kind in [
            PushFailureKind::Unreachable,
            PushFailureKind::Auth,
            PushFailureKind::UntrustedHost,
            PushFailureKind::NonFastForward,
            PushFailureKind::Refused,
            PushFailureKind::Local,
            PushFailureKind::TooLarge,
        ] {
            let json = serde_json::to_string(&kind).unwrap();
            assert_eq!(json, format!("\"{}\"", kind.as_str()));
            assert_eq!(kind.to_string(), kind.as_str());
        }
        assert_eq!(
            serde_json::to_string(&PushFailureKind::UntrustedHost).unwrap(),
            "\"untrusted_host\""
        );
    }
}
