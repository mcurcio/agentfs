//! Pushing the volume's refs to its remotes (design §11).
//!
//! Each configured remote gets a [`RemotePusher`] with its own repository
//! handle and its own persisted [`PushState`] in
//! `<state>/meta/push/<remote>.json`, so one failing remote never blocks or
//! colours another.
//!
//! A push sends only the volume's own refs (design §11.4): the history
//! branch and `refs/agentfs/<volume>/*` (and the forks it received, below),
//! as explicit `<oid>:<ref>` refspecs
//! on an anonymous remote, never forced. Remote configuration such as
//! `remote.<name>.push` is therefore never consulted, and the system,
//! global and XDG git config are not read at all
//! (see `history::init_libgit2`).
//!
//! Outcomes (design §11.3):
//!
//! - **Success** records the pushed head and clears failures and any
//!   divergence.
//! - **Non-fast-forward on the branch** means someone else wrote it. The
//!   local head is saved once per divergence under
//!   `refs/agentfs/<volume>/diverged/<UTC timestamp>`, which then follows
//!   the head by [`claim`] (F8 D3), and every attempt raises
//!   [`Event::PushFailed`] until an operator resolves it, with
//!   [`RemotePusher::push_with_lease`] or a new name.
//! - **The patch queue** writes only while the remote's branch is this
//!   worktree's, leased on it ([`QueuePush::branch_not_ours`]).
//! - **Forks received into this history** (a local-only source's forks,
//!   `fork::receive::received_refs`) follow a branch push that worked,
//!   create-only ([`RemotePusher::push_create_only`]'s rule): a remote
//!   added later gets them too, and a ref the remote holds at another oid
//!   fails the push, never overwritten.
//! - **Over the push size limit** (`agentfsMaxPushSize`, [`crate::push_size`]):
//!   measured from the branch the remote advertises (the push state can lag
//!   it), a backlog that does not fit is pushed in first-parent steps,
//!   raising [`Event::PushLimited`], with the next attempt at once; a single
//!   commit that does not fit fails as `too_large` before anything is sent.
//! - **Anything else** (unreachable, authentication, an untrusted host,
//!   refused) raises
//!   [`Event::PushFailed`] and backs off: the delay doubles from
//!   `pushInterval` up to `max(pushInterval, 15m)`.
//!
//! Authentication (design §11.1): the credentials callback gives one
//! credential per connection, for the push URL's host only. HTTP(S)
//! remotes get a username and password (token) from `agentfsCredentials`;
//! SSH remotes get only their private key from `agentfsSshKey`, and only
//! after their host key has been checked against `agentfsKnownHosts` in
//! libgit2's certificate callback, which runs before authentication. A
//! refused host key fails the push as `untrusted_host`, as does a TLS
//! certificate that fails verification.
//!
//! None of these affect recording or committing. A per-remote `flock` on
//! `<remote>.lock` keeps two processes (`agentfs push` and `agentfs run`)
//! from pushing the same remote at once. The lease push's scratch
//! repository, `<remote>.lease.git` beside it, holds the remote's history
//! (design §15): it is removed on every exit path, and whoever next takes
//! a push lock removes any a killed push left.
//!
//! libgit2's local transport updates a remote ref without compare-and-swap
//! once the ref exists, so a writer moving a local or `file://` remote
//! between libgit2's ref listing and its update can be overwritten. Smart
//! HTTP and SSH servers compare and swap.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[cfg(test)]
use git2::CredentialType;
use git2::{Error as GitError, Repository};
use git2::{ErrorClass, ErrorCode, Oid, PushOptions};
use opentelemetry::KeyValue;
use serde::{Deserialize, Serialize};

use crate::config::{RemoteSettings, Settings};
use crate::events::{Event, Events, PushFailureKind, PushStage};
use crate::file_lock::{FileLock, Mode};
use crate::history::init_libgit2;
use crate::lfs::upload::objects_to_upload;
use crate::lfs::{Direction, LfsError, ObjectCache, Transfer, UploadReport, transfer_for};
use crate::push_size::Step;
use crate::remote_auth::{AuthState, RemoteAuth};
use crate::remote_url::{self, Transport};
use crate::telemetry::{Telemetry, spans};
use crate::volume::StateDir;

mod claim;
mod stop;

pub use claim::{
    Claim, LeaseClaim, Recording, Stamp, StartBranchMoved, claim, lease_claim, start_branch_moved,
};
pub use stop::{PushPhase, PushStop};

/// The longest delay between retries of a failing remote, unless
/// `pushInterval` is longer.
pub const PUSH_BACKOFF_CAP: Duration = Duration::from_secs(900);

/// Why a remote could not be listed (F8 rule 3: the release decider asks
/// the remote, [`RemotePusher::branch_claim`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unreachable {
    pub kind: PushFailureKind,
    /// Scrubbed of any userinfo in the push URL.
    pub message: String,
}

/// What [`RemotePusher::push_with_lease`] did (F8 S2; runbooks:
/// divergence case A, history rewrite step 9).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeaseOutcome {
    /// The branch does not exist locally.
    NothingToPush,
    /// The remote already holds the head.
    UpToDate { head: Oid },
    /// The remote's branch was `from`, the lease, and is now the head.
    Pushed { from: Oid, to: Oid },
    /// The remote's branch is not the lease (`found`; `None`: no branch):
    /// nothing was written.
    LeaseBroken { found: Option<Oid> },
    /// The lease and the head share no commit: the remote's branch is
    /// another history, never replaced by this command (a rewrite that
    /// changes the root continues under a new name).
    Unrelated { lease: Oid, head: Oid },
    /// The remote's branch holds `commit`, which an agentfs recorder made
    /// and the head holds neither as is nor rewritten ([`lease_claim`]):
    /// this volume's newer history (divergence case B, a stale state area)
    /// or another worktree's. Never replaced: it continues under a new
    /// name.
    Recorded { commit: Oid },
    Failed {
        kind: PushFailureKind,
        message: String,
    },
    /// Another process holds this remote's push lock.
    Busy,
}

#[derive(Debug, thiserror::Error)]
pub enum PushError {
    #[error(transparent)]
    Git(#[from] GitError),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("no remote named {0:?} is configured")]
    UnknownRemote(String),
}

/// The last failure, as persisted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LastError {
    pub kind: PushFailureKind,
    pub message: String,
    /// Where it failed. Absent from state written before LFS existed.
    #[serde(default)]
    pub stage: PushStage,
}

/// An unresolved divergence (design §11.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Divergence {
    /// When it was first seen.
    pub since_ms: u64,
    /// The local head at the latest attempt.
    pub local_head: String,
    /// The remote branch at the latest attempt.
    pub remote_head: Option<String>,
    /// Where the local head was saved, once that push succeeded.
    pub diverged_ref: Option<String>,
    /// The name chosen for saving the head, recorded before the push and
    /// cleared once it is confirmed: a restart reuses it rather than
    /// saving the head again under a new name.
    #[serde(default)]
    pub pending_ref: Option<String>,
    /// The head the save under `diverged_ref` last reached.
    #[serde(default)]
    pub saved_head: Option<String>,
}

/// One remote's push bookkeeping, persisted across restarts.
///
/// It is bookkeeping only: a missing or unreadable file is treated as empty
/// and rewritten at the next attempt.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PushState {
    /// The last branch head the remote is known to have.
    pub last_pushed: Option<String>,
    pub last_attempt_ms: Option<u64>,
    pub last_success_ms: Option<u64>,
    pub consecutive_failures: u32,
    pub last_error: Option<LastError>,
    pub diverged: Option<Divergence>,
    /// The patch queue push's last failure (design §11.3, §12): kept apart
    /// from `last_error` so it never backs off or blocks the branch.
    pub queue_error: Option<LastError>,
    /// When the queue's refs were last fully pushed.
    pub queue_synced_ms: Option<u64>,
    /// The status commit then pushed, with its applied refs and inbox
    /// deletions: the queue push is skipped while it is still current.
    pub queue_synced_status: Option<String>,
    /// The start-branch alarm as the last attempt to this remote saw it, for `agentfs status`.
    #[serde(default)]
    pub start_moved: Option<StartMoved>,
}

/// The start-branch alarm as persisted ([`StartBranchMoved`], F8 R9).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartMoved {
    pub refname: String,
    pub pinned: String,
    pub tip: String,
}

impl PushState {
    pub fn last_pushed_oid(&self) -> Option<Oid> {
        self.last_pushed
            .as_deref()
            .and_then(|s| Oid::from_str(s).ok())
    }

    /// Reads the state at `path`; empty if it is missing or unreadable.
    pub fn load(path: &Path) -> Self {
        fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    /// Writes atomically: temporary file, fsync, rename, fsync directory.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        crate::fsutil::write_json_atomic(path, self)
    }
}

/// What one [`RemotePusher::push_once`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PushOutcome {
    /// The branch does not exist yet.
    NothingToPush,
    /// The remote already had every ref.
    UpToDate { head: Oid },
    Pushed {
        from: Option<Oid>,
        to: Oid,
        /// Set when the backlog was larger than the remote's
        /// `agentfsMaxPushSize`: only up to `to` was pushed, and this many
        /// first-parent commits are left for the next steps (design §11.3).
        limited: Option<usize>,
    },
    /// Someone else wrote the remote branch. `diverged_ref` is where the
    /// local head was saved, if that push has succeeded.
    Diverged {
        local_head: Oid,
        remote_head: Option<Oid>,
        diverged_ref: Option<String>,
    },
    Failed {
        kind: PushFailureKind,
        message: String,
        retry_in: Duration,
        /// The git push, or the LFS upload before it (design §7).
        stage: PushStage,
    },
    /// Another process holds this remote's push lock.
    Busy,
    /// The shutdown cancelled the attempt before it sent anything (design
    /// §11.2). Not a failure: no alarm, nothing saved, no backoff.
    Cancelled,
}

/// What the patch queue push of one attempt did (design §11.3, §12).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QueuePush {
    /// The status ref was pushed.
    pub status: bool,
    /// `applied/<n>` refs the remote did not have.
    pub applied: usize,
    /// `inbox/<n>` refs deleted on the remote.
    pub deleted: usize,
    /// `applied/<n>` refs held back because the branch push failed.
    pub held_back: bool,
    /// Why it failed, if it did.
    pub error: Option<(PushFailureKind, String)>,
    /// The branch the remote advertised in the queue push's connection when it was not this
    /// worktree's (Claim::Create or Claim::Diverged): nothing was written. None otherwise.
    pub branch_not_ours: Option<Claim>,
}

impl QueuePush {
    /// One line for `agentfs push`.
    pub fn summary(&self) -> String {
        if let Some(claim) = self.branch_not_ours {
            let why = match claim {
                Claim::Create => "the remote has no branch of this volume yet",
                _ => "the remote's branch is not on this volume's history",
            };
            return format!("nothing written: {why} (F8 rule 2)");
        }
        match &self.error {
            Some((kind, message)) => format!("failed ({kind}): {message}"),
            None => {
                let mut s = format!(
                    "status {}, {} applied ref(s) created, {} inbox ref(s) deleted",
                    if self.status { "pushed" } else { "unchanged" },
                    self.applied,
                    self.deleted
                );
                if self.held_back {
                    s.push_str(" (applied refs wait for the branch push)");
                }
                s
            }
        }
    }
}

impl PushOutcome {
    /// A push that sent only part of the backlog (design §11.3); the next
    /// step should follow at once.
    pub fn is_limited(&self) -> bool {
        matches!(
            self,
            Self::Pushed {
                limited: Some(_),
                ..
            }
        )
    }

    pub fn is_success(&self) -> bool {
        matches!(
            self,
            Self::NothingToPush | Self::UpToDate { .. } | Self::Pushed { .. }
        )
    }

    /// A stable label for spans (`agentfs.push.outcome`).
    pub fn label(&self) -> &'static str {
        match self {
            Self::NothingToPush => "nothing_to_push",
            Self::UpToDate { .. } => "up_to_date",
            Self::Pushed { .. } => "pushed",
            Self::Diverged { .. } => "diverged",
            Self::Failed { .. } => "failed",
            Self::Busy => "busy",
            Self::Cancelled => "cancelled",
        }
    }
}

/// One ref of [`RemotePusher::push_create_only`]: created on the remote
/// at `oid` if absent, never moved (design §11.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateOnlyRef {
    pub name: String,
    pub oid: Oid,
}

/// What [`RemotePusher::push_create_only`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CreateOnlyOutcome {
    /// Every ref is on the remote at exactly its oid (listed after the
    /// push).
    Held,
    /// Not yet: retried like any failed push (design §11.3).
    Failed {
        kind: PushFailureKind,
        message: String,
    },
    /// A ref is on the remote at another oid: someone else wrote it, and
    /// it is never overwritten.
    AnotherWriter { name: String, found: Oid },
}

/// Whether a lease and the head share a commit, and whether the commits
/// the lease push would drop are its to drop
/// ([`RemotePusher::push_with_lease`]).
enum LeaseCheck {
    /// They share a commit, and [`lease_claim`] lets the branch be
    /// replaced.
    Replace,
    Unrelated,
    /// [`LeaseClaim::Recorded`]: the remote's branch holds this commit, a
    /// recorder's, which the head lacks.
    Recorded(Oid),
    /// The remote's branch moved off the lease while it was fetched.
    Moved(Option<Oid>),
}

/// What one push of a divergence save did ([`RemotePusher::save_head`]).
enum Saved {
    /// The remote holds the head under the save's name.
    Held,
    /// The remote holds a commit under that name that is not on this
    /// history: nothing was sent.
    AnotherWriter,
    /// The remote refused the update.
    Refused(String),
    /// The push failed (unreachable, authentication, ...).
    Failed(String),
}

/// What libgit2 reported for one push call.
struct Attempt {
    result: Result<(), GitError>,
    /// The remote's ref list was read and the update commands computed.
    negotiated: bool,
    /// The credentials callback refused or ran out of attempts.
    auth_failed: bool,
    /// Why the SSH host key was refused, if it was: libgit2 replaces the
    /// callback's message with its own.
    host_key_failure: Option<String>,
    /// `(remote ref, its oid on the remote before the push)`.
    updates: Vec<(String, Oid)>,
    /// `(remote ref, the remote's rejection message)`.
    rejected: Vec<(String, String)>,
    /// The claim the push negotiation computed from the guarded ref's old
    /// value (F8 rule 2): the branch for a branch push, the `diverged/*`
    /// ref for a divergence save; `None` for other pushes.
    /// [`Claim::Diverged`] stopped the push before anything was sent.
    claim: Option<Claim>,
}

/// Pushes one remote. Owns its own repository handle, so each remote can
/// run on its own thread.
pub struct RemotePusher {
    repo: Repository,
    state_path: PathBuf,
    lock_path: PathBuf,
    volume: String,
    /// The volume's branch ref: the name pushed to on the remote.
    branch_ref: String,
    /// The local ref pushed there: the branch, or the change line for a
    /// remote with `agentfsPush = changeLine` ([`Settings::push_source_ref`],
    /// design §19.2 item 11). Claims and divergence are judged against it.
    source_ref: String,
    push_interval: Duration,
    remote: RemoteSettings,
    /// Credentials, SSH key and known hosts (design §11.1).
    auth: RemoteAuth,
    /// For resolving the LFS endpoint (design §7).
    settings: Settings,
    lfs: ObjectCache,
    /// The LFS upload transfer, resolved the first time a push has
    /// objects to upload.
    transfer: Option<Box<dyn Transfer>>,
    events: Events,
    telemetry: Telemetry,
    state: PushState,
    /// This remote is the patch remote: it gets the queue push.
    patch_remote: bool,
    /// What the last attempt's queue push did.
    queue: Option<QueuePush>,
    /// The last attempt pushed only part of the backlog (design §11.3):
    /// the next follows at once.
    step_pending: bool,
    /// Cancels its attempts at shutdown (design §11.2).
    stop: Arc<PushStop>,
    /// Runs at the end of a push's negotiation, once its update began: a
    /// test holds a push there.
    #[cfg(test)]
    after_negotiation: Option<Box<dyn Fn() + Send>>,
    /// Runs once a push's update ended, before its bookkeeping: a test
    /// holds a push there.
    #[cfg(test)]
    after_update: Option<Box<dyn Fn() + Send>>,
    /// Runs as [`RemotePusher::unpushed`] starts: a test holds a push
    /// thread there, deciding its final push.
    #[cfg(test)]
    before_unpushed: Option<Box<dyn Fn() + Send>>,
    /// Run by the service's push thread just before it reports its final
    /// push ([`RemotePusher::before_report`]).
    #[cfg(test)]
    before_report: Option<Box<dyn Fn() + Send>>,
    /// Runs between the queue push's listing and its pushes: a test moves
    /// the branch there.
    #[cfg(test)]
    after_queue_listing: Option<Box<dyn Fn() + Send>>,
    /// Runs in the lease check once the lease is fetched into the scratch
    /// repository: a test interrupts the push there.
    #[cfg(test)]
    in_lease_scratch: Option<Box<dyn Fn() + Send>>,
}

impl RemotePusher {
    /// A pusher for `remote`. Alarms go to `events`, which the caller has
    /// already wrapped with [`Telemetry::sink`] if telemetry is on; spans go
    /// to `telemetry` (design §13).
    pub fn open(
        state: &StateDir,
        settings: &Settings,
        remote: &RemoteSettings,
        events: Events,
        telemetry: &Telemetry,
    ) -> Result<Self, PushError> {
        init_libgit2();
        let repo = Repository::open_bare(state.git())?;
        fs::create_dir_all(state.push_dir())?;
        let state_path = state.push_state(&remote.name);
        Ok(Self {
            repo,
            state: PushState::load(&state_path),
            state_path,
            lock_path: state.push_lock(&remote.name),
            volume: settings.volume.clone(),
            branch_ref: settings.branch_ref.clone(),
            source_ref: settings.push_source_ref(remote),
            push_interval: settings.push_interval,
            auth: RemoteAuth::new(remote),
            remote: remote.clone(),
            settings: settings.clone(),
            lfs: ObjectCache::at(state.lfs())?,
            transfer: None,
            events,
            telemetry: telemetry.clone(),
            patch_remote: settings
                .patch_remote()
                .is_some_and(|p| p.name == remote.name),
            queue: None,
            step_pending: false,
            stop: Arc::default(),
            #[cfg(test)]
            after_negotiation: None,
            #[cfg(test)]
            after_update: None,
            #[cfg(test)]
            before_unpushed: None,
            #[cfg(test)]
            before_report: None,
            #[cfg(test)]
            after_queue_listing: None,
            #[cfg(test)]
            in_lease_scratch: None,
        })
    }

    /// What cancels this pusher's attempts (design §11.2).
    pub fn stop_handle(&self) -> Arc<PushStop> {
        self.stop.clone()
    }

    /// Runs `hook` at the end of each push's negotiation, once its update
    /// began.
    #[cfg(test)]
    pub(crate) fn set_after_negotiation(&mut self, hook: impl Fn() + Send + 'static) {
        self.after_negotiation = Some(Box::new(hook));
    }

    /// Runs `hook` once each push's update ended, before its bookkeeping.
    #[cfg(test)]
    pub(crate) fn set_after_update(&mut self, hook: impl Fn() + Send + 'static) {
        self.after_update = Some(Box::new(hook));
    }

    /// Runs `hook` as each [`RemotePusher::unpushed`] starts.
    #[cfg(test)]
    pub(crate) fn set_before_unpushed(&mut self, hook: impl Fn() + Send + 'static) {
        self.before_unpushed = Some(Box::new(hook));
    }

    /// Runs `hook` where [`RemotePusher::before_report`] is called.
    #[cfg(test)]
    pub(crate) fn set_before_report(&mut self, hook: impl Fn() + Send + 'static) {
        self.before_report = Some(Box::new(hook));
    }

    /// The service's push thread is about to report its final push.
    #[cfg(test)]
    pub(crate) fn before_report(&self) {
        if let Some(hook) = &self.before_report {
            hook();
        }
    }

    /// What the last [`RemotePusher::push_once`]'s queue push did; `None`
    /// when there was none (not the patch remote, nothing new to push).
    pub fn queue(&self) -> Option<&QueuePush> {
        self.queue.as_ref()
    }

    pub fn name(&self) -> &str {
        &self.remote.name
    }

    pub fn state(&self) -> &PushState {
        &self.state
    }

    /// How long to wait before the next attempt: `pushInterval` after a
    /// success, the backoff after failures, and none after a push that
    /// sent only part of the backlog (design §11.3).
    pub fn next_delay(&self) -> Duration {
        if self.step_pending {
            return Duration::ZERO;
        }
        backoff(
            self.push_interval,
            self.state.consecutive_failures,
            PUSH_BACKOFF_CAP,
        )
    }

    /// Commits on the branch the remote is not known to have; `None` if
    /// the branch does not exist yet.
    pub fn unpushed(&self) -> Result<Option<usize>, GitError> {
        #[cfg(test)]
        if let Some(hook) = &self.before_unpushed {
            hook();
        }
        unpushed(&self.repo, &self.source_ref, self.state.last_pushed_oid())
            .map(|w| w.map(|w| w.count))
    }

    /// One push attempt. Never fails: every problem becomes an outcome and,
    /// except for [`PushOutcome::Busy`], an event.
    ///
    /// Runs in an `agentfs.push` span carrying the remote, the last pushed
    /// commit, the head, the outcome and, on failure, the reason (design
    /// §13).
    pub fn push_once(&mut self) -> PushOutcome {
        let span = self.telemetry.span(spans::PUSH, || {
            let mut attrs = vec![KeyValue::new("agentfs.remote", self.remote.name.clone())];
            if let Some(last) = PushState::load(&self.state_path).last_pushed {
                attrs.push(KeyValue::new("agentfs.push.previous", last));
            }
            attrs
        });
        let mut head = None;
        let mut lfs = None;
        self.queue = None;
        let outcome = self.push_once_inner(&mut head, &mut lfs);
        if let Some(queue) = &self.queue {
            let label = if queue.error.is_some() {
                "failed"
            } else if queue.branch_not_ours.is_some() {
                "branch_not_ours"
            } else {
                "pushed"
            };
            span.set(KeyValue::new("agentfs.push.queue", label));
        }
        span.set(KeyValue::new("agentfs.push.outcome", outcome.label()));
        if let Some(report) = lfs {
            span.set(KeyValue::new("agentfs.lfs.objects", report.objects as i64));
            span.set(KeyValue::new(
                "agentfs.lfs.uploaded",
                report.uploaded as i64,
            ));
            span.set(KeyValue::new("agentfs.lfs.bytes", report.bytes as i64));
        }
        if let Some(head) = head {
            span.set_with(|| KeyValue::new("agentfs.push.head", head.to_string()));
        }
        match &outcome {
            PushOutcome::Failed {
                kind,
                message,
                stage,
                ..
            } => {
                span.set(KeyValue::new("agentfs.push.reason", kind.as_str()));
                span.set(KeyValue::new("agentfs.push.stage", stage.as_str()));
                span.error(message);
            }
            PushOutcome::Diverged { diverged_ref, .. } => {
                span.set(KeyValue::new(
                    "agentfs.push.reason",
                    PushFailureKind::NonFastForward.as_str(),
                ));
                if let Some(saved) = diverged_ref {
                    span.set(KeyValue::new("agentfs.push.diverged_ref", saved.clone()));
                }
                span.error("the remote branch has diverged");
            }
            _ => {}
        }
        span.end();
        outcome
    }

    /// [`RemotePusher::push_once`], setting `head_out` to the branch head
    /// once it is known and `lfs_out` to what the LFS upload did, if the
    /// unpushed commits reference LFS objects.
    fn push_once_inner(
        &mut self,
        head_out: &mut Option<Oid>,
        lfs_out: &mut Option<UploadReport>,
    ) -> PushOutcome {
        let _lock = match self.lock() {
            Ok(Some(lock)) => lock,
            Ok(None) => return PushOutcome::Busy,
            Err(e) => {
                let message = format!("locking {}: {e}", self.lock_path.display());
                return self.failed(PushFailureKind::Local, message, None);
            }
        };
        // Another process may have pushed since this one last looked.
        self.state = PushState::load(&self.state_path);
        self.state.last_attempt_ms = Some(now_ms());
        self.step_pending = false;
        let outcome = self.branch_push(head_out, lfs_out);
        let limited = outcome.is_limited();
        let outcome = if outcome.is_success() && !limited {
            self.received_push(outcome, *head_out)
        } else {
            outcome
        };
        self.step_pending = limited;
        // The queue push follows every branch attempt, failed or not
        // (design §12): the status ref reaches the remote even while the
        // branch cannot; applied refs wait for a branch push that worked,
        // and for the last step of a limited one (design §11.3).
        if self.patch_remote {
            self.queue = self.queue_push(outcome.is_success() && !limited);
        }
        outcome
    }

    /// The refs of the forks received into this history
    /// ([`crate::fork::receive::received_refs`]), pushed create-only after
    /// a branch push that worked (`outcome`), with the push lock held: a
    /// local-only source's forks reach a remote added later (F8 S3). A
    /// ref the remote holds at another oid is never overwritten and fails
    /// the push, alarmed and retried like any failure, as does a push
    /// that did not take. `outcome` stands when there is none, or all are
    /// held.
    fn received_push(&mut self, outcome: PushOutcome, head: Option<Oid>) -> PushOutcome {
        let refs = match self.received() {
            Ok(refs) if refs.is_empty() => return outcome,
            Ok(refs) => refs,
            Err(e) => {
                let message = format!("reading the received forks' refs: {}", e.message());
                return self.failed(PushFailureKind::Local, message, head);
            }
        };
        match self.create_only_locked(&refs) {
            CreateOnlyOutcome::Held => outcome,
            CreateOnlyOutcome::AnotherWriter { name, found } => {
                let message = format!(
                    "{name}, a fork received into this history, is at {found} on the remote, \
                     not at its own commit: another writer's, never overwritten"
                );
                self.failed(PushFailureKind::Refused, message, head)
            }
            CreateOnlyOutcome::Failed { kind, message } => self.failed(
                kind,
                format!("pushing the received forks' refs: {message}"),
                head,
            ),
        }
    }

    /// [`crate::fork::receive::received_refs`] of this repository.
    fn received(&self) -> Result<Vec<CreateOnlyRef>, GitError> {
        Ok(
            crate::fork::receive::received_refs(&self.repo, &self.volume)?
                .into_iter()
                .map(|(name, oid)| CreateOnlyRef { name, oid })
                .collect(),
        )
    }

    /// The first ref of a fork received into this history
    /// ([`crate::fork::receive::received_refs`]) that this remote does not
    /// list at its commit, in one listing connection; `None` when the
    /// remote holds them all, or there are none (no connection then). For
    /// the release decider, beside [`RemotePusher::branch_claim`]: never
    /// pushes, never takes the push lock.
    pub fn received_lacking(&self) -> Result<Option<String>, Unreachable> {
        let local = |message: String| Unreachable {
            kind: PushFailureKind::Local,
            message,
        };
        let refs = self
            .received()
            .map_err(|e| local(format!("reading the received forks' refs: {}", e.message())))?;
        if refs.is_empty() {
            return Ok(None);
        }
        let listed = self
            .list_remote_refs(|name| refs.iter().any(|r| r.name == name))
            .map_err(|(kind, message)| Unreachable {
                kind,
                message: scrub_userinfo(&message, self.remote.push_url()),
            })?;
        Ok(refs
            .into_iter()
            .find(|r| listed.get(&r.name) != Some(&r.oid))
            .map(|r| r.name))
    }

    /// The branch push of [`RemotePusher::push_once_inner`], with the push
    /// lock held and the state loaded.
    fn branch_push(
        &mut self,
        head_out: &mut Option<Oid>,
        lfs_out: &mut Option<UploadReport>,
    ) -> PushOutcome {
        let head = match self.repo.refname_to_id(&self.source_ref) {
            Ok(head) => head,
            Err(e) if e.code() == ErrorCode::NotFound => return PushOutcome::NothingToPush,
            Err(e) => return self.failed(PushFailureKind::Local, e.to_string(), None),
        };
        *head_out = Some(head);
        self.check_start(head);
        // A maximum push size (design §11.3): measured before pushing, from
        // the branch the remote advertises.
        let mut step = None;
        if let Some(limit) = self.remote.max_push_size {
            let base = self.measure_base(head);
            match crate::push_size::largest_step(&self.repo, head, base, limit) {
                Ok(Step::All) => {}
                Ok(Step::UpTo {
                    commit,
                    commits_left,
                }) => step = Some((commit, commits_left, limit)),
                Ok(Step::None {
                    first,
                    size_at_least,
                }) => {
                    let message = match base {
                        Some(base)
                            if !self.repo.graph_descendant_of(head, base).unwrap_or(false)
                                && base != head =>
                        {
                            format!(
                                "the commits since {base} take at least {size_at_least} bytes, \
                                 over remote.{name}.agentfsMaxPushSize ({limit}), and cannot \
                                 be pushed in steps: {base}, the last commit pushed, is not \
                                 an ancestor of the head; raise the limit",
                                name = self.remote.name
                            )
                        }
                        _ => format!(
                            "commit {first} alone takes at least {size_at_least} bytes to \
                             push, over remote.{name}.agentfsMaxPushSize ({limit}); raise the \
                             limit, or move large files to LFS from now on (see the \
                             stuck-pushes runbook)",
                            name = self.remote.name
                        ),
                    };
                    return self.failed(PushFailureKind::TooLarge, message, Some(head));
                }
                Err(e) => {
                    let message = format!("measuring the push: {}", e.message());
                    return self.failed(PushFailureKind::Local, message, Some(head));
                }
            }
        }
        if let Some((commit, commits_left, limit)) = step {
            return self.step_push(head, commit, commits_left, limit, lfs_out);
        }
        let refspecs = match self.refspecs(head) {
            Ok(refspecs) => refspecs,
            Err(e) => return self.failed(PushFailureKind::Local, e.to_string(), Some(head)),
        };
        // LFS objects first: a ref is never pushed before the objects its
        // commits reference (design §7).
        match self.upload_lfs(head) {
            Ok(report) => *lfs_out = report,
            Err(e) => return self.failed_at(e.kind, e.message, Some(head), PushStage::Lfs),
        }
        if let Err(e) = crate::failpoints::hit("push.after_lfs_upload") {
            return self.failed_at(
                PushFailureKind::Local,
                e.to_string(),
                Some(head),
                PushStage::Lfs,
            );
        }
        let attempt = self.attempt_branch(&refspecs, head);
        self.conclude(head, &refspecs, attempt)
    }

    /// The start-branch check (F8 R9): when this remote is the volume's
    /// start remote, lists the start branch and asks
    /// [`start_branch_moved`], with `on_history` true for a commit known
    /// here that is `head`, or an `accepted/*` tip, or an ancestor of
    /// either. Raises [`Event::StartBranchMoved`] and keeps it in the push
    /// state while it holds; clears it otherwise. A listing that fails
    /// leaves the last answer: the push reports the remote's failure.
    fn check_start(&mut self, head: Oid) {
        let Some(start) = self.settings.start.clone() else {
            return;
        };
        if start.remote != self.remote.name {
            return;
        }
        let refname = start.refname();
        let Ok(listed) = self.list_remote_refs(|name| name == refname) else {
            return;
        };
        let mut tips = vec![head];
        let accepted = crate::fork::identity::accepted_prefix(&self.volume);
        if let Ok(refs) = self.repo.references_glob(&format!("{accepted}*")) {
            tips.extend(refs.flatten().filter_map(|r| r.target()));
        }
        let repo = &self.repo;
        let on_history = |tip: Oid| {
            repo.odb().is_ok_and(|odb| odb.exists(tip))
                && tips
                    .iter()
                    .any(|t| *t == tip || repo.graph_descendant_of(*t, tip).unwrap_or(false))
        };
        match start_branch_moved(&start, listed.get(&refname).copied(), on_history) {
            Some(moved) => {
                self.state.start_moved = Some(StartMoved {
                    refname: moved.refname.clone(),
                    pinned: moved.pinned.to_string(),
                    tip: moved.tip.to_string(),
                });
                self.events.emit(Event::StartBranchMoved {
                    remote: moved.remote,
                    refname: moved.refname,
                    pinned: moved.pinned,
                    tip: moved.tip,
                });
            }
            None => self.state.start_moved = None,
        }
    }

    /// Whether `oid` is in this repository and is `head` or an ancestor of
    /// it: [`claim`]'s `on_history`.
    fn on_history(&self, head: Oid, oid: Oid) -> bool {
        self.repo.odb().is_ok_and(|odb| odb.exists(oid))
            && (oid == head || self.repo.graph_descendant_of(head, oid).unwrap_or(false))
    }

    /// One step of a backlog larger than the remote's `agentfsMaxPushSize`
    /// (design §11.3): the branch alone, up to `commit`, with the LFS
    /// objects of that step only. Diverged refs wait for the full push.
    fn step_push(
        &mut self,
        head: Oid,
        commit: Oid,
        commits_left: usize,
        limit: u64,
        lfs_out: &mut Option<UploadReport>,
    ) -> PushOutcome {
        match self.upload_lfs(commit) {
            Ok(report) => *lfs_out = report,
            Err(e) => return self.failed_at(e.kind, e.message, Some(commit), PushStage::Lfs),
        }
        let refspecs = vec![format!("{commit}:{}", self.branch_ref)];
        let attempt = self.attempt_branch(&refspecs, commit);
        let outcome = self.conclude(commit, &refspecs, attempt);
        match outcome {
            PushOutcome::Pushed { from, to, .. } => {
                self.events.emit(Event::PushLimited {
                    remote: self.remote.name.clone(),
                    limit,
                    pushed: to,
                    head,
                    commits_left,
                });
                PushOutcome::Pushed {
                    from,
                    to,
                    limited: Some(commits_left),
                }
            }
            // The remote already had the step (pushed by another process
            // since): the next step follows at once too.
            PushOutcome::UpToDate { head: to } => PushOutcome::Pushed {
                from: Some(to),
                to,
                limited: Some(commits_left),
            },
            other => other,
        }
    }

    /// The outcome of the branch push `attempt` of `refspecs`, whose
    /// branch head is `head`.
    fn conclude(&mut self, head: Oid, refspecs: &[String], attempt: Attempt) -> PushOutcome {
        let remote_branch = attempt
            .updates
            .iter()
            .find(|(name, _)| *name == self.branch_ref)
            .map(|(_, oid)| *oid)
            .filter(|oid| !oid.is_zero());

        // Rule 2: the negotiation stopped the push before anything was
        // sent, because the remote's branch is not on this history.
        if attempt.claim == Some(Claim::Diverged) {
            let message = format!(
                "{}: the remote holds {}, which is not on this volume's history (another \
                 worktree wrote it); nothing was sent",
                self.branch_ref,
                remote_branch.map_or_else(|| "an unknown commit".to_owned(), |o| o.to_string())
            );
            return self.diverged(head, remote_branch, message);
        }

        match &attempt.result {
            Ok(()) if attempt.rejected.is_empty() => {
                let up_to_date = attempt.updates.iter().all(|(name, remote)| {
                    refspecs
                        .iter()
                        .any(|spec| *spec == format!("{remote}:{name}"))
                });
                self.succeeded(head, remote_branch, up_to_date)
            }
            Ok(()) => {
                let message = attempt
                    .rejected
                    .iter()
                    .map(|(name, msg)| format!("{name}: {msg}"))
                    .collect::<Vec<_>>()
                    .join("; ");
                let non_ff =
                    |msg: &str| msg.contains("non-fast-forward") || msg.contains("fetch first");
                let branch_non_ff = attempt
                    .rejected
                    .iter()
                    .any(|(name, msg)| *name == self.branch_ref && non_ff(msg));
                if branch_non_ff && self.is_divergence(head, remote_branch) {
                    self.diverged(head, remote_branch, message)
                } else if attempt.rejected.iter().any(|(_, msg)| non_ff(msg)) {
                    self.failed(PushFailureKind::NonFastForward, message, Some(head))
                } else {
                    self.failed(PushFailureKind::Refused, message, Some(head))
                }
            }
            Err(e) if e.code() == ErrorCode::NotFastForward => {
                let message = e.message().to_owned();
                if self.is_divergence(head, remote_branch) {
                    self.diverged(head, remote_branch, message)
                } else {
                    self.failed(PushFailureKind::NonFastForward, message, Some(head))
                }
            }
            Err(e) => {
                if let Some(message) = attempt.host_key_failure {
                    return self.failed(PushFailureKind::UntrustedHost, message, Some(head));
                }
                let kind = if attempt.auth_failed {
                    PushFailureKind::Auth
                } else {
                    classify(e, attempt.negotiated)
                };
                self.failed(kind, e.message().to_owned(), Some(head))
            }
        }
    }

    /// Uploads the LFS objects that the commits the remote lacks reference
    /// (design §7), before any ref is pushed. `None` when they reference
    /// none: then no LFS endpoint is resolved or contacted. Only the
    /// objects the server lacks must be in the local cache: an object it
    /// already has needs no local copy (after `agentfs lfs prune`).
    fn upload_lfs(&mut self, head: Oid) -> Result<Option<UploadReport>, LfsError> {
        let objects = objects_to_upload(&self.repo, head, self.state.last_pushed_oid())?;
        if objects.is_empty() {
            return Ok(None);
        }
        // This remote's own object limit, before anything is sent: a
        // limit set after the objects were committed (design §7, §11.3).
        if let Some(limit) = self.remote.max_lfs_object_size {
            let over: Vec<String> = objects
                .iter()
                .filter(|(p, _)| p.size > limit)
                .map(|(p, path)| format!("{path} ({}, {} bytes)", p.oid, p.size))
                .collect();
            if !over.is_empty() {
                return Err(LfsError::new(
                    PushFailureKind::TooLarge,
                    format!(
                        "{} LFS object(s) the unpushed commits reference are over \
                         remote.{name}.agentfsMaxLfsObjectSize ({limit} bytes), so nothing \
                         was uploaded: {}; raise the limit if the server takes them, or \
                         remove them from history (see the history-rewrite runbook)",
                        over.len(),
                        over.iter().take(5).cloned().collect::<Vec<_>>().join(", "),
                        name = self.remote.name
                    ),
                ));
            }
        }
        if self.transfer.is_none() {
            self.transfer = Some(transfer_for(
                &self.settings,
                &self.remote,
                Direction::Upload,
            )?);
        }
        let transfer = self.transfer.as_ref().expect("resolved just above");
        let pointers: Vec<_> = objects.iter().map(|(p, _)| *p).collect();
        transfer
            .upload(&pointers, &self.lfs, &self.branch_ref)
            .map(Some)
            .map_err(|mut e| {
                if !e.missing.is_empty() {
                    // Name the paths, which the transfer does not know.
                    let named: Vec<String> = objects
                        .iter()
                        .filter(|(p, _)| e.missing.contains(&p.oid))
                        .take(5)
                        .map(|(p, path)| format!("{path} ({})", p.oid))
                        .collect();
                    e.message = format!(
                        "{} LFS object(s) that remote {} lacks are missing from the local \
                         cache (<state>/lfs/objects), so they cannot be uploaded: {}; run \
                         `agentfs lfs fetch --all` to bring them back from another remote",
                        e.missing.len(),
                        self.remote.name,
                        named.join(", ")
                    );
                }
                e
            })
    }

    /// One push of `refs`, each created only where the remote lacks it
    /// (never `+`), under this remote's push lock, with the same
    /// authentication and proxy as the branch push and the LFS objects
    /// their commits reference uploaded first; then a listing that checks
    /// each is at exactly its oid (design §11.5). A ref the remote holds at
    /// another oid, before or after the push, is
    /// [`CreateOnlyOutcome::AnotherWriter`] and is left alone. Never
    /// touches the branch or the queue refs, nor this remote's push state.
    pub fn push_create_only(&mut self, refs: &[CreateOnlyRef]) -> CreateOnlyOutcome {
        let url = self.remote.push_url().to_owned();
        let failed = |kind, message: String| CreateOnlyOutcome::Failed {
            kind,
            message: scrub_userinfo(&message, &url),
        };
        let _lock = match self.lock() {
            Ok(Some(lock)) => lock,
            Ok(None) => {
                return failed(
                    PushFailureKind::Local,
                    "another process is pushing this remote".into(),
                );
            }
            Err(e) => {
                let message = format!("locking {}: {e}", self.lock_path.display());
                return failed(PushFailureKind::Local, message);
            }
        };
        self.create_only_locked(refs)
    }

    /// [`RemotePusher::push_create_only`] with this remote's push lock
    /// held.
    fn create_only_locked(&mut self, refs: &[CreateOnlyRef]) -> CreateOnlyOutcome {
        let url = self.remote.push_url().to_owned();
        let failed = |kind, message: String| CreateOnlyOutcome::Failed {
            kind,
            message: scrub_userinfo(&message, &url),
        };
        let wanted = |name: &str| refs.iter().any(|r| r.name == name);
        let listed = match self.list_remote_refs(wanted) {
            Ok(listed) => listed,
            Err((kind, message)) => return failed(kind, message),
        };
        if let Some(other) = another_writer(refs, &listed) {
            return other;
        }
        let missing: Vec<&CreateOnlyRef> = refs
            .iter()
            .filter(|r| listed.get(&r.name) != Some(&r.oid))
            .collect();
        if missing.is_empty() {
            return CreateOnlyOutcome::Held;
        }
        for r in &missing {
            if let Err(e) = self.upload_lfs(r.oid) {
                return failed(e.kind, e.message);
            }
        }
        let refspecs: Vec<String> = missing
            .iter()
            .map(|r| format!("{}:{}", r.oid, r.name))
            .collect();
        let attempt = self.attempt(&refspecs, &[]);
        let pushed = self.attempt_error(&attempt);
        let listed = match self.list_remote_refs(wanted) {
            Ok(listed) => listed,
            Err((kind, message)) => {
                let (kind, message) = pushed.unwrap_or((kind, message));
                return failed(kind, message);
            }
        };
        if let Some(other) = another_writer(refs, &listed) {
            return other;
        }
        if refs.iter().all(|r| listed.get(&r.name) == Some(&r.oid)) {
            return CreateOnlyOutcome::Held;
        }
        let (kind, message) = pushed.unwrap_or((
            PushFailureKind::Refused,
            "the remote accepted the push but does not list every ref".into(),
        ));
        failed(kind, message)
    }

    /// What this remote advertises for the volume's branch, judged
    /// against `head` by [`claim`] (F8 S1, rule 3): one listing
    /// connection, with this remote's authentication and proxy; never
    /// pushes, never takes the push lock, never touches the push state.
    /// [`Claim::UpToDate`] is the only answer the release decider counts
    /// as held.
    pub fn branch_claim(&self, head: Oid) -> Result<Claim, Unreachable> {
        let branch = self.branch_ref.as_str();
        let listed = self
            .list_remote_refs(|name| name == branch)
            .map_err(|(kind, message)| Unreachable {
                kind,
                message: scrub_userinfo(&message, self.remote.push_url()),
            })?;
        Ok(claim(listed.get(branch).copied(), head, |oid| {
            self.on_history(head, oid)
        }))
    }

    /// Replaces this worktree's own branch on the remote with the local
    /// head, leased on `lease`: the value the operator saw on the remote
    /// (F8 S2; design §15). Under this remote's push lock, not the state
    /// lock. `lease` is fetched from the remote when it is not in this
    /// repository; the push is refused ([`LeaseOutcome::Unrelated`]) when
    /// `lease` and the head share no commit, refused
    /// ([`LeaseOutcome::Recorded`]) when the commits it would drop hold
    /// history an agentfs recorder wrote that the head lacks
    /// ([`lease_claim`]: divergence case B, F8 "one branch per worktree"),
    /// and stopped in libgit2's push negotiation
    /// ([`LeaseOutcome::LeaseBroken`]) when the remote's old value is not
    /// `lease`. Only `branch_ref`, never another ref; the
    /// `diverged/*` refs stay. Success clears the divergence in the push
    /// state and is recorded like any push.
    pub fn push_with_lease(&mut self, lease: Oid) -> LeaseOutcome {
        let url = self.remote.push_url().to_owned();
        let failed = |kind, message: String| LeaseOutcome::Failed {
            kind,
            message: scrub_userinfo(&message, &url),
        };
        let _lock = match self.lock() {
            Ok(Some(lock)) => lock,
            Ok(None) => return LeaseOutcome::Busy,
            Err(e) => {
                let message = format!("locking {}: {e}", self.lock_path.display());
                return failed(PushFailureKind::Local, message);
            }
        };
        self.state = PushState::load(&self.state_path);
        let head = match self.repo.refname_to_id(&self.source_ref) {
            Ok(head) => head,
            Err(e) if e.code() == ErrorCode::NotFound => return LeaseOutcome::NothingToPush,
            Err(e) => return failed(PushFailureKind::Local, e.message().to_owned()),
        };
        let branch = self.branch_ref.clone();
        let found = match self.list_remote_refs(|name| name == branch) {
            Ok(listed) => listed.get(&branch).copied(),
            Err((kind, message)) => return failed(kind, message),
        };
        if found == Some(head) {
            return self.leased(head, found, true);
        }
        if found != Some(lease) {
            return LeaseOutcome::LeaseBroken { found };
        }
        match self.lease_check(lease, head) {
            Ok(LeaseCheck::Replace) => {}
            Ok(LeaseCheck::Unrelated) => return LeaseOutcome::Unrelated { lease, head },
            Ok(LeaseCheck::Recorded(commit)) => return LeaseOutcome::Recorded { commit },
            Ok(LeaseCheck::Moved(found)) => return LeaseOutcome::LeaseBroken { found },
            Err((kind, message)) => return failed(kind, message),
        }
        // LFS objects first: a ref is never pushed before the objects its
        // commits reference (design §7).
        if let Err(e) = self.upload_lfs(head) {
            return failed(e.kind, e.message);
        }
        // Forced, so libgit2 sends it although the remote's branch is not
        // an ancestor; the lease is checked in the negotiation, from the
        // old value the remote reports in this connection, and by the
        // smart protocol's compare-and-swap on it.
        let attempt = self.attempt(&[format!("+{head}:{branch}")], &[(branch.clone(), lease)]);
        let now = attempt
            .updates
            .iter()
            .find(|(name, _)| *name == branch)
            .map(|(_, oid)| *oid);
        if let Some(now) = now
            && now != lease
        {
            return LeaseOutcome::LeaseBroken {
                found: Some(now).filter(|o| !o.is_zero()),
            };
        }
        match self.attempt_error(&attempt) {
            None => self.leased(head, Some(lease), false),
            Some((kind, message)) => failed(kind, message),
        }
    }

    /// A lease push that left the remote's branch at `head`: recorded like
    /// any successful push, which clears the divergence.
    fn leased(&mut self, head: Oid, previous: Option<Oid>, up_to_date: bool) -> LeaseOutcome {
        match self.succeeded(head, previous, up_to_date) {
            PushOutcome::UpToDate { head } => LeaseOutcome::UpToDate { head },
            PushOutcome::Pushed { from, to, .. } => LeaseOutcome::Pushed {
                from: from.unwrap_or(head),
                to,
            },
            PushOutcome::Failed { kind, message, .. } => LeaseOutcome::Failed { kind, message },
            other => LeaseOutcome::Failed {
                kind: PushFailureKind::Local,
                message: format!("unexpected outcome {}", other.label()),
            },
        }
    }

    /// Whether `lease` and `head` share a commit, and then [`lease_claim`]
    /// on the commits only `lease` reaches against those only `head`
    /// reaches. A lease not in this
    /// repository is fetched first, with the branch the remote advertises
    /// (it is the lease, or the lease is broken), into a scratch repository
    /// beside the push lock that borrows this repository's objects
    /// (`objects/info/alternates`) and is removed afterwards: the state
    /// area never takes in the remote's objects, such as the old history a
    /// rewrite removed (history-rewrite runbook, step 9).
    fn lease_check(&self, lease: Oid, head: Oid) -> Result<LeaseCheck, (PushFailureKind, String)> {
        let local = |e: GitError| (PushFailureKind::Local, e.message().to_owned());
        let related = |repo: &Repository| {
            match repo.merge_base(head, lease) {
                Ok(_) => {}
                Err(e) if e.code() == ErrorCode::NotFound => return Ok(LeaseCheck::Unrelated),
                Err(e) => return Err(local(e)),
            }
            let dropped = commits_beyond(repo, lease, head).map_err(local)?;
            // The head's side is walked only when a dropped commit is a
            // recorder's: case A drops none, and its walk would be wasted.
            let kept = if dropped.iter().any(|(_, recording)| recording.is_some()) {
                commits_beyond(repo, head, lease)
                    .map_err(local)?
                    .into_iter()
                    .filter_map(|(_, recording)| recording)
                    .collect()
            } else {
                std::collections::HashSet::new()
            };
            Ok(match lease_claim(dropped, &kept) {
                LeaseClaim::Replace => LeaseCheck::Replace,
                LeaseClaim::Recorded(commit) => LeaseCheck::Recorded(commit),
            })
        };
        if self.repo.odb().is_ok_and(|odb| odb.exists(lease)) {
            return related(&self.repo);
        }
        let guard = LeaseScratch::new(lease_scratch(&self.lock_path));
        let scratch = guard.path();
        (|| {
            Repository::init_bare(scratch).map_err(local)?;
            let objects = fs::canonicalize(self.repo.path().join("objects"))
                .map_err(|e| (PushFailureKind::Local, e.to_string()))?;
            fs::write(
                scratch.join("objects/info/alternates"),
                format!("{}\n", objects.display()),
            )
            .map_err(|e| (PushFailureKind::Local, e.to_string()))?;
            let repo = Repository::open_bare(scratch).map_err(local)?;
            // What the fetch negotiates from: the remote sends only what
            // this history lacks.
            repo.reference("refs/heads/head", head, true, "agentfs lease check")
                .map_err(local)?;
            let tip = crate::fork::start::fetch_branch(&repo, &self.remote, &self.branch_ref)?;
            #[cfg(test)]
            if let Some(hook) = &self.in_lease_scratch {
                hook();
            }
            if tip != Some(lease) {
                return Ok(LeaseCheck::Moved(tip));
            }
            related(&repo)
        })()
    }

    /// This remote's push lock file: held while a push runs, and by a
    /// repack ([`crate::repack`]) so it never deletes a pack a push reads.
    pub fn lock_path(&self) -> &Path {
        &self.lock_path
    }

    /// Takes this remote's push lock without waiting, then sweeps the lease
    /// scratch repositories a killed push left ([`sweep_lease_scratch`]).
    fn lock(&self) -> io::Result<Option<FileLock>> {
        let Some(lock) = FileLock::try_take(&self.lock_path, Mode::Exclusive)? else {
            return Ok(None);
        };
        sweep_lease_scratch(&self.lock_path);
        Ok(Some(lock))
    }

    /// `<oid>:<ref>` for the branch and every
    /// `refs/agentfs/<volume>/diverged/*` and `accepted/*` ref (F8 D2),
    /// never forced: those two never move, so they are created where the
    /// remote lacks them and otherwise left as they are. The patch queue's
    /// refs have their own push ([`RemotePusher::queue_push`]).
    fn refspecs(&self, head: Oid) -> Result<Vec<String>, GitError> {
        let mut refspecs = vec![format!("{head}:{}", self.branch_ref)];
        for glob in [
            format!("refs/agentfs/{}/diverged/*", self.volume),
            format!("{}*", crate::fork::identity::accepted_prefix(&self.volume)),
        ] {
            for reference in self.repo.references_glob(&glob)? {
                let reference = reference?;
                let (Ok(name), Ok(target)) = (reference.name(), reference.resolve()) else {
                    continue;
                };
                if let Some(oid) = target.target() {
                    refspecs.push(format!("{oid}:{name}"));
                }
            }
        }
        Ok(refspecs)
    }

    /// The patch queue push (design §11.3, §12), after the branch push,
    /// for the patch remote only.
    ///
    /// It writes only by the branch rule (F8 rule 2): one listing of the
    /// branch and the volume's refs, and [`claim`] on the branch listed,
    /// judged against the local head. A branch that is not this worktree's
    /// ([`Claim::Create`]: none yet; [`Claim::Diverged`]: another writer's)
    /// writes nothing and is reported as [`QueuePush::branch_not_ours`],
    /// not as a failure: the branch push already alarms. Otherwise each of
    /// the two pushes below also carries the branch as a no-op update
    /// (`<listed>:<branch>`) leased on the value listed, so libgit2's
    /// negotiation stops it if the branch moved since the listing. Two
    /// pushes:
    ///
    /// 1. the status ref, forced (it is a snapshot), and, only when the
    ///    branch push succeeded, every local `applied/<n>` ref the remote
    ///    lacks, so an applied ref never reaches the remote before the
    ///    commit that applied it;
    /// 2. then, once push 1 succeeded, the deletion of each remote
    ///    `inbox/<n>` whose number is applied: the applied patch, or a
    ///    stale replacement that the queue reports as ignored and never
    ///    applies. Each deletion is leased on the oid listed: libgit2's
    ///    negotiation is checked against the expected oids, and a mismatch
    ///    (a producer wrote the ref since) stops push 2.
    ///
    /// Inbox refs are deleted only after their applied refs are on the
    /// remote, so the highest number there never goes down and
    /// `agentfs patch submit` never reuses one. A failure raises
    /// [`Event::PushFailed`] at stage `queue` and is kept as `queue_error`;
    /// it never counts toward the branch's failures, backoff or divergence.
    /// Skipped while the status pushed last is still the local one.
    fn queue_push(&mut self, branch_ok: bool) -> Option<QueuePush> {
        let status_ref = crate::patch::status_ref(&self.volume);
        let status = self.repo.refname_to_id(&status_ref).ok()?;
        if self.state.queue_error.is_none()
            && self.state.queue_synced_status.as_deref() == Some(status.to_string().as_str())
        {
            return None;
        }
        let mut report = QueuePush::default();
        match self.queue_push_inner(status, &status_ref, branch_ok, &mut report) {
            Ok(()) => {
                self.state.queue_error = None;
                // Not synced while the branch is not ours: retried next time.
                if branch_ok && report.branch_not_ours.is_none() {
                    self.state.queue_synced_ms = Some(now_ms());
                    self.state.queue_synced_status = Some(status.to_string());
                }
            }
            // A cancel at shutdown stopped it: nothing to record.
            Err(_) if self.stop.stopped_an_attempt() => return None,
            Err((kind, message)) => {
                let message = scrub_userinfo(&message, self.remote.push_url());
                self.state.queue_error = Some(LastError {
                    kind,
                    message: message.clone(),
                    stage: PushStage::Queue,
                });
                self.events.emit(Event::PushFailed {
                    remote: self.remote.name.clone(),
                    kind,
                    message: message.clone(),
                    head: None,
                    last_pushed: self.state.last_pushed_oid(),
                    consecutive_failures: self.state.consecutive_failures,
                    retry_in: self.next_delay(),
                    stage: PushStage::Queue,
                });
                report.error = Some((kind, message));
            }
        }
        if let Err(e) = self.state.save(&self.state_path)
            && report.error.is_none()
        {
            report.error = Some((
                PushFailureKind::Local,
                format!("saving {}: {e}", self.state_path.display()),
            ));
        }
        Some(report)
    }

    fn queue_push_inner(
        &self,
        status: Oid,
        status_ref: &str,
        branch_ok: bool,
        report: &mut QueuePush,
    ) -> Result<(), (PushFailureKind, String)> {
        let local = |e: GitError| (PushFailureKind::Local, e.message().to_owned());
        let applied_prefix = crate::patch::applied_prefix(&self.volume);
        let mut applied = BTreeMap::new();
        for reference in self
            .repo
            .references_glob(&format!("{applied_prefix}*"))
            .map_err(local)?
        {
            let reference = reference.map_err(local)?;
            let (Ok(name), Some(oid)) = (reference.name(), reference.target()) else {
                continue;
            };
            if let Some(n) = crate::patch::seq_of_ref(&applied_prefix, name) {
                applied.insert(n, oid);
            }
        }
        // One listing: the branch, and the volume's refs.
        let prefix = format!("refs/agentfs/{}/", self.volume);
        let remote =
            self.list_remote_refs(|name| name == self.branch_ref || name.starts_with(&prefix))?;
        #[cfg(test)]
        if let Some(hook) = &self.after_queue_listing {
            hook();
        }
        let listed = remote.get(&self.branch_ref).copied();
        // No local branch: nothing the remote holds is on this history.
        let judged = match self.repo.refname_to_id(&self.branch_ref) {
            Ok(head) => claim(listed, head, |oid| self.on_history(head, oid)),
            Err(_) if listed.is_none() => Claim::Create,
            Err(_) => Claim::Diverged,
        };
        let Some(listed) = listed.filter(|_| judged.is_ours()) else {
            report.branch_not_ours = Some(judged);
            return Ok(());
        };
        // The branch, unchanged, leased on the value listed.
        let lease_spec = format!("{listed}:{}", self.branch_ref);
        let lease = (self.branch_ref.clone(), listed);

        // Push 1.
        let mut specs = Vec::new();
        if remote.get(status_ref) != Some(&status) {
            specs.push(format!("+{status}:{status_ref}"));
            report.status = true;
        }
        for (n, oid) in &applied {
            let name = crate::patch::applied_ref(&self.volume, *n);
            if remote.get(&name) == Some(oid) {
                continue;
            }
            if branch_ok {
                specs.push(format!("{oid}:{name}"));
                report.applied += 1;
            } else {
                report.held_back = true;
            }
        }
        if !specs.is_empty() {
            specs.push(lease_spec.clone());
            if let Some(error) =
                self.attempt_error(&self.attempt(&specs, std::slice::from_ref(&lease)))
            {
                report.status = false;
                report.applied = 0;
                return Err(error);
            }
        }
        if !branch_ok {
            return Ok(());
        }

        // Push 2: every remote inbox entry whose number is applied, the
        // applied patch or a stale one (replaced after it was applied),
        // leased on the oid listed.
        let mut deletes: Vec<(String, Oid)> = applied
            .keys()
            .map(|n| crate::patch::inbox_ref(&self.volume, *n))
            .filter_map(|name| remote.get(&name).map(|oid| (name, *oid)))
            .collect();
        if deletes.is_empty() {
            return Ok(());
        }
        let deleted = deletes.len();
        let mut specs: Vec<String> = deletes.iter().map(|(name, _)| format!(":{name}")).collect();
        specs.push(lease_spec);
        deletes.push(lease);
        if let Some(error) = self.attempt_error(&self.attempt(&specs, &deletes)) {
            return Err(error);
        }
        report.deleted = deleted;
        Ok(())
    }

    /// Where a measured push starts (design §11.3): the branch the remote
    /// advertises, when this repository has that commit and it is the head
    /// or an ancestor of it. It is then recorded as the last pushed commit:
    /// the push state can lag the remote (deleted by the history-rewrite
    /// runbook, a renamed remote, a crash between a push and the state
    /// save), and stepping from a commit behind the remote would push
    /// commits it has over newer ones, a false divergence at every attempt.
    /// No branch on the remote: everything counts. Otherwise (the remote
    /// cannot be listed, or holds a commit that is unknown here or not an
    /// ancestor of the head) the push state's commit, and the push reports
    /// the real failure or divergence.
    fn measure_base(&mut self, head: Oid) -> Option<Oid> {
        let recorded = self.state.last_pushed_oid();
        let branch = self.branch_ref.clone();
        let Ok(listed) = self.list_remote_refs(|name| name == branch) else {
            return recorded;
        };
        let remote = listed.get(&self.branch_ref).copied()?;
        let known = self.repo.odb().is_ok_and(|odb| odb.exists(remote));
        if known && (remote == head || self.repo.graph_descendant_of(head, remote).unwrap_or(false))
        {
            self.state.last_pushed = Some(remote.to_string());
            return Some(remote);
        }
        recorded
    }

    /// The remote's refs whose names pass `keep`, as its receive side
    /// advertises them.
    fn list_remote_refs(
        &self,
        keep: impl Fn(&str) -> bool,
    ) -> Result<BTreeMap<String, Oid>, (PushFailureKind, String)> {
        let auth = AuthState::new();
        let _scope = self.auth.ssh_scope(&auth);
        let listed = (|| {
            let mut remote = self.repo.remote_anonymous(self.remote.push_url())?;
            let callbacks = self.auth.callbacks(&auth);
            let proxy =
                crate::proxy::git_options(self.remote.proxy.as_ref(), self.remote.push_url());
            let connection =
                remote.connect_auth(git2::Direction::Push, Some(callbacks), Some(proxy))?;
            Ok(connection
                .list()?
                .iter()
                .filter(|head| keep(head.name()))
                .map(|head| (head.name().to_owned(), head.oid()))
                .collect())
        })();
        listed.map_err(|e: GitError| {
            let attempt = Attempt {
                result: Err(e),
                negotiated: false,
                auth_failed: auth.auth_failed(),
                host_key_failure: auth.host_key_failure(),
                updates: Vec::new(),
                rejected: Vec::new(),
                claim: None,
            };
            self.attempt_error(&attempt)
                .unwrap_or((PushFailureKind::Unreachable, "listing the remote".into()))
        })
    }

    /// How a queue push attempt failed, if it did.
    fn attempt_error(&self, attempt: &Attempt) -> Option<(PushFailureKind, String)> {
        match &attempt.result {
            Ok(()) if attempt.rejected.is_empty() => None,
            Ok(()) => {
                let message = attempt
                    .rejected
                    .iter()
                    .map(|(name, msg)| format!("{name}: {msg}"))
                    .collect::<Vec<_>>()
                    .join("; ");
                let kind =
                    if message.contains("non-fast-forward") || message.contains("fetch first") {
                        PushFailureKind::NonFastForward
                    } else {
                        PushFailureKind::Refused
                    };
                Some((kind, message))
            }
            Err(e) => {
                if let Some(message) = &attempt.host_key_failure {
                    return Some((PushFailureKind::UntrustedHost, message.clone()));
                }
                let kind = if attempt.auth_failed {
                    PushFailureKind::Auth
                } else if e.code() == ErrorCode::User {
                    PushFailureKind::Refused
                } else {
                    classify(e, attempt.negotiated)
                };
                Some((kind, e.message().to_owned()))
            }
        }
    }

    /// One libgit2 push of `refspecs`. A ref in `expect` must hold the
    /// given oid on the remote, or the push is stopped before it is sent.
    fn attempt(&self, refspecs: &[String], expect: &[(String, Oid)]) -> Attempt {
        self.attempt_guarded(refspecs, expect, None)
    }

    /// The branch push of `refspecs`, whose branch update pushes `head`:
    /// in the negotiation, from the old value the remote reports for the
    /// branch in this very connection, [`claim`] decides (F8 rule 2), and
    /// [`Claim::Diverged`] stops the push before anything is sent.
    fn attempt_branch(&self, refspecs: &[String], head: Oid) -> Attempt {
        self.attempt_guarded(refspecs, &[], Some((&self.branch_ref, head)))
    }

    /// One libgit2 push of `refspecs`, checked in its negotiation: a ref in
    /// `expect` must hold the given oid on the remote, and the update of
    /// `guard`'s ref, which pushes its oid, must be [`claim`]ed from the
    /// old value the remote reports in this connection (F8 rule 2):
    /// [`Claim::Diverged`] stops the push before anything is sent.
    fn attempt_guarded(
        &self,
        refspecs: &[String],
        expect: &[(String, Oid)],
        guard: Option<(&str, Oid)>,
    ) -> Attempt {
        let negotiated = Cell::new(false);
        let auth = AuthState::new();
        let _scope = self.auth.ssh_scope(&auth);
        let updates = RefCell::new(Vec::new());
        let rejected = RefCell::new(Vec::new());
        let judged_claim = Cell::new(None);
        let result = (|| {
            let mut remote = self.repo.remote_anonymous(self.remote.push_url())?;
            // Where libgit2 will push, after any `url.<base>.insteadOf` or
            // `pushInsteadOf` of the volume's own config: a path there is
            // updated by this process.
            let local = remote_url::transport(
                remote
                    .pushurl()
                    .ok()
                    .flatten()
                    .or(remote.url().ok())
                    .unwrap_or(self.remote.push_url()),
            ) == Transport::Local;
            let mut callbacks = self.auth.callbacks(&auth);
            callbacks.push_negotiation(|ups| {
                negotiated.set(true);
                updates.borrow_mut().extend(ups.iter().map(|u| {
                    (
                        String::from_utf8_lossy(u.dst_refname_bytes()).into_owned(),
                        u.src(),
                    )
                }));
                for update in ups {
                    let name = String::from_utf8_lossy(update.dst_refname_bytes());
                    if let Some((_, want)) = expect.iter().find(|(n, _)| *n == name)
                        && update.src() != *want
                    {
                        return Err(GitError::from_str(&format!(
                            "{name} changed on the remote (it holds {}, not {want}); kept",
                            update.src()
                        )));
                    }
                    if let Some((guarded, head)) = guard
                        && name == guarded
                    {
                        let old = Some(update.src()).filter(|o| !o.is_zero());
                        let judged = claim(old, head, |oid| self.on_history(head, oid));
                        judged_claim.set(Some(judged));
                        if judged == Claim::Diverged {
                            return Err(GitError::from_str(&format!(
                                "{name} on the remote is not on this volume's history; nothing \
                                 was sent"
                            )));
                        }
                    }
                }
                // The last point where libgit2 lets a push stop.
                self.stop.enter(local)?;
                #[cfg(test)]
                if let Some(hook) = &self.after_negotiation {
                    hook();
                }
                Ok(())
            });
            callbacks.push_update_reference(|name, status| {
                if let Some(status) = status {
                    rejected
                        .borrow_mut()
                        .push((name.to_owned(), status.to_owned()));
                }
                Ok(())
            });
            let mut options = PushOptions::new();
            options.remote_callbacks(callbacks);
            options.proxy_options(crate::proxy::git_options(
                self.remote.proxy.as_ref(),
                self.remote.push_url(),
            ));
            remote.push(refspecs, Some(&mut options))
        })();
        self.stop.leave();
        #[cfg(test)]
        if let Some(hook) = &self.after_update {
            hook();
        }
        Attempt {
            result,
            negotiated: negotiated.get(),
            auth_failed: auth.auth_failed(),
            host_key_failure: auth.host_key_failure(),
            updates: updates.into_inner(),
            rejected: rejected.into_inner(),
            claim: judged_claim.get(),
        }
    }

    #[cfg(test)]
    fn answer(
        &self,
        url: &str,
        username: Option<&str>,
        allowed: CredentialType,
        calls: &Cell<u32>,
    ) -> Result<crate::credentials::Credential, String> {
        self.auth.answer(url, username, allowed, calls)
    }

    #[cfg(test)]
    fn check_host_key(
        &self,
        hostname: &str,
        raw: Option<&[u8]>,
        sha256: Option<&[u8; 32]>,
    ) -> Result<(), String> {
        self.auth.check_host_key(hostname, raw, sha256)
    }

    /// True if the remote branch at `remote` is not an ancestor of `head`.
    fn is_divergence(&self, head: Oid, remote: Option<Oid>) -> bool {
        let Some(remote) = remote else {
            return false;
        };
        let known = self.repo.odb().is_ok_and(|odb| odb.exists(remote));
        !known || !(remote == head || self.repo.graph_descendant_of(head, remote).unwrap_or(false))
    }

    fn succeeded(&mut self, head: Oid, previous: Option<Oid>, up_to_date: bool) -> PushOutcome {
        let now = now_ms();
        self.state.last_pushed = Some(head.to_string());
        self.state.last_success_ms = Some(now);
        self.state.consecutive_failures = 0;
        self.state.last_error = None;
        self.state.diverged = None;
        if let Err(e) = crate::failpoints::hit("push.after_remote_update")
            .and_then(|()| self.state.save(&self.state_path))
        {
            let message = format!(
                "pushed {head}, but saving {}: {e}",
                self.state_path.display()
            );
            return self.failed(PushFailureKind::Local, message, Some(head));
        }
        self.events.emit(Event::PushSucceeded {
            remote: self.remote.name.clone(),
            head,
            previous,
        });
        if up_to_date {
            PushOutcome::UpToDate { head }
        } else {
            PushOutcome::Pushed {
                from: previous,
                to: head,
                limited: None,
            }
        }
    }

    /// Someone else wrote the branch: save the local head under
    /// `refs/agentfs/<volume>/diverged/<UTC timestamp>`, then fail like any
    /// other attempt.
    ///
    /// The save is created once per divergence, raising
    /// [`Event::Diverged`], and then follows the head (F8 D3): at each
    /// attempt whose head moved past the one saved, the save is pushed
    /// again, judged by [`claim`] in the push's negotiation on the value
    /// the remote holds there. A fast-forward moves it, leased on that
    /// value; a save the remote lacks is re-created; one another writer
    /// moved is kept and named in the alarm, never overwritten.
    ///
    /// The ref name is chosen and saved in the state file as
    /// `pending_ref` before it is first pushed, so a crash between the push
    /// and the state update retries the same name (the same or a
    /// descendant head: a no-op or a fast-forward) instead of saving the
    /// head again under a new timestamp.
    fn diverged(&mut self, head: Oid, remote_head: Option<Oid>, message: String) -> PushOutcome {
        let now = now_ms();
        let previous = self.state.diverged.take();
        let since_ms = previous.as_ref().map_or(now, |d| d.since_ms);
        let mut diverged_ref = previous.as_ref().and_then(|d| d.diverged_ref.clone());
        let mut pending_ref = previous.as_ref().and_then(|d| d.pending_ref.clone());
        let mut saved_head = previous.as_ref().and_then(|d| d.saved_head.clone());
        let mut message = message;
        if let Some(name) = diverged_ref.clone() {
            if saved_head.as_deref() != Some(head.to_string().as_str()) {
                match self.save_head(&name, head) {
                    Saved::Held => saved_head = Some(head.to_string()),
                    Saved::AnotherWriter => {
                        message = format!(
                            "{message}; {name} on the remote is not on this volume's history \
                             (another writer moved it): kept, not moved to the head"
                        );
                    }
                    Saved::Refused(why) | Saved::Failed(why) => {
                        message = format!("{message}; moving {name} to the head: {why}");
                    }
                }
            }
        } else {
            let name = pending_ref
                .clone()
                .unwrap_or_else(|| self::diverged_ref(&self.volume, now / 1000));
            pending_ref = Some(name.clone());
            self.state.diverged = Some(Divergence {
                since_ms,
                local_head: head.to_string(),
                remote_head: remote_head.map(|o| o.to_string()),
                diverged_ref: None,
                pending_ref: pending_ref.clone(),
                saved_head: None,
            });
            match self
                .state
                .save(&self.state_path)
                .and_then(|()| crate::failpoints::hit("push.diverged.after_pending"))
            {
                Err(e) => {
                    // Without a durable name, pushing could save the head
                    // twice; try again at the next attempt.
                    message = format!(
                        "{message}; not saving the head as {name}: saving {}: {e}",
                        self.state_path.display()
                    );
                }
                Ok(()) => match self.save_head(&name, head) {
                    Saved::Held => {
                        if let Err(e) = crate::failpoints::hit("push.diverged.after_ref") {
                            message = format!("{message}; {e}");
                        }
                        self.events.emit(Event::Diverged {
                            remote: self.remote.name.clone(),
                            local_head: head,
                            remote_head,
                            diverged_ref: name.clone(),
                        });
                        diverged_ref = Some(name);
                        pending_ref = None;
                        saved_head = Some(head.to_string());
                    }
                    Saved::AnotherWriter => {
                        // Someone else holds this name; pick a fresh one
                        // next time.
                        message = format!(
                            "{message}; saving the head as {name}: the remote holds another \
                             commit there"
                        );
                        pending_ref = None;
                    }
                    Saved::Refused(why) => {
                        // The remote refused this name; pick a fresh one
                        // next time.
                        message = format!("{message}; saving the head as {name}: {why}");
                        pending_ref = None;
                    }
                    Saved::Failed(why) => {
                        message = format!("{message}; saving the head as {name}: {why}");
                    }
                },
            }
        }
        self.state.diverged = Some(Divergence {
            since_ms,
            local_head: head.to_string(),
            remote_head: remote_head.map(|o| o.to_string()),
            diverged_ref: diverged_ref.clone(),
            pending_ref,
            saved_head,
        });
        self.failed(PushFailureKind::NonFastForward, message, Some(head));
        PushOutcome::Diverged {
            local_head: head,
            remote_head,
            diverged_ref,
        }
    }

    /// Pushes `head` to the divergence save `name`, judged by [`claim`] on
    /// the remote's value of `name` in the push's own negotiation: created
    /// where the remote lacks it, fast-forwarded where it holds a commit on
    /// this history, and never moved otherwise.
    fn save_head(&self, name: &str, head: Oid) -> Saved {
        let attempt = self.attempt_guarded(&[format!("{head}:{name}")], &[], Some((name, head)));
        if attempt.claim == Some(Claim::Diverged) {
            return Saved::AnotherWriter;
        }
        match (&attempt.result, attempt.rejected.first()) {
            (Ok(()), None) => Saved::Held,
            (Ok(()), Some((_, msg))) => Saved::Refused(msg.clone()),
            (Err(e), _) => Saved::Failed(e.message().to_owned()),
        }
    }

    fn failed(&mut self, kind: PushFailureKind, message: String, head: Option<Oid>) -> PushOutcome {
        self.failed_at(kind, message, head, PushStage::Git)
    }

    /// A failed attempt at `stage`: persisted, alarmed, backed off.
    fn failed_at(
        &mut self,
        kind: PushFailureKind,
        message: String,
        head: Option<Oid>,
        stage: PushStage,
    ) -> PushOutcome {
        // libgit2 quotes the URL in its messages; never persist or emit the
        // userinfo it may carry.
        // A cancel at shutdown stopped it: not the remote's failure.
        if self.stop.stopped_an_attempt() {
            return PushOutcome::Cancelled;
        }
        let message = scrub_userinfo(&message, self.remote.push_url());
        self.state.consecutive_failures = self.state.consecutive_failures.saturating_add(1);
        self.state.last_error = Some(LastError {
            kind,
            message: message.clone(),
            stage,
        });
        let retry_in = self.next_delay();
        let mut message = message;
        if let Err(e) = self.state.save(&self.state_path) {
            message = format!("{message} (saving {}: {e})", self.state_path.display());
        }
        self.events.emit(Event::PushFailed {
            remote: self.remote.name.clone(),
            kind,
            message: message.clone(),
            head,
            last_pushed: self.state.last_pushed_oid(),
            consecutive_failures: self.state.consecutive_failures,
            retry_in,
            stage,
        });
        PushOutcome::Failed {
            kind,
            message,
            retry_in,
            stage,
        }
    }
}

/// The first of `refs` that `listed` holds at another oid.
fn another_writer(
    refs: &[CreateOnlyRef],
    listed: &BTreeMap<String, Oid>,
) -> Option<CreateOnlyOutcome> {
    refs.iter().find_map(|r| match listed.get(&r.name) {
        Some(found) if *found != r.oid => Some(CreateOnlyOutcome::AnotherWriter {
            name: r.name.clone(),
            found: *found,
        }),
        _ => None,
    })
}

/// What a lease scratch repository's name ends with, after its remote's
/// name: `<remote>.lease.git`, beside `<remote>.lock`.
const LEASE_SCRATCH_SUFFIX: &str = ".lease.git";

/// The lease check's scratch repository for the remote whose push lock is
/// `lock` (`<push dir>/<remote>.lock`).
fn lease_scratch(lock: &Path) -> PathBuf {
    let mut name = lock.file_stem().unwrap_or_default().to_os_string();
    name.push(LEASE_SCRATCH_SUFFIX);
    lock.with_file_name(name)
}

/// The lease check's scratch repository (design §15), which holds the
/// remote's history, the old history a rewrite removed included: removed
/// when created (a leftover is never reused) and when dropped, so no exit
/// path, an error or a panic unwinding, leaves it behind. A process killed
/// meanwhile skips the drop; the next push sweeps it
/// ([`sweep_lease_scratch`]).
struct LeaseScratch(PathBuf);

impl LeaseScratch {
    fn new(path: PathBuf) -> Self {
        let _ = fs::remove_dir_all(&path);
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for LeaseScratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Removes every lease scratch repository beside `own_lock`, a push lock
/// the caller holds: its own remote's, and any other remote's, a
/// configured one or one since removed, whose push lock is free. A scratch
/// repository is only made under its remote's lock, so one whose lock is
/// free (or whose lock file is gone) is what a killed lease push left; one
/// whose lock another process holds is in use and stays. Best effort: a
/// failure leaves it for the next push.
fn sweep_lease_scratch(own_lock: &Path) {
    let Some(dir) = own_lock.parent() else {
        return;
    };
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(remote) = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_suffix(LEASE_SCRATCH_SUFFIX))
        else {
            continue;
        };
        let lock = dir.join(format!("{remote}.lock"));
        // Held for the removal only, through [`FileLock`] like every
        // push lock, and never this process's own lock twice: flock locks
        // are per open file, so a second open would conflict with the one
        // the caller holds. Taking a removed remote's lock creates its
        // file, so a push of that name starting now waits for the removal.
        let _held = if lock == own_lock {
            None
        } else {
            match FileLock::try_take(&lock, Mode::Exclusive) {
                Ok(Some(held)) => Some(held),
                Ok(None) | Err(_) => continue,
            }
        };
        let _ = fs::remove_dir_all(&path);
    }
}

/// The commits `from` reaches and `hidden` does not, newest first, each
/// with its [`Recording`] ([`lease_claim`]'s inputs).
fn commits_beyond(
    repo: &Repository,
    from: Oid,
    hidden: Oid,
) -> Result<Vec<(Oid, Option<Recording>)>, GitError> {
    let mut walk = repo.revwalk()?;
    walk.push(from)?;
    walk.hide(hidden)?;
    walk.map(|oid| {
        let oid = oid?;
        Ok((oid, recording(&repo.find_commit(oid)?)))
    })
    .collect()
}

/// `commit` as a recorder made it: `None` without an `Agentfs-Seq`
/// trailer (design §9), which every agentfs commit carries.
fn recording(commit: &git2::Commit) -> Option<Recording> {
    let seq = crate::history::seq_from_message(&String::from_utf8_lossy(commit.message_bytes()))?;
    let stamp = |sig: git2::Signature| Stamp {
        name: sig.name_bytes().to_vec(),
        email: sig.email_bytes().to_vec(),
        seconds: sig.when().seconds(),
        offset_minutes: sig.when().offset_minutes(),
    };
    Some(Recording {
        seq,
        author: stamp(commit.author()),
        committer: stamp(commit.committer()),
    })
}

/// A pusher for every configured remote (see [`RemotePusher::open`]).
pub fn open_all(
    state: &StateDir,
    settings: &Settings,
    events: Events,
    telemetry: &Telemetry,
) -> Result<Vec<RemotePusher>, PushError> {
    settings
        .remotes
        .pushed()
        .iter()
        .map(|remote| RemotePusher::open(state, settings, remote, events.clone(), telemetry))
        .collect()
}

/// One remote as `agentfs status` shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteStatus {
    pub name: String,
    /// The push URL, with any userinfo redacted.
    pub url: String,
    pub state: PushState,
    pub unpushed: Option<usize>,
}

/// Every remote's state, read without taking the push locks.
pub fn status(state: &StateDir, settings: &Settings) -> Result<Vec<RemoteStatus>, PushError> {
    init_libgit2();
    let repo = Repository::open_bare(state.git())?;
    settings
        .remotes
        .pushed()
        .iter()
        .map(|remote| {
            let push_state = PushState::load(&state.push_state(&remote.name));
            let source = settings.push_source_ref(remote);
            let unpushed = unpushed(&repo, &source, push_state.last_pushed_oid())?.map(|w| w.count);
            Ok(RemoteStatus {
                name: remote.name.clone(),
                url: redact_url(remote.push_url()),
                state: push_state,
                unpushed,
            })
        })
        .collect()
}

/// One remote's push backlog, as the telemetry gauges report it (design
/// §13).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteBacklog {
    pub remote: String,
    /// Commits on the branch the remote is not known to have; 0 if the
    /// branch does not exist yet.
    pub unpushed: usize,
    /// The committer time (Unix seconds) of the oldest unpushed commit.
    pub oldest_unpushed_s: Option<i64>,
    /// The remote branch has diverged and is not resolved (design §11.3).
    pub diverged: bool,
}

/// Every remote's backlog, read from its push state and the branch without
/// taking the push locks. Opens its own repository handle, so it can run on
/// any thread.
pub fn backlog(state: &StateDir, settings: &Settings) -> Result<Vec<RemoteBacklog>, PushError> {
    init_libgit2();
    let repo = Repository::open_bare(state.git())?;
    settings
        .remotes
        .pushed()
        .iter()
        .map(|remote| {
            let push_state = PushState::load(&state.push_state(&remote.name));
            let source = settings.push_source_ref(remote);
            let walk = unpushed(&repo, &source, push_state.last_pushed_oid())?;
            let oldest = match walk.and_then(|w| w.oldest) {
                Some(oid) => Some(repo.find_commit(oid)?.committer().when().seconds()),
                None => None,
            };
            Ok(RemoteBacklog {
                remote: remote.name.clone(),
                unpushed: walk.map_or(0, |w| w.count),
                oldest_unpushed_s: oldest,
                diverged: push_state.diverged.is_some(),
            })
        })
        .collect()
}

/// What [`unpushed`] walked.
#[derive(Debug, Clone, Copy)]
struct Unpushed {
    count: usize,
    /// The last commit walked: the oldest unpushed one, since the history
    /// branch is linear.
    oldest: Option<Oid>,
}

fn unpushed(
    repo: &Repository,
    branch_ref: &str,
    last_pushed: Option<Oid>,
) -> Result<Option<Unpushed>, GitError> {
    let head = match repo.refname_to_id(branch_ref) {
        Ok(head) => head,
        Err(e) if e.code() == ErrorCode::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let mut walk = repo.revwalk()?;
    walk.push(head)?;
    if let Some(last) = last_pushed
        && repo.odb()?.exists(last)
    {
        walk.hide(last)?;
    }
    let mut count = 0;
    let mut oldest = None;
    for oid in walk {
        oldest = Some(oid?);
        count += 1;
    }
    Ok(Some(Unpushed { count, oldest }))
}

/// The userinfo (`user:pass`) of a `scheme://user:pass@host/…` URL.
fn userinfo(url: &str) -> Option<&str> {
    let (_, rest) = url.split_once("://")?;
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let at = rest[..end].rfind('@')?;
    Some(&rest[..at]).filter(|u| !u.is_empty())
}

/// `url` with the userinfo of a `scheme://user:pass@host/…` URL replaced by
/// `***`.
pub fn redact_url(url: &str) -> String {
    match (userinfo(url), url.split_once("://")) {
        (Some(info), Some((scheme, rest))) => {
            format!("{scheme}://***@{}", &rest[info.len() + 1..])
        }
        _ => url.to_owned(),
    }
}

/// `message` with every copy of `url`'s userinfo replaced by `***`.
pub(crate) fn scrub_userinfo(message: &str, url: &str) -> String {
    match userinfo(url) {
        Some(info) => message.replace(info, "***"),
        None => message.to_owned(),
    }
}

/// Maps a failed push to a failure kind. `negotiated` says whether libgit2
/// got as far as reading the remote's refs; anything failing before that is
/// treated as the remote being unreachable, except:
///
/// - a certificate error (a TLS certificate that failed verification, or an
///   SSH host key the SSH transport refused) is an untrusted host;
/// - an authentication error (from the credentials callback, or the SSH
///   transport's key being refused or unreadable) is an authentication
///   failure;
/// - a timeout is the remote being unreachable.
pub fn classify(err: &GitError, negotiated: bool) -> PushFailureKind {
    match err.code() {
        ErrorCode::Auth => return PushFailureKind::Auth,
        ErrorCode::NotFastForward => return PushFailureKind::NonFastForward,
        ErrorCode::Certificate => return PushFailureKind::UntrustedHost,
        ErrorCode::Timeout => return PushFailureKind::Unreachable,
        _ => {}
    }
    if !negotiated {
        return PushFailureKind::Unreachable;
    }
    match err.class() {
        ErrorClass::Net | ErrorClass::Http | ErrorClass::Ssh | ErrorClass::Ssl | ErrorClass::Os => {
            PushFailureKind::Unreachable
        }
        _ => PushFailureKind::Refused,
    }
}

/// The delay after `failures` consecutive failures: `base` doubled for each
/// failure after the first, at most `max(base, cap)`. `base` with no
/// failures.
pub fn backoff(base: Duration, failures: u32, cap: Duration) -> Duration {
    let cap = cap.max(base);
    let doublings = failures.saturating_sub(1).min(31);
    base.checked_mul(1 << doublings).map_or(cap, |d| d.min(cap))
}

/// `YYYYMMDDTHHMMSSZ`: an ISO 8601 basic UTC timestamp, without colons so
/// it is a valid ref component.
pub fn utc_basic(unix_secs: u64) -> String {
    let days = unix_secs / 86_400;
    let secs = unix_secs % 86_400;
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}{m:02}{d:02}T{:02}{:02}{:02}Z",
        secs / 3600,
        secs / 60 % 60,
        secs % 60
    )
}

/// Where a diverged head is saved (design §11.3, §11.4).
pub fn diverged_ref(volume: &str, unix_secs: u64) -> String {
    format!("refs/agentfs/{volume}/diverged/{}", utc_basic(unix_secs))
}

/// Days since 1970-01-01 to a proleptic Gregorian date (Howard Hinnant's
/// `civil_from_days`, for non-negative days).
fn civil_from_days(days: u64) -> (u64, u64, u64) {
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + u64::from(m <= 2);
    (y, m, d)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD_NO_PAD;

    use super::*;
    use crate::credentials::Credential;
    use crate::events::{MemorySink, NullSink};
    use crate::identity::Context;
    use crate::paths::RepoPath;
    use crate::record::{Caller, Trigger};
    use crate::volume::{self, Committer, Input, Recorder};

    const CALLER: Caller = Caller {
        uid: 1,
        gid: 1,
        pid: 1,
    };

    /// A volume in a temporary directory, with remotes added to its config.
    struct Vol {
        dir: tempfile::TempDir,
        state: StateDir,
        rec: Recorder,
        com: Committer,
        settings: Settings,
        n: usize,
    }

    impl Vol {
        fn new(volume: &str, remotes: &[(&str, &str)]) -> Self {
            Self::with_config(volume, remotes, &[])
        }

        /// [`Vol::new`] with more `(key, value)` config entries.
        fn with_config(volume: &str, remotes: &[(&str, &str)], extra: &[(&str, &str)]) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let state = StateDir::new(dir.path().join("state"));
            volume::init(&state, volume, None).unwrap();
            let mut config = git2::Config::open(&state.git().join("config")).unwrap();
            for (name, url) in remotes {
                config.set_str(&format!("remote.{name}.url"), url).unwrap();
            }
            for (key, value) in extra {
                config.set_str(key, value).unwrap();
            }
            let context = Context {
                volume: volume.into(),
                replica: "r".into(),
                pod_uid: "u".into(),
                ..Context::default()
            };
            fs::create_dir_all(dir.path().join("tree")).unwrap();
            let (rec, com, _) = volume::open(
                &state,
                &dir.path().join("tree"),
                context,
                Arc::new(NullSink),
                &Telemetry::noop(),
            )
            .unwrap();
            let settings = com.history().settings().clone();
            Self {
                dir,
                state,
                rec,
                com,
                settings,
                n: 0,
            }
        }

        /// Records and commits one new file; returns the new head.
        fn commit(&mut self) -> Oid {
            self.n += 1;
            let backing = self.dir.path().join("tree").join(format!("f{}", self.n));
            fs::write(&backing, format!("content {}", self.n)).unwrap();
            self.rec
                .record(
                    Trigger::Close,
                    CALLER,
                    vec![Input::Write {
                        path: RepoPath::new(format!("f{}", self.n)).unwrap(),
                        backing,
                        executable: false,
                    }],
                )
                .unwrap();
            self.com.commit_pending().unwrap();
            self.head()
        }

        /// Records and commits `name` holding `content`; returns the new head.
        fn commit_file(&mut self, name: &str, content: &[u8]) -> Oid {
            let backing = self.dir.path().join("tree").join(name);
            fs::write(&backing, content).unwrap();
            self.rec
                .record(
                    Trigger::Close,
                    CALLER,
                    vec![Input::Write {
                        path: RepoPath::new(name).unwrap(),
                        backing,
                        executable: false,
                    }],
                )
                .unwrap();
            self.com.commit_pending().unwrap();
            self.head()
        }

        fn head(&self) -> Oid {
            self.com.history().head().unwrap().unwrap().id()
        }

        fn pusher(&self, remote: &str, events: Events) -> RemotePusher {
            self.pusher_with(remote, events, &Telemetry::noop())
        }

        fn pusher_with(&self, remote: &str, events: Events, telemetry: &Telemetry) -> RemotePusher {
            let r = self
                .settings
                .remotes
                .pushed()
                .iter()
                .find(|r| r.name == remote)
                .unwrap();
            RemotePusher::open(&self.state, &self.settings, r, events, telemetry).unwrap()
        }
    }

    fn bare(dir: &Path) -> Repository {
        Repository::init_bare(dir).unwrap()
    }

    fn remote_ref(remote: &Path, name: &str) -> Option<Oid> {
        Repository::open_bare(remote)
            .unwrap()
            .refname_to_id(name)
            .ok()
    }

    fn remote_refs(remote: &Path) -> Vec<String> {
        let repo = Repository::open_bare(remote).unwrap();
        let mut names: Vec<String> = repo
            .references()
            .unwrap()
            .map(|r| r.unwrap().name().unwrap().to_owned())
            .collect();
        names.sort();
        names
    }

    fn sink() -> (Arc<MemorySink>, Events) {
        let sink = Arc::new(MemorySink::new());
        (sink.clone(), sink)
    }

    /// A remote with `agentfsPush = changeLine` gets the change line at
    /// its branch ref, never the branch; another remote gets the branch.
    /// Claims, backlog and status follow each remote's own source.
    #[test]
    fn a_change_line_remote_gets_the_change_line() {
        let lines = tempfile::tempdir().unwrap();
        let branch = tempfile::tempdir().unwrap();
        bare(lines.path());
        bare(branch.path());
        let mut v = Vol::with_config(
            "vol",
            &[
                ("lines", &lines.path().display().to_string()),
                ("all", &branch.path().display().to_string()),
            ],
            &[("remote.lines.agentfsPush", "changeLine")],
        );
        let remote = |name: &str| {
            v.settings
                .remotes
                .pushed()
                .iter()
                .find(|r| r.name == name)
                .unwrap()
                .push_source
        };
        assert_eq!(remote("lines"), crate::config::PushSource::ChangeLine);
        assert_eq!(remote("all"), crate::config::PushSource::Branch);
        v.commit();
        // No change line yet: nothing to push there.
        let mut lines_pusher = v.pusher("lines", Arc::new(NullSink));
        assert_eq!(lines_pusher.unpushed().unwrap(), None);
        assert_eq!(lines_pusher.push_once(), PushOutcome::NothingToPush);

        let request = crate::checkpoint::CheckpointRequest {
            message: Some("one".into()),
            caller: CALLER,
        };
        let crate::checkpoint::CheckpointOutcome::Cut { change, .. } =
            crate::checkpoint::cut(&v.com, &request).unwrap()
        else {
            panic!("nothing cut");
        };
        v.commit();
        let head = v.head();
        assert_eq!(lines_pusher.unpushed().unwrap(), Some(1));
        assert!(lines_pusher.push_once().is_success());
        assert!(v.pusher("all", Arc::new(NullSink)).push_once().is_success());
        assert_eq!(
            remote_ref(lines.path(), "refs/heads/agentfs/vol"),
            Some(change)
        );
        assert_eq!(
            remote_ref(branch.path(), "refs/heads/agentfs/vol"),
            Some(head)
        );
        assert_eq!(lines_pusher.unpushed().unwrap(), Some(0));
        let status = status(&v.state, &v.settings).unwrap();
        let unpushed: Vec<_> = status
            .iter()
            .map(|r| (r.name.as_str(), r.unpushed))
            .collect();
        assert_eq!(unpushed, [("all", Some(0)), ("lines", Some(0))]);
        assert!(
            backlog(&v.state, &v.settings)
                .unwrap()
                .iter()
                .all(|b| b.unpushed == 0)
        );
        assert_eq!(lines_pusher.branch_claim(change).unwrap(), Claim::UpToDate);

        // The next checkpoint fast-forwards the remote's change line.
        v.commit();
        let crate::checkpoint::CheckpointOutcome::Cut { change: next, .. } =
            crate::checkpoint::cut(&v.com, &request).unwrap()
        else {
            panic!("nothing cut");
        };
        assert!(lines_pusher.push_once().is_success());
        assert_eq!(
            remote_ref(lines.path(), "refs/heads/agentfs/vol"),
            Some(next)
        );
    }

    #[test]
    fn classify_errors() {
        let e = |code, class| GitError::new(code, class, "x");
        use PushFailureKind::*;
        assert_eq!(classify(&e(ErrorCode::Auth, ErrorClass::Http), true), Auth);
        assert_eq!(
            classify(&e(ErrorCode::Auth, ErrorClass::Callback), false),
            Auth
        );
        assert_eq!(
            classify(&e(ErrorCode::NotFastForward, ErrorClass::Reference), true),
            NonFastForward
        );
        for class in [ErrorClass::Ssl, ErrorClass::Ssh, ErrorClass::Net] {
            for negotiated in [false, true] {
                assert_eq!(
                    classify(&e(ErrorCode::Certificate, class), negotiated),
                    UntrustedHost
                );
            }
        }
        assert_eq!(classify(&e(ErrorCode::Auth, ErrorClass::Ssh), false), Auth);
        assert_eq!(
            classify(&e(ErrorCode::Timeout, ErrorClass::Ssh), true),
            Unreachable
        );
        assert_eq!(
            classify(&e(ErrorCode::GenericError, ErrorClass::Ssh), false),
            Unreachable
        );
        assert_eq!(
            classify(&e(ErrorCode::GenericError, ErrorClass::Reference), false),
            Unreachable
        );
        for class in [
            ErrorClass::Net,
            ErrorClass::Http,
            ErrorClass::Ssh,
            ErrorClass::Ssl,
            ErrorClass::Os,
        ] {
            assert_eq!(
                classify(&e(ErrorCode::GenericError, class), true),
                Unreachable,
                "{class:?}"
            );
        }
        assert_eq!(
            classify(&e(ErrorCode::GenericError, ErrorClass::Reference), true),
            Refused
        );
    }

    #[test]
    fn backoff_doubles_and_caps() {
        let s = Duration::from_secs;
        let delays: Vec<u64> = (1..=6)
            .map(|n| backoff(s(60), n, PUSH_BACKOFF_CAP).as_secs())
            .collect();
        assert_eq!(delays, [60, 120, 240, 480, 900, 900]);
        assert_eq!(backoff(s(60), 0, PUSH_BACKOFF_CAP), s(60));
        assert_eq!(backoff(s(60), u32::MAX, PUSH_BACKOFF_CAP), s(900));
        for n in [1, 2, 10] {
            assert_eq!(backoff(s(1800), n, PUSH_BACKOFF_CAP), s(1800));
        }
        assert_eq!(backoff(Duration::ZERO, 5, PUSH_BACKOFF_CAP), Duration::ZERO);
    }

    #[test]
    fn utc_basic_formats() {
        assert_eq!(utc_basic(0), "19700101T000000Z");
        assert_eq!(utc_basic(951_782_400), "20000229T000000Z");
        assert_eq!(utc_basic(951_868_799), "20000229T235959Z");
        assert_eq!(utc_basic(951_868_800), "20000301T000000Z");
        assert_eq!(utc_basic(1_700_000_000), "20231114T221320Z");
        assert_eq!(utc_basic(4_102_444_800), "21000101T000000Z");
        assert_eq!(
            diverged_ref("vol", 0),
            "refs/agentfs/vol/diverged/19700101T000000Z"
        );
    }

    #[test]
    fn redacts_userinfo() {
        assert_eq!(
            redact_url("https://user:tok@example.com/r.git"),
            "https://***@example.com/r.git"
        );
        assert_eq!(
            redact_url("https://example.com/a@b"),
            "https://example.com/a@b"
        );
        assert_eq!(redact_url("git@host:r.git"), "git@host:r.git");
        assert_eq!(redact_url("/srv/r.git"), "/srv/r.git");
        assert_eq!(
            scrub_userinfo(
                "failed to resolve path 'file://u:pw@/x': nope",
                "file://u:pw@/x"
            ),
            "failed to resolve path 'file://***@/x': nope"
        );
        assert_eq!(scrub_userinfo("plain", "/x"), "plain");
    }

    #[test]
    fn failure_messages_never_carry_url_credentials() {
        let dir = tempfile::tempdir().unwrap();
        let url = format!("file://bot:hunter2@{}", dir.path().join("nope").display());
        let mut v = Vol::new("vol", &[("origin", &url)]);
        v.commit();
        let (events, sink_events) = sink();
        let mut pusher = v.pusher("origin", sink_events);
        let outcome = pusher.push_once();
        assert!(!format!("{outcome:?}").contains("hunter2"), "{outcome:?}");
        for event in events.take() {
            assert!(!event.to_string().contains("hunter2"), "{event}");
        }
        let saved = fs::read_to_string(v.state.push_state("origin")).unwrap();
        assert!(!saved.contains("hunter2"), "{saved}");
    }

    #[test]
    fn pushes_branch_to_a_bare_remote() {
        for file_url in [false, true] {
            let remote_dir = tempfile::tempdir().unwrap();
            bare(remote_dir.path());
            let url = if file_url {
                format!("file://{}", remote_dir.path().display())
            } else {
                remote_dir.path().display().to_string()
            };
            let mut v = Vol::new("vol", &[("origin", &url)]);
            let (events, sink_events) = sink();
            let mut pusher = v.pusher("origin", sink_events);
            assert_eq!(pusher.push_once(), PushOutcome::NothingToPush);
            assert_eq!(pusher.unpushed().unwrap(), None);

            v.commit();
            let head = v.commit();
            assert_eq!(pusher.unpushed().unwrap(), Some(2));
            let outcome = pusher.push_once();
            assert_eq!(
                outcome,
                PushOutcome::Pushed {
                    from: None,
                    to: head,
                    limited: None
                },
                "file url: {file_url}"
            );
            assert!(outcome.is_success());
            assert_eq!(
                remote_ref(remote_dir.path(), "refs/heads/agentfs/vol"),
                Some(head)
            );
            let saved = PushState::load(&v.state.push_state("origin"));
            assert_eq!(saved.last_pushed, Some(head.to_string()));
            assert_eq!(saved.consecutive_failures, 0);
            assert!(saved.last_success_ms.is_some());
            assert_eq!(pusher.unpushed().unwrap(), Some(0));
            assert_eq!(
                events.take(),
                [Event::PushSucceeded {
                    remote: "origin".into(),
                    head,
                    previous: None,
                }]
            );

            assert_eq!(pusher.push_once(), PushOutcome::UpToDate { head });
            let next = v.commit();
            assert_eq!(
                pusher.push_once(),
                PushOutcome::Pushed {
                    from: Some(head),
                    to: next,
                    limited: None
                }
            );
            assert_eq!(pusher.next_delay(), v.settings.push_interval);
        }
    }

    #[test]
    fn pushes_only_the_volumes_refs() {
        let remote_dir = tempfile::tempdir().unwrap();
        bare(remote_dir.path());
        let mut v = Vol::new(
            "vol",
            &[("origin", &remote_dir.path().display().to_string())],
        );
        let head = v.commit();
        let repo = v.com.history().repo();
        for name in [
            "refs/agentfs/vol/diverged/20260101T000000Z",
            "refs/agentfs/vol/inbox/0000000001",
            "refs/heads/other",
            "refs/agentfs/other/x",
            "refs/agentfs/volx/y",
        ] {
            repo.reference(name, head, false, "test").unwrap();
        }
        let mut pusher = v.pusher("origin", Arc::new(NullSink));
        assert!(pusher.push_once().is_success());
        // No status ref yet: no queue push.
        assert_eq!(pusher.queue(), None);
        assert_eq!(
            remote_refs(remote_dir.path()),
            [
                "refs/agentfs/vol/diverged/20260101T000000Z",
                "refs/heads/agentfs/vol"
            ]
        );
        // The queue's refs go with the queue push, inbox refs never.
        repo.reference("refs/agentfs/vol/status", head, false, "test")
            .unwrap();
        repo.reference("refs/agentfs/vol/applied/0000000001", head, false, "test")
            .unwrap();
        assert!(pusher.push_once().is_success());
        assert_eq!(
            remote_refs(remote_dir.path()),
            [
                "refs/agentfs/vol/applied/0000000001",
                "refs/agentfs/vol/diverged/20260101T000000Z",
                "refs/agentfs/vol/status",
                "refs/heads/agentfs/vol"
            ]
        );
    }

    #[test]
    fn only_the_patch_remote_gets_the_queue_push() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        bare(a.path());
        bare(b.path());
        let mut v = Vol::with_config(
            "vol",
            &[
                ("a", &a.path().display().to_string()),
                ("b", &b.path().display().to_string()),
            ],
            &[("agentfs.patchRemote", "b")],
        );
        let head = v.commit();
        v.com
            .history()
            .repo()
            .reference("refs/agentfs/vol/status", head, false, "t")
            .unwrap();
        for (name, dir, status) in [("a", a.path(), None), ("b", b.path(), Some(head))] {
            let mut pusher = v.pusher(name, Arc::new(NullSink));
            assert!(pusher.push_once().is_success());
            assert_eq!(pusher.queue().is_some(), status.is_some());
            assert_eq!(remote_ref(dir, "refs/agentfs/vol/status"), status);
        }
    }

    /// Push 1 creates the applied refs and forces the status; push 2
    /// deletes the inbox refs of applied numbers, stale ones included.
    #[test]
    fn queue_push_moves_applied_refs_and_deletes_applied_inbox_refs() {
        let remote_dir = tempfile::tempdir().unwrap();
        let remote = remote_dir.path();
        bare(remote);
        let mut v = Vol::new("vol", &[("origin", &remote.display().to_string())]);
        let c1 = v.commit();
        let c2 = v.commit();
        let mut pusher = v.pusher("origin", Arc::new(NullSink));
        assert!(pusher.push_once().is_success());
        let theirs = Repository::open_bare(remote).unwrap();
        theirs
            .reference("refs/agentfs/vol/inbox/0000000001", c1, false, "t")
            .unwrap();
        // Replaced by the producer after this volume applied c1 as 2.
        theirs
            .reference("refs/agentfs/vol/inbox/0000000002", c2, false, "t")
            .unwrap();
        let repo = v.com.history().repo();
        repo.reference("refs/agentfs/vol/applied/0000000001", c1, false, "t")
            .unwrap();
        repo.reference("refs/agentfs/vol/applied/0000000002", c1, false, "t")
            .unwrap();
        repo.reference("refs/agentfs/vol/status", c2, false, "t")
            .unwrap();

        assert!(pusher.push_once().is_success());
        assert_eq!(
            pusher.queue(),
            Some(&QueuePush {
                status: true,
                applied: 2,
                deleted: 2,
                held_back: false,
                error: None,
                branch_not_ours: None,
            })
        );
        assert_eq!(
            remote_ref(remote, "refs/agentfs/vol/applied/0000000001"),
            Some(c1)
        );
        assert_eq!(
            remote_ref(remote, "refs/agentfs/vol/inbox/0000000001"),
            None
        );
        // Stale: replaced after it was applied, never applied again.
        assert_eq!(
            remote_ref(remote, "refs/agentfs/vol/inbox/0000000002"),
            None
        );
        assert_eq!(remote_ref(remote, "refs/agentfs/vol/status"), Some(c2));
        let state = PushState::load(&v.state.push_state("origin"));
        assert_eq!(state.queue_synced_status, Some(c2.to_string()));
        assert!(state.queue_synced_ms.is_some());
        assert_eq!(state.queue_error, None);
        // Nothing new: no queue push.
        assert!(pusher.push_once().is_success());
        assert_eq!(pusher.queue(), None);
        // A new status is forced over whatever the remote has.
        repo.reference("refs/agentfs/vol/status", c1, true, "t")
            .unwrap();
        assert!(pusher.push_once().is_success());
        assert_eq!(remote_ref(remote, "refs/agentfs/vol/status"), Some(c1));
    }

    /// A queue push failure is the queue's: the branch succeeds, does not
    /// back off, and push 2 is skipped.
    #[test]
    fn queue_failure_never_touches_the_branch() {
        let remote_dir = tempfile::tempdir().unwrap();
        let remote = remote_dir.path();
        bare(remote);
        let mut v = Vol::new("vol", &[("origin", &remote.display().to_string())]);
        let c1 = v.commit();
        let (sink, events) = sink();
        let mut pusher = v.pusher("origin", events);
        assert!(pusher.push_once().is_success());
        let theirs = Repository::open_bare(remote).unwrap();
        theirs
            .reference("refs/agentfs/vol/applied/0000000001", c1, false, "t")
            .unwrap();
        theirs
            .reference("refs/agentfs/vol/inbox/0000000001", c1, false, "t")
            .unwrap();
        drop(theirs);
        // The remote's applied/1 moves to a commit this volume lacks.
        let other = write_remote(remote, "refs/agentfs/vol/applied/0000000001");
        let repo = v.com.history().repo();
        repo.reference("refs/agentfs/vol/applied/0000000001", c1, false, "t")
            .unwrap();
        repo.reference("refs/agentfs/vol/status", c1, false, "t")
            .unwrap();
        let c2 = v.commit();
        sink.take();

        let outcome = pusher.push_once();
        assert_eq!(
            outcome,
            PushOutcome::Pushed {
                from: Some(c1),
                to: c2,
                limited: None
            }
        );
        let queue = pusher.queue().unwrap();
        assert!(
            matches!(queue.error, Some((PushFailureKind::NonFastForward, _))),
            "{queue:?}"
        );
        assert_eq!(pusher.state().consecutive_failures, 0);
        assert_eq!(pusher.state().last_error, None);
        assert_eq!(pusher.next_delay(), v.settings.push_interval);
        let error = pusher.state().queue_error.clone().unwrap();
        assert_eq!(error.stage, PushStage::Queue);
        assert_eq!(
            remote_ref(remote, "refs/agentfs/vol/applied/0000000001"),
            Some(other)
        );
        // Push 2 did not run.
        assert_eq!(
            remote_ref(remote, "refs/agentfs/vol/inbox/0000000001"),
            Some(c1)
        );
        let failed: Vec<Event> = sink
            .take()
            .into_iter()
            .filter(|e| matches!(e, Event::PushFailed { .. }))
            .collect();
        assert!(
            matches!(
                failed.as_slice(),
                [Event::PushFailed {
                    stage: PushStage::Queue,
                    head: None,
                    ..
                }]
            ),
            "{failed:?}"
        );
        // It is retried at the next push even though the status is the same.
        assert!(pusher.push_once().is_success());
        assert!(pusher.queue().unwrap().error.is_some());
    }

    /// Applied refs wait for the branch; the status goes anyway, while the
    /// remote's branch is this worktree's (here: a push over the size
    /// limit fails before anything is sent).
    #[test]
    fn applied_refs_wait_for_the_branch_push() {
        let remote_dir = tempfile::tempdir().unwrap();
        let remote = remote_dir.path();
        bare(remote);
        let mut v = Vol::with_config(
            "vol",
            &[("origin", &remote.display().to_string())],
            &[("remote.origin.agentfsMaxPushSize", "1m")],
        );
        let c1 = v.commit();
        let mut pusher = v.pusher("origin", Arc::new(NullSink));
        assert!(pusher.push_once().is_success());
        let c2 = v.commit_file("big", &random(2 << 20, 5));
        let repo = v.com.history().repo();
        repo.reference("refs/agentfs/vol/applied/0000000001", c1, false, "t")
            .unwrap();
        repo.reference("refs/agentfs/vol/status", c2, false, "t")
            .unwrap();
        let outcome = pusher.push_once();
        assert!(
            matches!(
                outcome,
                PushOutcome::Failed {
                    kind: PushFailureKind::TooLarge,
                    ..
                }
            ),
            "{outcome:?}"
        );
        let queue = pusher.queue().unwrap();
        assert!(
            queue.status
                && queue.held_back
                && queue.error.is_none()
                && queue.branch_not_ours.is_none(),
            "{queue:?}"
        );
        assert_eq!(queue.applied, 0);
        assert_eq!(remote_ref(remote, "refs/agentfs/vol/status"), Some(c2));
        assert_eq!(
            remote_ref(remote, "refs/agentfs/vol/applied/0000000001"),
            None
        );
        assert_eq!(remote_ref(remote, "refs/heads/agentfs/vol"), Some(c1));
        assert_eq!(pusher.state().queue_synced_status, None);
    }

    /// F8 rule 2, acceptance 1: a queue push to a remote whose branch is
    /// another repository's writes no status, applied or inbox-deletion
    /// ref, and is not a failure (the branch push alarms).
    #[test]
    fn the_queue_push_writes_nothing_while_the_branch_is_not_ours() {
        for created_by_other in [true, false] {
            let remote_dir = tempfile::tempdir().unwrap();
            let remote = remote_dir.path();
            bare(remote);
            let mut v = Vol::new("vol", &[("origin", &remote.display().to_string())]);
            let c1 = v.commit();
            let theirs = if created_by_other {
                // Another repository created the branch first: unrelated.
                let other = Repository::open_bare(remote).unwrap();
                let sig = git2::Signature::now("other", "o@example.invalid").unwrap();
                let tree = other.treebuilder(None).unwrap().write().unwrap();
                let tree = other.find_tree(tree).unwrap();
                other
                    .commit(Some("refs/heads/agentfs/vol"), &sig, &sig, "x", &tree, &[])
                    .unwrap()
            } else {
                // Ours first, then another writer moved it.
                assert!(
                    v.pusher("origin", Arc::new(NullSink))
                        .push_once()
                        .is_success()
                );
                write_remote(remote, "refs/heads/agentfs/vol")
            };
            Repository::open_bare(remote)
                .unwrap()
                .reference("refs/agentfs/vol/inbox/0000000001", theirs, false, "t")
                .unwrap();
            let repo = v.com.history().repo();
            repo.reference("refs/agentfs/vol/applied/0000000001", c1, false, "t")
                .unwrap();
            repo.reference("refs/agentfs/vol/status", c1, false, "t")
                .unwrap();
            let before = remote_refs(remote);
            let (events, sink_events) = sink();
            let mut pusher = v.pusher("origin", sink_events);
            let outcome = pusher.push_once();
            assert!(
                matches!(outcome, PushOutcome::Diverged { .. }),
                "{outcome:?}"
            );
            let queue = pusher.queue().unwrap().clone();
            assert_eq!(queue.branch_not_ours, Some(Claim::Diverged), "{queue:?}");
            assert_eq!((queue.status, queue.applied, queue.deleted), (false, 0, 0));
            assert_eq!(queue.error, None);
            // Only the divergence save was added.
            let after: Vec<String> = remote_refs(remote)
                .into_iter()
                .filter(|r| !r.starts_with("refs/agentfs/vol/diverged/"))
                .collect();
            assert_eq!(after, before);
            assert_eq!(remote_ref(remote, "refs/heads/agentfs/vol"), Some(theirs));
            assert!(
                !events.take().iter().any(|e| matches!(
                    e,
                    Event::PushFailed {
                        stage: PushStage::Queue,
                        ..
                    }
                )),
                "a branch not ours is no queue failure"
            );
            assert_eq!(pusher.state().queue_error, None);
            assert_eq!(
                pusher.state().queue_synced_status,
                None,
                "retried next time"
            );
        }
    }

    /// No branch on the remote yet (the branch push failed before it was
    /// created): the queue push waits for it ([`Claim::Create`]).
    #[test]
    fn the_queue_push_waits_for_the_branch_to_exist() {
        let remote_dir = tempfile::tempdir().unwrap();
        let remote = remote_dir.path();
        bare(remote);
        let mut v = Vol::with_config(
            "vol",
            &[("origin", &remote.display().to_string())],
            &[("remote.origin.agentfsMaxPushSize", "1m")],
        );
        let big = v.commit_file("big", &random(2 << 20, 6));
        v.com
            .history()
            .repo()
            .reference("refs/agentfs/vol/status", big, false, "t")
            .unwrap();
        let mut pusher = v.pusher("origin", Arc::new(NullSink));
        assert!(!pusher.push_once().is_success());
        let queue = pusher.queue().unwrap();
        assert_eq!(queue.branch_not_ours, Some(Claim::Create), "{queue:?}");
        assert!(queue.summary().starts_with("nothing written"), "{queue:?}");
        assert!(remote_refs(remote).is_empty());
    }

    /// F8 rule 2, acceptance 1: the branch moved between the queue push's
    /// listing and its push. The no-op branch update it carries is leased
    /// on the listed value, so libgit2's negotiation stops the push.
    #[test]
    fn a_branch_moved_after_the_listing_stops_the_queue_push() {
        let remote_dir = tempfile::tempdir().unwrap();
        let remote = remote_dir.path().to_owned();
        bare(&remote);
        let url = format!("file://{}", remote.display());
        let mut v = Vol::new("vol", &[("origin", &url)]);
        let c1 = v.commit();
        let mut pusher = v.pusher("origin", Arc::new(NullSink));
        assert!(pusher.push_once().is_success());
        v.com
            .history()
            .repo()
            .reference("refs/agentfs/vol/status", c1, false, "t")
            .unwrap();
        let moved = remote.clone();
        pusher.after_queue_listing = Some(Box::new(move || {
            write_remote(&moved, "refs/heads/agentfs/vol");
        }));
        assert!(pusher.push_once().is_success(), "the branch was up to date");
        let queue = pusher.queue().unwrap();
        let Some((_, message)) = &queue.error else {
            panic!("the queue push went through: {queue:?}");
        };
        assert!(message.contains("changed on the remote"), "{message}");
        assert!(!queue.status);
        assert_eq!(remote_ref(&remote, "refs/agentfs/vol/status"), None);
        // The other writer's commit stays.
        assert_ne!(remote_ref(&remote, "refs/heads/agentfs/vol"), Some(c1));
    }

    /// Incompressible bytes: a xorshift stream.
    fn random(len: usize, seed: u64) -> Vec<u8> {
        let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        (0..len)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect()
    }

    #[test]
    fn a_backlog_over_the_push_size_limit_goes_out_in_steps() {
        let remote_dir = tempfile::tempdir().unwrap();
        let remote = remote_dir.path();
        bare(remote);
        let mut v = Vol::with_config(
            "vol",
            &[("origin", &remote.display().to_string())],
            &[
                ("remote.origin.agentfsMaxPushSize", "1m"),
                ("agentfs.patchRemote", "origin"),
            ],
        );
        let first = v.commit();
        let mut chain = Vec::new();
        for i in 0..5 {
            chain.push(v.commit_file(&format!("big{i}"), &random(400 << 10, i + 1)));
        }
        let repo = v.com.history().repo();
        repo.reference("refs/agentfs/vol/applied/0000000001", first, false, "t")
            .unwrap();
        repo.reference("refs/agentfs/vol/status", first, false, "t")
            .unwrap();
        let head = v.head();
        let (events, sink_events) = sink();
        let mut pusher = v.pusher("origin", sink_events);
        let mut steps = Vec::new();
        loop {
            let outcome = pusher.push_once();
            match outcome {
                PushOutcome::Pushed {
                    to,
                    limited: Some(left),
                    ..
                } => {
                    assert_eq!(remote_ref(remote, "refs/heads/agentfs/vol"), Some(to));
                    assert_eq!(pusher.next_delay(), Duration::ZERO);
                    // Applied refs wait for the full push.
                    let queue = pusher.queue().unwrap();
                    assert!(queue.held_back, "{queue:?}");
                    assert_eq!(
                        remote_ref(remote, "refs/agentfs/vol/applied/0000000001"),
                        None
                    );
                    steps.push((to, left));
                }
                PushOutcome::Pushed {
                    to, limited: None, ..
                } => {
                    assert_eq!(to, head);
                    break;
                }
                other => panic!("{other:?}"),
            }
            assert!(steps.len() < 10, "{steps:?}");
        }
        // 400 KiB each in 1 MiB: two commits per step (the first, small one
        // rides along with the first step).
        assert_eq!(steps, [(chain[1], 3), (chain[3], 1)]);
        assert_eq!(remote_ref(remote, "refs/heads/agentfs/vol"), Some(head));
        assert_eq!(
            remote_ref(remote, "refs/agentfs/vol/applied/0000000001"),
            Some(first)
        );
        assert_eq!(pusher.next_delay(), v.settings.push_interval);
        let limited: Vec<_> = events
            .take()
            .into_iter()
            .filter_map(|e| match e {
                Event::PushLimited {
                    pushed,
                    head: h,
                    commits_left,
                    limit,
                    ..
                } => Some((pushed, h, commits_left, limit)),
                _ => None,
            })
            .collect();
        assert_eq!(
            limited,
            [(chain[1], head, 3, 1 << 20), (chain[3], head, 1, 1 << 20)]
        );
        assert_eq!(pusher.push_once(), PushOutcome::UpToDate { head });
    }

    #[test]
    fn a_commit_over_the_push_size_limit_fails_as_too_large() {
        let remote_dir = tempfile::tempdir().unwrap();
        let remote = remote_dir.path();
        bare(remote);
        let mut v = Vol::with_config(
            "vol",
            &[("origin", &remote.display().to_string())],
            &[("remote.origin.agentfsMaxPushSize", "1m")],
        );
        let small = v.commit();
        let mut pusher = v.pusher("origin", Arc::new(NullSink));
        assert!(matches!(
            pusher.push_once(),
            PushOutcome::Pushed { limited: None, .. }
        ));
        let big = v.commit_file("big", &random(2 << 20, 3));
        let (events, sink_events) = sink();
        let mut pusher = v.pusher("origin", sink_events);
        let outcome = pusher.push_once();
        let PushOutcome::Failed {
            kind,
            message,
            stage,
            retry_in,
        } = outcome
        else {
            panic!("{outcome:?}");
        };
        assert_eq!(kind, PushFailureKind::TooLarge);
        assert_eq!(stage, PushStage::Git);
        assert_eq!(retry_in, v.settings.push_interval);
        assert!(message.contains(&big.to_string()), "{message}");
        assert!(message.contains("agentfsMaxPushSize"), "{message}");
        assert_eq!(remote_ref(remote, "refs/heads/agentfs/vol"), Some(small));
        assert_eq!(pusher.state().consecutive_failures, 1);
        assert!(matches!(
            events.take().as_slice(),
            [Event::PushFailed {
                kind: PushFailureKind::TooLarge,
                ..
            }]
        ));
        // Backs off like any failure.
        assert!(matches!(
            pusher.push_once(),
            PushOutcome::Failed {
                kind: PushFailureKind::TooLarge,
                ..
            }
        ));
        assert_eq!(pusher.next_delay(), v.settings.push_interval * 2);
    }

    #[test]
    fn remotes_without_a_push_size_limit_push_everything() {
        let remote_dir = tempfile::tempdir().unwrap();
        let remote = remote_dir.path();
        bare(remote);
        let mut v = Vol::new("vol", &[("origin", &remote.display().to_string())]);
        assert_eq!(v.settings.remotes.pushed()[0].max_push_size, None);
        let head = v.commit_file("big", &random(2 << 20, 5));
        let mut pusher = v.pusher("origin", Arc::new(NullSink));
        assert!(matches!(
            pusher.push_once(),
            PushOutcome::Pushed { limited: None, to, .. } if to == head
        ));
    }

    /// The push state lagging the remote (deleted as the history-rewrite
    /// runbook says, a renamed remote, a crash before the state save) must
    /// not make a limited push step from behind the remote: that pushes
    /// commits it has over newer ones, a false divergence every attempt.
    #[test]
    fn a_limited_push_measures_from_the_remote_branch() {
        let remote_dir = tempfile::tempdir().unwrap();
        let remote = remote_dir.path();
        bare(remote);
        let mut v = Vol::new("vol", &[("origin", &remote.display().to_string())]);
        v.commit();
        for i in 0..5 {
            v.commit_file(&format!("big{i}"), &random(400 << 10, i + 1));
        }
        // One commit alone over the limit, already on the remote.
        let pushed = v.commit_file("huge", &random(2 << 20, 9));
        let mut pusher = v.pusher("origin", Arc::new(NullSink));
        assert!(matches!(
            pusher.push_once(),
            PushOutcome::Pushed { limited: None, to, .. } if to == pushed
        ));
        let forget = |v: &Vol| {
            let path = v.state.push_state("origin");
            assert!(path.exists());
            fs::remove_file(path).unwrap();
        };
        forget(&v);
        v.settings.remotes.pushed_mut()[0].max_push_size = Some(1 << 20);

        // Up to date already: nothing measured, nothing refused.
        let (events, sink_events) = sink();
        let mut pusher = v.pusher("origin", sink_events);
        assert_eq!(pusher.push_once(), PushOutcome::UpToDate { head: pushed });
        assert_eq!(pusher.state().last_pushed_oid(), Some(pushed));
        assert_eq!(pusher.state().consecutive_failures, 0);
        assert!(diverged_refs(remote, "vol").is_empty());
        assert!(
            !events
                .take()
                .iter()
                .any(|e| matches!(e, Event::PushFailed { .. } | Event::Diverged { .. })),
        );

        // A backlog over the limit on top of the remote: stepped from the
        // remote's branch, not from the start of history.
        let mut chain = Vec::new();
        for i in 0..4 {
            chain.push(v.commit_file(&format!("more{i}"), &random(400 << 10, 20 + i)));
        }
        forget(&v);
        let mut pusher = v.pusher("origin", Arc::new(NullSink));
        let mut steps = Vec::new();
        loop {
            match pusher.push_once() {
                PushOutcome::Pushed {
                    from,
                    to,
                    limited: Some(left),
                } => steps.push((from, to, left)),
                PushOutcome::Pushed {
                    to, limited: None, ..
                } => {
                    assert_eq!(to, chain[3]);
                    break;
                }
                other => panic!("{other:?}"),
            }
            assert!(steps.len() < 10, "{steps:?}");
        }
        assert_eq!(steps, [(Some(pushed), chain[1], 2)]);
        assert_eq!(remote_ref(remote, "refs/heads/agentfs/vol"), Some(chain[3]));
        assert!(diverged_refs(remote, "vol").is_empty());
    }

    /// Design §11.5: create-only refs are created where absent, confirmed
    /// by a listing, and never moved.
    #[test]
    fn create_only_refs_are_created_and_never_moved() {
        let remote_dir = tempfile::tempdir().unwrap();
        bare(remote_dir.path());
        let url = remote_dir.path().display().to_string();
        let mut v = Vol::new("vol", &[("origin", &url)]);
        let first = v.commit();
        let second = v.commit();
        let mut pusher = v.pusher("origin", Arc::new(NullSink));
        let refs = [CreateOnlyRef {
            name: "refs/agentfs/vol/finish".into(),
            oid: second,
        }];
        assert_eq!(pusher.push_create_only(&refs), CreateOnlyOutcome::Held);
        assert_eq!(
            remote_ref(remote_dir.path(), "refs/agentfs/vol/finish"),
            Some(second)
        );
        // Again: already held, nothing sent.
        assert_eq!(pusher.push_create_only(&refs), CreateOnlyOutcome::Held);
        // A ref the remote holds elsewhere, even an ancestor a plain push
        // would fast-forward, is another writer's and stays.
        let other = [CreateOnlyRef {
            name: "refs/agentfs/vol/change".into(),
            oid: second,
        }];
        assert!(pusher.push_once().is_success());
        let remote = Repository::open_bare(remote_dir.path()).unwrap();
        remote
            .reference("refs/agentfs/vol/change", first, true, "test")
            .unwrap();
        assert_eq!(
            pusher.push_create_only(&other),
            CreateOnlyOutcome::AnotherWriter {
                name: "refs/agentfs/vol/change".into(),
                found: first,
            }
        );
        assert_eq!(
            remote_ref(remote_dir.path(), "refs/agentfs/vol/change"),
            Some(first)
        );
        // An unreachable remote fails and is retried.
        let missing = tempfile::tempdir().unwrap().path().join("nope");
        let mut gone = Vol::new("vol", &[("gone", &missing.display().to_string())]);
        let head = gone.commit();
        let mut pusher = gone.pusher("gone", Arc::new(NullSink));
        let outcome = pusher.push_create_only(&[CreateOnlyRef {
            name: "refs/agentfs/vol/finish".into(),
            oid: head,
        }]);
        assert!(
            matches!(outcome, CreateOnlyOutcome::Failed { .. }),
            "{outcome:?}"
        );
    }

    #[test]
    fn unreachable_remote_alarms_and_backs_off() {
        let missing = tempfile::tempdir().unwrap().path().join("nope");
        let mut v = Vol::new("vol", &[("gone", &missing.display().to_string())]);
        let head = v.commit();
        let (events, sink_events) = sink();
        let mut pusher = v.pusher("gone", sink_events);
        let mut retries = Vec::new();
        for n in 1..=2u32 {
            let outcome = pusher.push_once();
            let PushOutcome::Failed { kind, retry_in, .. } = outcome else {
                panic!("{outcome:?}");
            };
            assert_eq!(kind, PushFailureKind::Unreachable);
            retries.push(retry_in);
            let got = events.take();
            assert!(
                matches!(
                    &got[..],
                    [Event::PushFailed { remote, kind: PushFailureKind::Unreachable, consecutive_failures, head: Some(h), last_pushed: None, .. }]
                        if remote == "gone" && *consecutive_failures == n && *h == head
                ),
                "{got:?}"
            );
        }
        assert_eq!(retries, [Duration::from_secs(60), Duration::from_secs(120)]);
        assert_eq!(pusher.next_delay(), Duration::from_secs(120));
        let saved = PushState::load(&v.state.push_state("gone"));
        assert_eq!(saved.consecutive_failures, 2);
        assert_eq!(
            saved.last_error.map(|e| e.kind),
            Some(PushFailureKind::Unreachable)
        );
        // Recording and committing carry on.
        let next = v.commit();
        assert_ne!(next, head);
        assert_eq!(pusher.unpushed().unwrap(), Some(2));
    }

    #[test]
    fn refused_push_then_recovery() {
        let remote_dir = tempfile::tempdir().unwrap();
        bare(remote_dir.path());
        let mut v = Vol::new(
            "vol",
            &[("origin", &remote_dir.path().display().to_string())],
        );
        let head = v.commit();
        let lock = remote_dir.path().join("refs/heads/agentfs/vol.lock");
        fs::create_dir_all(lock.parent().unwrap()).unwrap();
        fs::write(&lock, "").unwrap();

        let (events, sink_events) = sink();
        let mut pusher = v.pusher("origin", sink_events);
        let outcome = pusher.push_once();
        assert!(
            matches!(&outcome, PushOutcome::Failed { kind: PushFailureKind::Refused, message, .. }
                if message.contains("refs/heads/agentfs/vol")),
            "{outcome:?}"
        );
        assert!(matches!(
            &events.take()[..],
            [Event::PushFailed {
                kind: PushFailureKind::Refused,
                ..
            }]
        ));
        assert_eq!(
            remote_ref(remote_dir.path(), "refs/heads/agentfs/vol"),
            None
        );

        fs::remove_file(&lock).unwrap();
        assert!(pusher.push_once().is_success());
        assert_eq!(pusher.state().consecutive_failures, 0);
        assert_eq!(pusher.state().last_error, None);
        assert_eq!(
            remote_ref(remote_dir.path(), "refs/heads/agentfs/vol"),
            Some(head)
        );
    }

    /// A push cancelled at shutdown stops at its negotiation: nothing
    /// reaches the remote, and no ref of it is locked or moved.
    #[test]
    fn a_cancelled_push_sends_nothing() {
        let remote_dir = tempfile::tempdir().unwrap();
        bare(remote_dir.path());
        let mut v = Vol::new(
            "vol",
            &[("origin", &remote_dir.path().display().to_string())],
        );
        v.commit();
        let (events, sink_events) = sink();
        let mut pusher = v.pusher("origin", sink_events);
        assert_eq!(pusher.stop_handle().cancel(), PushPhase::Idle);
        let outcome = pusher.push_once();
        assert_eq!(outcome, PushOutcome::Cancelled);
        assert_eq!(
            remote_ref(remote_dir.path(), "refs/heads/agentfs/vol"),
            None
        );
        // Not the remote's failure: no alarm, no failure counted or saved.
        assert!(events.take().is_empty());
        assert_eq!(pusher.state().consecutive_failures, 0);
        assert_eq!(pusher.state().last_error, None);
        let saved = PushState::load(&v.state.push_state("origin"));
        assert_eq!(saved.consecutive_failures, 0);
        assert_eq!(saved.last_error, None);
    }

    /// A push URL that `url.<base>.insteadOf` rewrites to a path is a
    /// local push: libgit2 updates that remote in this process, so it is
    /// waited for at shutdown like any path remote.
    #[test]
    fn an_insteadof_rewrite_to_a_path_is_a_local_push() {
        let remote_dir = tempfile::tempdir().unwrap();
        bare(remote_dir.path());
        let url = "git://example.invalid/r.git";
        let mut v = Vol::with_config(
            "vol",
            &[("origin", url)],
            &[(
                &format!("url.{}.insteadOf", remote_dir.path().display()),
                url,
            )],
        );
        let head = v.commit();
        let (_events, sink_events) = sink();
        let mut pusher = v.pusher("origin", sink_events);
        let stop = pusher.stop_handle();
        let seen = Arc::new(std::sync::Mutex::new(None));
        let at = seen.clone();
        let phase = stop.clone();
        pusher.set_after_negotiation(move || *at.lock().unwrap() = Some(phase.phase()));
        assert!(pusher.push_once().is_success());
        assert_eq!(
            remote_ref(remote_dir.path(), "refs/heads/agentfs/vol"),
            Some(head),
            "the rewrite took the push to the path"
        );
        assert_eq!(*seen.lock().unwrap(), Some(PushPhase::Updating));
    }

    /// Another writer adds a commit on top of the remote branch.
    fn write_remote(remote: &Path, branch: &str) -> Oid {
        let repo = Repository::open_bare(remote).unwrap();
        let parent = repo
            .find_commit(repo.refname_to_id(branch).unwrap())
            .unwrap();
        let sig = git2::Signature::now("other", "other@example.invalid").unwrap();
        let tree = parent.tree().unwrap();
        repo.commit(Some(branch), &sig, &sig, "someone else", &tree, &[&parent])
            .unwrap()
    }

    fn diverged_refs(remote: &Path, volume: &str) -> Vec<(String, Oid)> {
        let repo = Repository::open_bare(remote).unwrap();
        repo.references_glob(&format!("refs/agentfs/{volume}/diverged/*"))
            .unwrap()
            .map(|r| {
                let r = r.unwrap();
                (r.name().unwrap().to_owned(), r.target().unwrap())
            })
            .collect()
    }

    #[test]
    fn divergence_by_another_writer() {
        let remote_dir = tempfile::tempdir().unwrap();
        let remote = remote_dir.path();
        bare(remote);
        let mut v = Vol::new("vol", &[("origin", &remote.display().to_string())]);
        let pushed = v.commit();
        let (events, sink_events) = sink();
        let mut pusher = v.pusher("origin", sink_events);
        assert!(pusher.push_once().is_success());
        events.take();

        let theirs = write_remote(remote, "refs/heads/agentfs/vol");
        let ours = v.commit();
        let outcome = pusher.push_once();
        let PushOutcome::Diverged {
            local_head,
            remote_head,
            diverged_ref: Some(saved_as),
        } = &outcome
        else {
            panic!("{outcome:?}");
        };
        assert_eq!((*local_head, *remote_head), (ours, Some(theirs)));
        assert!(!outcome.is_success());
        assert_eq!(diverged_refs(remote, "vol"), [(saved_as.clone(), ours)]);
        assert_eq!(
            remote_ref(remote, "refs/heads/agentfs/vol"),
            Some(theirs),
            "never forced"
        );
        let got = events.take();
        assert!(
            matches!(
                &got[..],
                [
                    Event::Diverged { diverged_ref, local_head, remote_head: Some(r), .. },
                    Event::PushFailed { kind: PushFailureKind::NonFastForward, consecutive_failures: 1, .. },
                ] if diverged_ref == saved_as && *local_head == ours && *r == theirs
            ),
            "{got:?}"
        );
        let divergence = pusher.state().diverged.clone().unwrap();
        assert_eq!(divergence.diverged_ref.as_ref(), Some(saved_as));

        // Later attempts keep alarming but save the head only once.
        let again = pusher.push_once();
        assert!(
            matches!(&again, PushOutcome::Diverged { diverged_ref: Some(r), .. } if r == saved_as),
            "{again:?}"
        );
        let got = events.take();
        assert!(
            matches!(
                &got[..],
                [Event::PushFailed {
                    kind: PushFailureKind::NonFastForward,
                    consecutive_failures: 2,
                    ..
                }]
            ),
            "{got:?}"
        );
        assert_eq!(diverged_refs(remote, "vol").len(), 1);
        assert_eq!(
            pusher.state().diverged.as_ref().unwrap().since_ms,
            divergence.since_ms
        );

        // The operator resolves it by resetting the remote branch.
        Repository::open_bare(remote)
            .unwrap()
            .reference("refs/heads/agentfs/vol", pushed, true, "operator reset")
            .unwrap();
        assert_eq!(
            pusher.push_once(),
            PushOutcome::Pushed {
                from: Some(pushed),
                to: ours,
                limited: None
            }
        );
        assert_eq!(pusher.state().diverged, None);
        assert_eq!(pusher.state().consecutive_failures, 0);
    }

    /// A crash after the diverged ref reached the remote but before the
    /// state recorded it: the restart reuses the recorded name.
    #[test]
    fn divergence_saved_once_across_a_crash() {
        let remote_dir = tempfile::tempdir().unwrap();
        let remote = remote_dir.path();
        bare(remote);
        let url = remote.display().to_string();
        let mut v = Vol::new("vol", &[("origin", &url)]);
        v.commit();
        assert!(
            v.pusher("origin", Arc::new(NullSink))
                .push_once()
                .is_success()
        );
        write_remote(remote, "refs/heads/agentfs/vol");
        let ours = v.commit();

        // What the crashed process left behind: the name recorded as
        // pending, and the head already pushed under it.
        let name = diverged_ref("vol", 1);
        let state_path = v.state.push_state("origin");
        let mut state = PushState::load(&state_path);
        state.diverged = Some(Divergence {
            since_ms: 1000,
            local_head: ours.to_string(),
            remote_head: None,
            diverged_ref: None,
            pending_ref: Some(name.clone()),
            saved_head: None,
        });
        state.save(&state_path).unwrap();
        v.com
            .history()
            .repo()
            .remote_anonymous(&url)
            .unwrap()
            .push(&[format!("{ours}:{name}")], None)
            .unwrap();

        // The workload carries on, then the service restarts.
        let newer = v.commit();
        let (events, sink_events) = sink();
        let mut pusher = v.pusher("origin", sink_events);
        let outcome = pusher.push_once();
        assert!(
            matches!(&outcome, PushOutcome::Diverged { diverged_ref: Some(r), .. } if *r == name),
            "{outcome:?}"
        );
        assert_eq!(diverged_refs(remote, "vol"), [(name.clone(), newer)]);
        assert!(matches!(
            &events.take()[..],
            [Event::Diverged { diverged_ref, .. }, Event::PushFailed { .. }] if *diverged_ref == name
        ));
        let saved = PushState::load(&state_path).diverged.unwrap();
        assert_eq!(saved.diverged_ref, Some(name));
        assert_eq!(saved.pending_ref, None);
        assert_eq!(saved.since_ms, 1000);
    }

    /// F8 D3, acceptance 2: the divergence save follows the head, so a
    /// diverged worktree's later commits reach the remote under its own
    /// name; a save another writer moved is kept, never overwritten.
    #[test]
    fn the_divergence_save_follows_the_head() {
        let remote_dir = tempfile::tempdir().unwrap();
        let remote = remote_dir.path();
        bare(remote);
        let mut v = Vol::new("vol", &[("origin", &remote.display().to_string())]);
        v.commit();
        let (events, sink_events) = sink();
        let mut pusher = v.pusher("origin", sink_events);
        assert!(pusher.push_once().is_success());
        let theirs = write_remote(remote, "refs/heads/agentfs/vol");
        let first = v.commit();
        let PushOutcome::Diverged {
            diverged_ref: Some(name),
            ..
        } = pusher.push_once()
        else {
            panic!("not diverged");
        };
        assert_eq!(diverged_refs(remote, "vol"), [(name.clone(), first)]);
        events.take();

        // Two more commits: the save fast-forwards to the new head, with no
        // second Diverged and no second save.
        v.commit();
        let head = v.commit();
        let outcome = pusher.push_once();
        assert!(
            matches!(&outcome, PushOutcome::Diverged { diverged_ref: Some(r), local_head, .. }
                if *r == name && *local_head == head),
            "{outcome:?}"
        );
        assert_eq!(diverged_refs(remote, "vol"), [(name.clone(), head)]);
        let got = events.take();
        assert!(
            matches!(
                &got[..],
                [Event::PushFailed {
                    kind: PushFailureKind::NonFastForward,
                    ..
                }]
            ),
            "{got:?}"
        );
        let saved = PushState::load(&v.state.push_state("origin"))
            .diverged
            .unwrap();
        assert_eq!(saved.saved_head, Some(head.to_string()));
        assert_eq!(remote_ref(remote, "refs/heads/agentfs/vol"), Some(theirs));

        // The save deleted on the remote is re-created at the next head.
        Repository::open_bare(remote)
            .unwrap()
            .find_reference(&name)
            .unwrap()
            .delete()
            .unwrap();
        let head = v.commit();
        pusher.push_once();
        assert_eq!(diverged_refs(remote, "vol"), [(name.clone(), head)]);

        // Another writer moves the save: kept, and the alarm says so.
        let moved = write_remote(remote, &name);
        v.commit();
        let outcome = pusher.push_once();
        assert!(
            matches!(&outcome, PushOutcome::Diverged { diverged_ref: Some(r), .. } if *r == name),
            "{outcome:?}"
        );
        assert_eq!(diverged_refs(remote, "vol"), [(name.clone(), moved)]);
        let got = events.take();
        assert!(
            got.iter()
                .any(|e| matches!(e, Event::PushFailed { message, .. }
                if message.contains("another writer moved it"))),
            "{got:?}"
        );
        let saved = pusher.state().diverged.clone().unwrap();
        assert_eq!(
            saved.saved_head,
            Some(head.to_string()),
            "the last head it reached"
        );
    }

    /// Writes a commit unrelated to anything else into `remote` at `name`.
    fn write_unrelated(remote: &Path, name: &str) -> Oid {
        let repo = Repository::open_bare(remote).unwrap();
        let sig = git2::Signature::now("other", "o@example.invalid").unwrap();
        let blob = repo.blob(b"unrelated").unwrap();
        let mut tree = repo.treebuilder(None).unwrap();
        tree.insert("u", blob, 0o100644).unwrap();
        let tree = repo.find_tree(tree.write().unwrap()).unwrap();
        let oid = repo
            .commit(None, &sig, &sig, "unrelated", &tree, &[])
            .unwrap();
        repo.reference(name, oid, true, "t").unwrap();
        oid
    }

    /// F8 S2, acceptance 3: `push_with_lease` replaces the volume's own
    /// branch only when the remote holds the lease and the lease shares a
    /// commit with the head, and clears the divergence.
    #[test]
    fn a_lease_push_replaces_only_the_leased_branch() {
        let remote_dir = tempfile::tempdir().unwrap();
        let remote = remote_dir.path();
        bare(remote);
        let mut v = Vol::new("vol", &[("origin", &remote.display().to_string())]);
        v.commit();
        let mut pusher = v.pusher("origin", Arc::new(NullSink));
        assert!(pusher.push_once().is_success());
        // Another writer's commit: not in this repository.
        let theirs = write_remote(remote, "refs/heads/agentfs/vol");
        let head = v.commit();
        assert!(matches!(pusher.push_once(), PushOutcome::Diverged { .. }));
        assert!(
            v.com.history().repo().find_commit(theirs).is_err(),
            "the other writer's commit is unknown here"
        );
        let before = remote_refs(remote);

        // A wrong lease: nothing written, nothing fetched.
        assert_eq!(
            pusher.push_with_lease(head),
            LeaseOutcome::LeaseBroken {
                found: Some(theirs)
            }
        );
        assert_eq!(remote_refs(remote), before);
        assert_eq!(remote_ref(remote, "refs/heads/agentfs/vol"), Some(theirs));
        assert!(pusher.state().diverged.is_some());

        // The right lease, not in the repository: fetched into a scratch
        // repository first (the state area never takes it in), then the
        // branch is replaced and the divergence cleared.
        assert_eq!(
            pusher.push_with_lease(theirs),
            LeaseOutcome::Pushed {
                from: theirs,
                to: head
            }
        );
        assert_eq!(remote_ref(remote, "refs/heads/agentfs/vol"), Some(head));
        assert!(v.com.history().repo().find_commit(theirs).is_err());
        assert!(!pusher.lock_path().with_extension("lease.git").exists());
        let state = PushState::load(&v.state.push_state("origin"));
        assert_eq!(state.diverged, None);
        assert_eq!(state.last_pushed, Some(head.to_string()));
        assert_eq!(state.consecutive_failures, 0);
        // Only the branch moved: the diverged save stays.
        assert_eq!(diverged_refs(remote, "vol").len(), 1);
        assert_eq!(
            pusher.push_with_lease(theirs),
            LeaseOutcome::UpToDate { head }
        );
        assert!(pusher.push_once().is_success());

        // An unrelated history on the remote is never replaced.
        let unrelated = write_unrelated(remote, "refs/heads/agentfs/vol");
        v.commit();
        assert_eq!(
            pusher.push_with_lease(unrelated),
            LeaseOutcome::Unrelated {
                lease: unrelated,
                head: v.head()
            }
        );
        assert_eq!(
            remote_ref(remote, "refs/heads/agentfs/vol"),
            Some(unrelated)
        );

        // No branch on the remote: the lease is broken.
        Repository::open_bare(remote)
            .unwrap()
            .find_reference("refs/heads/agentfs/vol")
            .unwrap()
            .delete()
            .unwrap();
        assert_eq!(
            pusher.push_with_lease(unrelated),
            LeaseOutcome::LeaseBroken { found: None }
        );
        assert_eq!(remote_ref(remote, "refs/heads/agentfs/vol"), None);
    }

    #[test]
    fn a_lease_push_waits_for_no_lock_and_needs_a_branch() {
        let remote_dir = tempfile::tempdir().unwrap();
        bare(remote_dir.path());
        let mut v = Vol::new(
            "vol",
            &[("origin", &remote_dir.path().display().to_string())],
        );
        let mut pusher = v.pusher("origin", Arc::new(NullSink));
        let lease = Oid::from_bytes(&[7; 20]).unwrap();
        assert_eq!(pusher.push_with_lease(lease), LeaseOutcome::NothingToPush);
        v.commit();
        let held = v.pusher("origin", Arc::new(NullSink));
        let _lock = held.lock().unwrap().unwrap();
        assert_eq!(pusher.push_with_lease(lease), LeaseOutcome::Busy);
    }

    /// Writes onto `remote`'s `branch` a commit an agentfs recorder made
    /// (it carries `Agentfs-Seq`) that this repository lacks: the volume's
    /// newer history, written from a newer copy of its state area.
    fn write_remote_recorded(remote: &Path, branch: &str, seq: u64) -> Oid {
        let repo = Repository::open_bare(remote).unwrap();
        let parent = repo
            .find_commit(repo.refname_to_id(branch).unwrap())
            .unwrap();
        let sig = git2::Signature::now("agent", "agent@example.invalid").unwrap();
        let tree = parent.tree().unwrap();
        let message = format!("close f9\n\nAgentfs-Seq: {seq}\nAgentfs-Trigger: close\n");
        repo.commit(Some(branch), &sig, &sig, &message, &tree, &[&parent])
            .unwrap()
    }

    /// Bead agentfs-4lb.1: a lease push interrupted while the remote's old
    /// history sits in its scratch repository (here a panic, unwinding)
    /// leaves no scratch repository behind.
    #[test]
    fn an_interrupted_lease_push_leaves_no_scratch_repository() {
        let remote_dir = tempfile::tempdir().unwrap();
        let remote = remote_dir.path();
        bare(remote);
        let mut v = Vol::new("vol", &[("origin", &remote.display().to_string())]);
        v.commit();
        let mut pusher = v.pusher("origin", Arc::new(NullSink));
        assert!(pusher.push_once().is_success());
        let theirs = write_remote(remote, "refs/heads/agentfs/vol");
        v.commit();
        let scratch = pusher.lock_path().with_extension("lease.git");
        let seen = scratch.clone();
        pusher.in_lease_scratch = Some(Box::new(move || {
            assert!(seen.join("objects").exists(), "the lease was fetched aside");
            panic!("interrupted mid-push");
        }));
        let interrupted = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            pusher.push_with_lease(theirs)
        }));
        assert!(interrupted.is_err(), "the hook ran");
        assert!(
            !scratch.exists(),
            "the remote's old history is left at {}",
            scratch.display()
        );
        assert_eq!(remote_ref(remote, "refs/heads/agentfs/vol"), Some(theirs));
    }

    /// Bead agentfs-4lb.1: a scratch repository a killed lease push left
    /// (for this remote, or one no longer configured) is removed by the
    /// next push of any remote; one whose remote's push lock another
    /// process holds is in use and stays.
    #[test]
    fn a_push_sweeps_lease_scratch_repositories_a_killed_push_left() {
        let remote_dir = tempfile::tempdir().unwrap();
        bare(remote_dir.path());
        let mut v = Vol::new(
            "vol",
            &[("origin", &remote_dir.path().display().to_string())],
        );
        v.commit();
        let dir = v.state.push_dir();
        fs::create_dir_all(&dir).unwrap();
        let stale = |name: &str| {
            let scratch = dir.join(format!("{name}.lease.git"));
            Repository::init_bare(&scratch).unwrap();
            scratch
        };
        let own = stale("origin");
        let gone = stale("gone.remote");
        let busy = stale("busy");
        let held = FileLock::try_take(&dir.join("busy.lock"), Mode::Exclusive)
            .unwrap()
            .unwrap();

        let mut pusher = v.pusher("origin", Arc::new(NullSink));
        assert!(pusher.push_once().is_success());
        assert!(!own.exists(), "{} is left", own.display());
        assert!(!gone.exists(), "{} is left", gone.display());
        assert!(busy.exists(), "a scratch in use stays");
        drop(held);
    }

    /// Bead agentfs-4lb.2, F8 "one branch per worktree": the lease push
    /// replaces a foreign writer's commits (divergence case A) and refuses
    /// a branch holding history an agentfs recorder wrote that this state
    /// area lacks (case B: a stale state area; the branch is not its to
    /// replace).
    #[test]
    fn a_lease_push_replaces_case_a_and_refuses_case_b() {
        let a_dir = tempfile::tempdir().unwrap();
        let b_dir = tempfile::tempdir().unwrap();
        bare(a_dir.path());
        bare(b_dir.path());
        let mut v = Vol::new(
            "vol",
            &[
                ("a", &a_dir.path().display().to_string()),
                ("b", &b_dir.path().display().to_string()),
            ],
        );
        v.commit();
        let mut a = v.pusher("a", Arc::new(NullSink));
        let mut b = v.pusher("b", Arc::new(NullSink));
        assert!(a.push_once().is_success());
        assert!(b.push_once().is_success());
        let branch = "refs/heads/agentfs/vol";
        let seq = v.com.history().head_seq().unwrap().unwrap();
        // Case A: a person's commit on the volume's branch.
        let foreign = write_remote(a_dir.path(), branch);
        // Case B: this volume's own newer history, recorded elsewhere.
        let newer = write_remote_recorded(b_dir.path(), branch, seq + 1);
        let head = v.commit();
        assert!(matches!(a.push_once(), PushOutcome::Diverged { .. }));
        assert!(matches!(b.push_once(), PushOutcome::Diverged { .. }));

        assert_eq!(
            a.push_with_lease(foreign),
            LeaseOutcome::Pushed {
                from: foreign,
                to: head
            }
        );
        assert_eq!(remote_ref(a_dir.path(), branch), Some(head));

        let outcome = b.push_with_lease(newer);
        assert_eq!(
            remote_ref(b_dir.path(), branch),
            Some(newer),
            "case B is never replaced: {outcome:?}"
        );
        assert_eq!(outcome, LeaseOutcome::Recorded { commit: newer });
        assert!(b.state().diverged.is_some(), "still diverged");
        assert!(!b.lock_path().with_extension("lease.git").exists());
        assert!(
            v.com.history().repo().find_commit(newer).is_err(),
            "the remote's history was not taken in"
        );
    }

    /// Case B as it happens: the newer copy of the state area recorded the
    /// same `Agentfs-Seq` with the same author and committer as this stale
    /// one, at another time. Only the time tells the records apart, and the
    /// branch is refused.
    #[test]
    fn a_lease_push_refuses_the_same_record_made_at_another_time() {
        let remote_dir = tempfile::tempdir().unwrap();
        let remote = remote_dir.path();
        bare(remote);
        let mut v = Vol::new("vol", &[("origin", &remote.display().to_string())]);
        let pushed = v.commit();
        let mut pusher = v.pusher("origin", Arc::new(NullSink));
        assert!(pusher.push_once().is_success());
        let stale = v.commit();
        let repo = v.com.history().repo();
        let ours = repo.find_commit(stale).unwrap();
        let later = |sig: git2::Signature| {
            git2::Signature::new(
                sig.name().unwrap(),
                sig.email().unwrap(),
                &git2::Time::new(sig.when().seconds() + 3600, sig.when().offset_minutes()),
            )
            .unwrap()
        };
        let theirs = {
            let remote_repo = Repository::open_bare(remote).unwrap();
            let parent = remote_repo.find_commit(pushed).unwrap();
            remote_repo
                .commit(
                    Some("refs/heads/agentfs/vol"),
                    &later(ours.author()),
                    &later(ours.committer()),
                    ours.message().unwrap(),
                    &parent.tree().unwrap(),
                    &[&parent],
                )
                .unwrap()
        };
        assert!(matches!(pusher.push_once(), PushOutcome::Diverged { .. }));
        assert_eq!(
            pusher.push_with_lease(theirs),
            LeaseOutcome::Recorded { commit: theirs }
        );
        assert_eq!(remote_ref(remote, "refs/heads/agentfs/vol"), Some(theirs));
    }

    /// The history-rewrite runbook's step 9: the remote's old commits were
    /// recorded by agentfs, and the head holds each one rewritten (the same
    /// `Agentfs-Seq`, author and committer; another tree): replaced.
    #[test]
    fn a_lease_push_replaces_a_history_the_head_rewrote() {
        let remote_dir = tempfile::tempdir().unwrap();
        let remote = remote_dir.path();
        bare(remote);
        let mut v = Vol::new("vol", &[("origin", &remote.display().to_string())]);
        let clean = v.commit();
        v.commit_file("secret", b"s3cret");
        let old = v.commit();
        let mut pusher = v.pusher("origin", Arc::new(NullSink));
        assert!(pusher.push_once().is_success());
        // filter-repo, in short: every commit since `clean` again, with the
        // secret dropped from its tree and nothing else changed.
        let repo = v.com.history().repo();
        let mut walk = repo.revwalk().unwrap();
        walk.push(old).unwrap();
        walk.hide(clean).unwrap();
        walk.set_sorting(git2::Sort::TOPOLOGICAL | git2::Sort::REVERSE)
            .unwrap();
        let mut parent = repo.find_commit(clean).unwrap();
        for oid in walk {
            let commit = repo.find_commit(oid.unwrap()).unwrap();
            let mut tree = repo.treebuilder(Some(&commit.tree().unwrap())).unwrap();
            if tree.get("secret").unwrap().is_some() {
                tree.remove("secret").unwrap();
            }
            let tree = repo.find_tree(tree.write().unwrap()).unwrap();
            let rewritten = repo
                .commit(
                    None,
                    &commit.author(),
                    &commit.committer(),
                    commit.message().unwrap(),
                    &tree,
                    &[&parent],
                )
                .unwrap();
            parent = repo.find_commit(rewritten).unwrap();
        }
        let new = parent.id();
        repo.reference("refs/heads/agentfs/vol", new, true, "rewrite")
            .unwrap();
        assert_eq!(
            pusher.push_with_lease(old),
            LeaseOutcome::Pushed { from: old, to: new }
        );
        assert_eq!(remote_ref(remote, "refs/heads/agentfs/vol"), Some(new));
    }

    /// F8 rule 2: the branch push decides in libgit2's negotiation, from
    /// the old value the remote reports, and a branch another repository
    /// wrote stops the push before any ref is sent.
    #[test]
    fn the_negotiation_stops_a_push_to_a_branch_another_repository_wrote() {
        let remote_dir = tempfile::tempdir().unwrap();
        let remote = remote_dir.path();
        bare(remote);
        let url = format!("file://{}", remote.display());
        let mut v = Vol::new("vol", &[("origin", &url)]);
        v.commit();
        let mut pusher = v.pusher("origin", Arc::new(NullSink));
        assert!(pusher.push_once().is_success());
        let theirs = write_remote(remote, "refs/heads/agentfs/vol");
        let ours = v.commit();
        // A ref that would go out in the same push as the branch.
        let accepted = crate::fork::identity::accepted_ref("vol", ours);
        v.com
            .history()
            .repo()
            .reference(&accepted, ours, false, "t")
            .unwrap();
        let (events, sink_events) = sink();
        let mut pusher = v.pusher("origin", sink_events);
        let outcome = pusher.push_once();
        assert!(
            matches!(&outcome, PushOutcome::Diverged { local_head, remote_head: Some(r), diverged_ref: Some(_) }
                if *local_head == ours && *r == theirs),
            "{outcome:?}"
        );
        assert_eq!(
            remote_ref(remote, &accepted),
            None,
            "nothing of the branch push was sent"
        );
        assert_eq!(remote_ref(remote, "refs/heads/agentfs/vol"), Some(theirs));
        let got = events.take();
        assert!(
            matches!(&got[..], [Event::Diverged { .. }, Event::PushFailed { kind: PushFailureKind::NonFastForward, message, .. }]
                if message.contains("nothing was sent")),
            "{got:?}"
        );
    }

    /// F8 rule 3: what a remote advertises for the branch, judged against
    /// the head, without pushing or touching the push state.
    #[test]
    fn branch_claim_asks_the_remote() {
        let remote_dir = tempfile::tempdir().unwrap();
        let remote = remote_dir.path();
        bare(remote);
        let mut v = Vol::new(
            "vol",
            &[
                ("origin", &remote.display().to_string()),
                ("gone", &remote.join("nope").display().to_string()),
            ],
        );
        let first = v.commit();
        let pusher = v.pusher("origin", Arc::new(NullSink));
        assert_eq!(pusher.branch_claim(first), Ok(Claim::Create));
        assert!(
            v.pusher("origin", Arc::new(NullSink))
                .push_once()
                .is_success()
        );
        let before = fs::read(v.state.push_state("origin")).unwrap();
        assert_eq!(pusher.branch_claim(first), Ok(Claim::UpToDate));
        let second = v.commit();
        assert_eq!(pusher.branch_claim(second), Ok(Claim::FastForward));
        write_remote(remote, "refs/heads/agentfs/vol");
        assert_eq!(pusher.branch_claim(second), Ok(Claim::Diverged));
        assert_eq!(
            fs::read(v.state.push_state("origin")).unwrap(),
            before,
            "the push state is untouched"
        );
        let Err(unreachable) = v.pusher("gone", Arc::new(NullSink)).branch_claim(second) else {
            panic!("a missing remote cannot be listed");
        };
        assert_eq!(unreachable.kind, PushFailureKind::Unreachable);
    }

    /// F8 R9 and D2: a started volume raises StartBranchMoved at every
    /// push to its start remote while the old branch holds commits it
    /// does not have, and an accepted tip clears it (and is pushed,
    /// create-only, with the branch).
    #[test]
    fn the_start_branch_alarm_and_an_accepted_tip() {
        let remote_dir = tempfile::tempdir().unwrap();
        let remote = remote_dir.path();
        bare(remote);
        let url = remote.display().to_string();
        let mut v1 = Vol::new("v1", &[("origin", &url)]);
        let f = v1.commit();
        assert!(
            v1.pusher("origin", Arc::new(NullSink))
                .push_once()
                .is_success()
        );
        let f_text = f.to_string();
        let mut v2 = Vol::with_config(
            "v2",
            &[("origin", &url)],
            &[
                ("agentfs.start.remote", "origin"),
                ("agentfs.start.branch", "agentfs/v1"),
                ("agentfs.start.commit", &f_text),
            ],
        );
        assert_eq!(
            v2.settings.start.as_ref().map(ToString::to_string),
            Some(format!("origin:agentfs/v1@{f}"))
        );
        v2.commit();
        let (events, sink_events) = sink();
        let mut pusher = v2.pusher("origin", sink_events);
        assert!(pusher.push_once().is_success());
        assert!(
            !events
                .take()
                .iter()
                .any(|e| matches!(e, Event::StartBranchMoved { .. })),
            "at the pin: no alarm"
        );
        assert_eq!(pusher.state().start_moved, None);

        // The old node's late commit reaches its own branch.
        let g = v1.commit();
        assert!(
            v1.pusher("origin", Arc::new(NullSink))
                .push_once()
                .is_success()
        );
        v2.commit();
        for _ in 0..2 {
            assert!(
                pusher.push_once().is_success(),
                "the alarm never stops the push"
            );
            let got = events.take();
            assert!(
                got.contains(&Event::StartBranchMoved {
                    remote: "origin".into(),
                    refname: "refs/heads/agentfs/v1".into(),
                    pinned: f,
                    tip: g,
                }),
                "{got:?}"
            );
            let saved = PushState::load(&v2.state.push_state("origin"));
            assert_eq!(
                saved.start_moved,
                Some(StartMoved {
                    refname: "refs/heads/agentfs/v1".into(),
                    pinned: f.to_string(),
                    tip: g.to_string(),
                })
            );
        }

        // The operator takes g in and accepts it (what `agentfs start
        // accept` writes): no alarm, and the accepted ref is pushed.
        let repo = v2.com.history().repo();
        repo.remote_anonymous(&url)
            .unwrap()
            .fetch(&["refs/heads/agentfs/v1"], None, None)
            .unwrap();
        let accepted = crate::fork::identity::accepted_ref("v2", g);
        repo.reference(&accepted, g, false, "accepted").unwrap();
        assert!(pusher.push_once().is_success());
        assert!(
            !events
                .take()
                .iter()
                .any(|e| matches!(e, Event::StartBranchMoved { .. }))
        );
        assert_eq!(pusher.state().start_moved, None);
        assert_eq!(remote_ref(remote, &accepted), Some(g));
        // Another volume's pushes never ask (it has no start point).
        let (others, sink_events) = sink();
        v1.commit();
        assert!(v1.pusher("origin", sink_events).push_once().is_success());
        assert!(
            !others
                .take()
                .iter()
                .any(|e| matches!(e, Event::StartBranchMoved { .. }))
        );
    }

    #[test]
    fn unrelated_remote_history_is_divergence() {
        let remote_dir = tempfile::tempdir().unwrap();
        let remote = remote_dir.path();
        let repo = bare(remote);
        let sig = git2::Signature::now("other", "o@example.invalid").unwrap();
        let tree = repo.treebuilder(None).unwrap().write().unwrap();
        let tree = repo.find_tree(tree).unwrap();
        let theirs = repo
            .commit(Some("refs/heads/agentfs/vol"), &sig, &sig, "x", &tree, &[])
            .unwrap();
        let mut v = Vol::new("vol", &[("origin", &remote.display().to_string())]);
        let ours = v.commit();
        let outcome = v.pusher("origin", Arc::new(NullSink)).push_once();
        assert!(
            matches!(&outcome, PushOutcome::Diverged { local_head, remote_head: Some(r), diverged_ref: Some(_) }
                if *local_head == ours && *r == theirs),
            "{outcome:?}"
        );
        assert_eq!(remote_ref(remote, "refs/heads/agentfs/vol"), Some(theirs));
    }

    #[test]
    fn non_fast_forward_on_another_ref_is_not_divergence() {
        let remote_dir = tempfile::tempdir().unwrap();
        let remote = remote_dir.path();
        bare(remote);
        let mut v = Vol::new("vol", &[("origin", &remote.display().to_string())]);
        let first = v.commit();
        v.com
            .history()
            .repo()
            .reference("refs/agentfs/vol/status", first, false, "t")
            .unwrap();
        let mut pusher = v.pusher("origin", Arc::new(NullSink));
        assert!(pusher.push_once().is_success());
        // Someone moves the status ref on the remote to a commit we lack.
        let theirs = write_remote(remote, "refs/agentfs/vol/status");
        let second = v.commit();
        // The branch push no longer carries it: it succeeds.
        let outcome = pusher.push_once();
        assert_eq!(
            outcome,
            PushOutcome::Pushed {
                from: Some(first),
                to: second,
                limited: None
            }
        );
        assert!(diverged_refs(remote, "vol").is_empty());
        assert_eq!(remote_ref(remote, "refs/agentfs/vol/status"), Some(theirs));
        // A new local status is forced over it by the queue push.
        v.com
            .history()
            .repo()
            .reference("refs/agentfs/vol/status", second, true, "t")
            .unwrap();
        assert!(pusher.push_once().is_success());
        assert_eq!(pusher.queue().and_then(|q| q.error.clone()), None);
        assert_eq!(remote_ref(remote, "refs/agentfs/vol/status"), Some(second));
    }

    #[test]
    fn two_volumes_share_one_remote() {
        let remote_dir = tempfile::tempdir().unwrap();
        let remote = remote_dir.path();
        bare(remote);
        let url = remote.display().to_string();
        let mut a = Vol::new("a", &[("shared", &url)]);
        let mut b = Vol::new("b", &[("shared", &url)]);
        let a_head = a.commit();
        a.com
            .history()
            .repo()
            .reference("refs/agentfs/a/status", a_head, false, "t")
            .unwrap();
        let b_head = b.commit();
        b.commit();
        let b_head2 = b.head();
        assert_ne!(b_head, b_head2);

        assert!(
            a.pusher("shared", Arc::new(NullSink))
                .push_once()
                .is_success()
        );
        assert!(
            b.pusher("shared", Arc::new(NullSink))
                .push_once()
                .is_success()
        );
        assert_eq!(remote_ref(remote, "refs/heads/agentfs/a"), Some(a_head));
        assert_eq!(remote_ref(remote, "refs/agentfs/a/status"), Some(a_head));
        assert_eq!(remote_ref(remote, "refs/heads/agentfs/b"), Some(b_head2));
        assert_eq!(
            remote_refs(remote),
            [
                "refs/agentfs/a/status",
                "refs/heads/agentfs/a",
                "refs/heads/agentfs/b"
            ]
        );

        // A second recorder misconfigured with the same name is detected.
        let mut impostor = Vol::new("a", &[("shared", &url)]);
        impostor.n = 100; // different content, so a different commit
        impostor.commit();
        let outcome = impostor.pusher("shared", Arc::new(NullSink)).push_once();
        assert!(
            matches!(outcome, PushOutcome::Diverged { .. }),
            "{outcome:?}"
        );
        assert_eq!(remote_ref(remote, "refs/heads/agentfs/a"), Some(a_head));
        assert_eq!(remote_ref(remote, "refs/heads/agentfs/b"), Some(b_head2));
    }

    /// Volumes committing and pushing to one remote at the same time (design
    /// §11.3, §18): two volumes, and a third misconfigured with `a`'s name.
    /// Each volume's branch ends at its head, never overwritten; of the two
    /// `a`s the first to push owns the branch and the other diverges, saved
    /// once; the remote passes `git fsck --full --strict`.
    #[test]
    fn concurrent_pushes_to_one_shared_remote() {
        const COMMITS: usize = 40;
        let remote_dir = tempfile::tempdir().unwrap();
        let remote = remote_dir.path().to_path_buf();
        bare(&remote);
        let url = remote.display().to_string();
        let vols = vec![
            Vol::new("a", &[("shared", &url)]),
            Vol::new("b", &[("shared", &url)]),
            Vol::new("a", &[("shared", &url)]),
        ];
        let barrier = Arc::new(std::sync::Barrier::new(vols.len()));
        let threads: Vec<_> = vols
            .into_iter()
            .enumerate()
            .map(|(i, mut v)| {
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    v.n = i * 1000; // distinct content per writer
                    let mut pusher = v.pusher("shared", Arc::new(NullSink));
                    let mut diverged = false;
                    barrier.wait();
                    for _ in 0..COMMITS {
                        v.commit();
                        // A ref lock held by another writer's push fails the
                        // attempt; the push loop would retry it.
                        let mut done = false;
                        for _ in 0..50 {
                            match pusher.push_once() {
                                PushOutcome::Pushed { .. } | PushOutcome::UpToDate { .. } => {
                                    assert!(!diverged, "pushed after diverging");
                                    done = true;
                                }
                                PushOutcome::Diverged { .. } => {
                                    diverged = true;
                                    done = true;
                                }
                                PushOutcome::Failed { .. }
                                | PushOutcome::Busy
                                | PushOutcome::Cancelled => {
                                    std::thread::sleep(Duration::from_millis(5));
                                }
                                PushOutcome::NothingToPush => unreachable!("committed"),
                            }
                            if done {
                                break;
                            }
                        }
                        assert!(done, "writer {i} never got an answer from the remote");
                    }
                    (v, diverged)
                })
            })
            .collect();
        let results: Vec<(Vol, bool)> = threads.into_iter().map(|t| t.join().unwrap()).collect();

        let (b, b_diverged) = &results[1];
        assert!(!b_diverged, "b shares the remote with no one");
        assert_eq!(remote_ref(&remote, "refs/heads/agentfs/b"), Some(b.head()));
        let (winner, loser) = match (results[0].1, results[2].1) {
            (false, true) => (&results[0].0, &results[2].0),
            (true, false) => (&results[2].0, &results[0].0),
            other => panic!("exactly one `a` owns the branch; diverged: {other:?}"),
        };
        assert_eq!(
            remote_ref(&remote, "refs/heads/agentfs/a"),
            Some(winner.head())
        );
        let saved = diverged_refs(&remote, "a");
        assert_eq!(saved.len(), 1, "saved once: {saved:?}");
        let loser_repo = loser.com.history().repo();
        assert!(
            saved[0].1 == loser.head()
                || loser_repo
                    .graph_descendant_of(loser.head(), saved[0].1)
                    .unwrap(),
            "the saved head is the loser's"
        );
        let fsck = std::process::Command::new("git")
            .args(["fsck", "--full", "--strict"])
            .env("GIT_DIR", &remote)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .unwrap();
        assert!(
            fsck.status.success(),
            "{}",
            String::from_utf8_lossy(&fsck.stderr)
        );
    }

    #[test]
    fn state_survives_reopen() {
        let missing = tempfile::tempdir().unwrap().path().join("nope");
        let mut v = Vol::new("vol", &[("gone", &missing.display().to_string())]);
        v.commit();
        {
            let mut pusher = v.pusher("gone", Arc::new(NullSink));
            pusher.push_once();
            pusher.push_once();
        }
        let pusher = v.pusher("gone", Arc::new(NullSink));
        assert_eq!(pusher.state().consecutive_failures, 2);
        assert_eq!(pusher.next_delay(), Duration::from_secs(120));

        // A corrupt state file is treated as empty, not as an error.
        fs::write(v.state.push_state("gone"), "{not json").unwrap();
        let mut pusher = v.pusher("gone", Arc::new(NullSink));
        assert_eq!(pusher.state(), &PushState::default());
        pusher.push_once();
        assert_eq!(pusher.state().consecutive_failures, 1);
    }

    #[test]
    fn lock_makes_concurrent_push_busy() {
        let remote_dir = tempfile::tempdir().unwrap();
        bare(remote_dir.path());
        let mut v = Vol::new(
            "vol",
            &[("origin", &remote_dir.path().display().to_string())],
        );
        v.commit();
        let (events, sink_events) = sink();
        let mut pusher = v.pusher("origin", sink_events);
        let held = FileLock::try_take(&v.state.push_lock("origin"), Mode::Exclusive)
            .unwrap()
            .unwrap();
        assert_eq!(pusher.push_once(), PushOutcome::Busy);
        assert!(!PushOutcome::Busy.is_success());
        assert!(events.take().is_empty());
        drop(held);
        assert!(pusher.push_once().is_success());
    }

    #[test]
    fn status_reports_each_remote() {
        let remote_dir = tempfile::tempdir().unwrap();
        bare(remote_dir.path());
        let missing = remote_dir.path().join("missing");
        let mut v = Vol::new(
            "vol",
            &[
                ("good", &remote_dir.path().display().to_string()),
                ("bad", &format!("file://user:secret@{}", missing.display())),
            ],
        );
        assert!(
            status(&v.state, &v.settings)
                .unwrap()
                .iter()
                .all(|s| s.unpushed.is_none())
        );
        v.commit();
        v.commit();
        for mut pusher in open_all(
            &v.state,
            &v.settings,
            Arc::new(NullSink),
            &Telemetry::noop(),
        )
        .unwrap()
        {
            pusher.push_once();
        }
        v.commit();
        let all = status(&v.state, &v.settings).unwrap();
        let names: Vec<&str> = all.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["bad", "good"]);
        assert_eq!(all[0].unpushed, Some(3));
        assert_eq!(all[0].state.consecutive_failures, 1);
        assert!(!all[0].url.contains("secret"), "{}", all[0].url);
        assert_eq!(all[1].unpushed, Some(1));
        assert_eq!(all[1].state.consecutive_failures, 0);
    }

    #[test]
    fn credentials_callback_without_a_file_is_an_auth_error() {
        let remote_dir = tempfile::tempdir().unwrap();
        bare(remote_dir.path());
        let v = Vol::new(
            "vol",
            &[("origin", &remote_dir.path().display().to_string())],
        );
        let pusher = v.pusher("origin", Arc::new(NullSink));
        let calls = Cell::new(0);
        let err = pusher
            .answer(
                "https://h/r",
                None,
                CredentialType::USER_PASS_PLAINTEXT,
                &calls,
            )
            .unwrap_err();
        assert!(err.contains("agentfsCredentials is not set"), "{err}");
    }

    #[test]
    fn credentials_callback_gives_one_credential_per_connection() {
        let remote_dir = tempfile::tempdir().unwrap();
        bare(remote_dir.path());
        let cred = remote_dir.path().join("token");
        fs::write(&cred, "tok\n").unwrap();
        let mut v = Vol::new("vol", &[("origin", "https://git.example/o/r.git")]);
        v.settings.remotes.pushed_mut()[0].credentials = Some(cred);
        let pusher = v.pusher("origin", Arc::new(NullSink));
        let calls = Cell::new(0);
        let url = "https://git.example/o/r.git";
        assert_eq!(
            pusher.answer(url, None, CredentialType::USER_PASS_PLAINTEXT, &calls),
            Ok(Credential {
                username: crate::credentials::DEFAULT_USERNAME.into(),
                password: "tok".into()
            })
        );
        let again = pusher
            .answer(url, None, CredentialType::USER_PASS_PLAINTEXT, &calls)
            .unwrap_err();
        assert!(again.contains("rejected the credentials"), "{again}");
        // Never for another host, never an SSH key, and a user name alone
        // is not answered for a non-ssh remote.
        let calls = Cell::new(0);
        assert!(
            pusher
                .answer(
                    "https://evil.example/o/r.git",
                    None,
                    CredentialType::USER_PASS_PLAINTEXT,
                    &calls
                )
                .is_err()
        );
        for allowed in [
            CredentialType::SSH_KEY,
            CredentialType::SSH_MEMORY | CredentialType::SSH_KEY,
            CredentialType::USERNAME,
        ] {
            let calls = Cell::new(0);
            assert!(
                pusher.answer(url, None, allowed, &calls).is_err(),
                "{allowed:?}"
            );
        }
    }

    /// An unencrypted OpenSSH private key file; only its header and cipher
    /// name are parsed by these tests (the transport decodes the rest).
    fn openssh_key() -> String {
        let mut body = b"openssh-key-v1\0".to_vec();
        for field in [&b"none"[..], b"none", b""] {
            body.extend_from_slice(&(field.len() as u32).to_be_bytes());
            body.extend_from_slice(field);
        }
        body.extend_from_slice(&[0u8; 64]);
        format!(
            "-----BEGIN OPENSSH PRIVATE KEY-----\n{}\n-----END OPENSSH PRIVATE KEY-----\n",
            base64::engine::general_purpose::STANDARD.encode(&body)
        )
    }

    const SSH_URL: &str = "ssh://git@git.example:2222/o/r.git";

    /// A volume whose remote `origin` is an ssh remote with a key and a
    /// known_hosts file listing `known` for `[git.example]:2222`. The
    /// settings are edited directly, so this runs without the `ssh`
    /// feature.
    fn ssh_vol(known: &[u8]) -> (Vol, PathBuf, PathBuf) {
        let mut v = Vol::new("vol", &[("origin", "/unused")]);
        let key = v.dir.path().join("id");
        fs::write(&key, openssh_key()).unwrap();
        let kh = v.dir.path().join("known_hosts");
        fs::write(
            &kh,
            crate::known_hosts::tests::line("[git.example]:2222", known),
        )
        .unwrap();
        let r = &mut v.settings.remotes.pushed_mut()[0];
        r.url = SSH_URL.into();
        r.ssh_key = Some(key.clone());
        r.known_hosts = Some(kh.clone());
        (v, key, kh)
    }

    #[test]
    fn ssh_remotes_get_only_their_own_key() {
        let (v, _key, _kh) = ssh_vol(&crate::known_hosts::tests::ed25519(1));
        let pusher = v.pusher("origin", Arc::new(NullSink));
        // libgit2's credentials callback never serves an ssh remote: its
        // transport is agentfs's own.
        for allowed in [
            CredentialType::USERNAME,
            CredentialType::SSH_KEY | CredentialType::SSH_MEMORY,
            CredentialType::USER_PASS_PLAINTEXT,
        ] {
            let err = pusher
                .answer(SSH_URL, None, allowed, &Cell::new(0))
                .unwrap_err();
            assert!(err.contains("agentfs's SSH transport"), "{err}");
        }
        let remote = pusher.auth.ssh_remote().unwrap();
        assert_eq!(remote.key(SSH_URL).unwrap().text, openssh_key());
        // Never for another host or port.
        for url in [
            "ssh://git@evil.example:2222/o/r.git",
            "ssh://git@git.example/o/r.git",
        ] {
            let err = remote.key(url).unwrap_err();
            assert!(err.contains("bound to host"), "{err}");
        }
    }

    #[test]
    fn ssh_key_problems_are_auth_errors() {
        let (mut v, key, _kh) = ssh_vol(&crate::known_hosts::tests::ed25519(1));
        fs::write(&key, "ssh-ed25519 AAAA me@host\n").unwrap();
        let pusher = v.pusher("origin", Arc::new(NullSink));
        let remote = pusher.auth.ssh_remote().unwrap();
        let err = remote.key(SSH_URL).unwrap_err();
        assert!(err.contains("no private key"), "{err}");
        assert!(err.contains(&key.display().to_string()), "{err}");
        fs::remove_file(&key).unwrap();
        assert!(
            remote
                .key(SSH_URL)
                .unwrap_err()
                .contains("reading credentials")
        );
        v.settings.remotes.pushed_mut()[0].ssh_key = None;
        let pusher = v.pusher("origin", Arc::new(NullSink));
        let err = pusher.auth.ssh_remote().unwrap().key(SSH_URL).unwrap_err();
        assert!(err.contains("agentfsSshKey is not set"), "{err}");
    }

    #[test]
    fn host_key_verdicts() {
        use crate::known_hosts::tests::{ecdsa, ed25519};
        let trusted = ed25519(1);
        let (v, _key, kh) = ssh_vol(&trusted);
        let pusher = v.pusher("origin", Arc::new(NullSink));
        let hash = [7u8; 32];
        let fingerprint = format!("SHA256:{}", STANDARD_NO_PAD.encode(hash));
        pusher
            .check_host_key("git.example", Some(&trusted), Some(&hash))
            .unwrap();
        pusher
            .check_host_key("GIT.example", Some(&trusted), None)
            .unwrap();
        let refused = |hostname: &str, raw: Option<&[u8]>| {
            pusher
                .check_host_key(hostname, raw, Some(&hash))
                .unwrap_err()
        };
        let common = |err: &str, key_type: &str| {
            for part in [
                &kh.display().to_string(),
                "[git.example]:2222",
                key_type,
                &fingerprint,
                "ssh-keyscan -p 2222 git.example",
            ] {
                assert!(err.contains(part), "{part:?} missing from {err}");
            }
        };
        // A changed key of the same type.
        let err = refused("git.example", Some(&ed25519(2)));
        common(&err, "ssh-ed25519");
        assert!(err.contains("does not match"), "{err}");
        // A key type known_hosts does not list.
        let err = refused("git.example", Some(&ecdsa(1)));
        common(&err, "ecdsa-sha2-nistp256");
        assert!(err.contains("lists only ssh-ed25519"), "{err}");
        // Revoked.
        let mut text = fs::read_to_string(&kh).unwrap();
        text.push_str(&format!(
            "\n@revoked {}\n",
            crate::known_hosts::tests::line("*", &trusted)
        ));
        fs::write(&kh, text).unwrap();
        let err = refused("git.example", Some(&trusted));
        common(&err, "ssh-ed25519");
        assert!(err.contains("@revoked"), "{err}");
        // Unknown: the file names another host.
        fs::write(
            &kh,
            crate::known_hosts::tests::line("[otherhost]:2222", &trusted),
        )
        .unwrap();
        let err = refused("git.example", Some(&trusted));
        common(&err, "ssh-ed25519");
        assert!(err.contains("unknown"), "{err}");
        // Unreadable.
        fs::remove_file(&kh).unwrap();
        let err = refused("git.example", Some(&trusted));
        assert!(err.contains("reading known_hosts"), "{err}");
        assert!(err.contains(&kh.display().to_string()), "{err}");
        // Another host than the push URL's, and no raw key.
        let err = refused("evil.example", Some(&trusted));
        assert!(err.contains("not \"git.example\""), "{err}");
        let err = refused("git.example", None);
        assert!(err.contains("no host key"), "{err}");
    }

    #[test]
    fn host_key_check_needs_an_ssh_remote() {
        let remote_dir = tempfile::tempdir().unwrap();
        let v = Vol::new(
            "vol",
            &[("origin", &remote_dir.path().display().to_string())],
        );
        let pusher = v.pusher("origin", Arc::new(NullSink));
        assert!(pusher.auth.ssh().is_none());
        assert!(
            pusher
                .check_host_key("h", Some(&crate::known_hosts::tests::ed25519(1)), None)
                .is_err()
        );
    }

    mod lfs {
        use super::*;
        use crate::lfs::Pointer;
        use crate::lfs::fake::FakeLfs;

        const THRESHOLD: (&str, &str) = ("agentfs.lfsThreshold", "8");

        fn object_in(remote: &Path, p: &Pointer) -> PathBuf {
            let hex = p.oid.to_string();
            remote.join(format!("lfs/objects/{}/{}/{hex}", &hex[..2], &hex[2..4]))
        }

        fn pointer_at(v: &Vol, path: &str) -> Pointer {
            let head = v.com.history().head().unwrap().unwrap();
            let id = head.tree().unwrap().get_path(Path::new(path)).unwrap().id();
            Pointer::parse(v.com.history().repo().find_blob(id).unwrap().content()).unwrap()
        }

        #[test]
        fn objects_are_uploaded_with_the_push_and_only_once() {
            let remote_dir = tempfile::tempdir().unwrap();
            bare(remote_dir.path());
            let url = remote_dir.path().display().to_string();
            let mut v = Vol::with_config("vol", &[("origin", &url)], &[THRESHOLD]);
            v.commit_file("big.bin", b"large content one");
            let head = v.commit_file("small.txt", b"small");
            let first = pointer_at(&v, "big.bin");
            let (events, sink) = sink();
            let mut pusher = v.pusher("origin", sink);
            assert!(pusher.push_once().is_success());
            assert_eq!(
                remote_ref(remote_dir.path(), "refs/heads/agentfs/vol"),
                Some(head)
            );
            assert_eq!(
                fs::read(object_in(remote_dir.path(), &first)).unwrap(),
                b"large content one"
            );
            assert!(matches!(&events.take()[..], [Event::PushSucceeded { .. }]));

            // A later push uploads only the new object.
            let mtime = fs::metadata(object_in(remote_dir.path(), &first))
                .unwrap()
                .modified()
                .unwrap();
            v.commit_file("second.bin", b"large content two");
            let second = pointer_at(&v, "second.bin");
            assert!(pusher.push_once().is_success());
            assert!(object_in(remote_dir.path(), &second).is_file());
            assert_eq!(
                fs::metadata(object_in(remote_dir.path(), &first))
                    .unwrap()
                    .modified()
                    .unwrap(),
                mtime
            );
        }

        #[test]
        fn a_missing_cache_object_fails_the_lfs_stage_and_pushes_no_ref() {
            let remote_dir = tempfile::tempdir().unwrap();
            bare(remote_dir.path());
            let url = remote_dir.path().display().to_string();
            let mut v = Vol::with_config("vol", &[("origin", &url)], &[THRESHOLD]);
            v.commit_file("big.bin", b"large content");
            let p = pointer_at(&v, "big.bin");
            fs::remove_file(v.com.lfs_cache().path(&p.oid)).unwrap();
            let (events, sink) = sink();
            let mut pusher = v.pusher("origin", sink);
            let outcome = pusher.push_once();
            let PushOutcome::Failed {
                kind,
                stage,
                message,
                ..
            } = &outcome
            else {
                panic!("{outcome:?}")
            };
            assert_eq!((*kind, *stage), (PushFailureKind::Local, PushStage::Lfs));
            assert!(message.contains("big.bin"), "{message}");
            assert!(
                message.contains("missing from the local cache"),
                "{message}"
            );
            assert_eq!(
                remote_ref(remote_dir.path(), "refs/heads/agentfs/vol"),
                None
            );
            let saved = PushState::load(&v.state.push_state("origin"));
            assert_eq!(saved.last_error.unwrap().stage, PushStage::Lfs);
            assert!(matches!(
                &events.take()[..],
                [Event::PushFailed {
                    stage: PushStage::Lfs,
                    kind: PushFailureKind::Local,
                    ..
                }]
            ));
        }

        /// An object over the remote's own `agentfsMaxLfsObjectSize`
        /// fails the attempt as `too_large` before any batch request.
        #[test]
        fn objects_over_the_remotes_lfs_limit_fail_before_any_upload() {
            let remote_dir = tempfile::tempdir().unwrap();
            bare(remote_dir.path());
            let url = remote_dir.path().display().to_string();
            let server = FakeLfs::start();
            let lfs_url = server.url();
            let mut v = Vol::with_config(
                "vol",
                &[("origin", &url)],
                &[THRESHOLD, ("remote.origin.lfsurl", &lfs_url)],
            );
            v.commit_file("big.bin", b"large content of 33 bytes, really");
            // The limit is set after the commit: the record-time check
            // never saw it.
            v.settings.remotes.pushed_mut()[0].max_lfs_object_size = Some(16);
            let (events, sink) = sink();
            let mut pusher = v.pusher("origin", sink);
            let outcome = pusher.push_once();
            let PushOutcome::Failed {
                kind,
                stage,
                message,
                ..
            } = &outcome
            else {
                panic!("{outcome:?}")
            };
            assert_eq!((*kind, *stage), (PushFailureKind::TooLarge, PushStage::Lfs));
            assert!(message.contains("big.bin"), "{message}");
            assert!(
                message.contains("agentfsMaxLfsObjectSize (16 bytes)"),
                "{message}"
            );
            assert!(server.seen().is_empty(), "{:?}", server.seen());
            assert_eq!(
                remote_ref(remote_dir.path(), "refs/heads/agentfs/vol"),
                None
            );
            assert!(matches!(
                &events.take()[..],
                [Event::PushFailed {
                    kind: PushFailureKind::TooLarge,
                    stage: PushStage::Lfs,
                    ..
                }]
            ));
        }

        /// An object the server already has needs no cached copy (after
        /// `agentfs lfs prune`): only those it lacks must be in the cache.
        #[test]
        fn objects_the_server_has_need_no_cached_copy() {
            let remote_dir = tempfile::tempdir().unwrap();
            bare(remote_dir.path());
            let url = remote_dir.path().display().to_string();
            let mut v = Vol::with_config("vol", &[("origin", &url)], &[THRESHOLD]);
            v.commit_file("big.bin", b"large content");
            let p = pointer_at(&v, "big.bin");
            let mut pusher = v.pusher("origin", Arc::new(NullSink));
            assert!(pusher.push_once().is_success());
            fs::remove_file(v.com.lfs_cache().path(&p.oid)).unwrap();
            // The remote's push state is lost: the whole branch is walked,
            // and big.bin's object is asked for again.
            fs::remove_file(v.state.push_state("origin")).unwrap();
            let head = v.commit_file("more.bin", b"more large content");
            let mut pusher = v.pusher("origin", Arc::new(NullSink));
            let outcome = pusher.push_once();
            assert!(outcome.is_success(), "{outcome:?}");
            assert_eq!(
                remote_ref(remote_dir.path(), "refs/heads/agentfs/vol"),
                Some(head)
            );
        }

        /// A branch push that fails before git runs (stage lfs) still
        /// pushes the queue's status while the remote's branch is this
        /// worktree's (F8 rule 2), and holds the applied refs back.
        #[test]
        fn a_failed_lfs_stage_still_pushes_the_queue_status() {
            let remote_dir = tempfile::tempdir().unwrap();
            bare(remote_dir.path());
            let url = remote_dir.path().display().to_string();
            let mut v = Vol::with_config("vol", &[("origin", &url)], &[THRESHOLD]);
            v.commit_file("small.txt", b"small");
            assert!(
                v.pusher("origin", Arc::new(NullSink))
                    .push_once()
                    .is_success()
            );
            let head = v.commit_file("big.bin", b"large content");
            let p = pointer_at(&v, "big.bin");
            fs::remove_file(v.com.lfs_cache().path(&p.oid)).unwrap();
            let repo = v.com.history().repo();
            repo.reference("refs/agentfs/vol/status", head, false, "t")
                .unwrap();
            repo.reference("refs/agentfs/vol/applied/0000000001", head, false, "t")
                .unwrap();
            let mut pusher = v.pusher("origin", Arc::new(NullSink));
            assert!(matches!(
                pusher.push_once(),
                PushOutcome::Failed {
                    stage: PushStage::Lfs,
                    ..
                }
            ));
            let queue = pusher.queue().expect("the queue push ran");
            assert!(queue.status && queue.held_back, "{queue:?}");
            assert_eq!(
                remote_ref(remote_dir.path(), "refs/agentfs/vol/status"),
                Some(head)
            );
            assert_eq!(
                remote_ref(remote_dir.path(), "refs/agentfs/vol/applied/0000000001"),
                None
            );
        }

        #[test]
        fn remotes_without_lfs_content_never_resolve_an_endpoint() {
            // git:// derives no LFS endpoint; without LFS content the push
            // fails at the git stage only.
            let mut v =
                Vol::with_config("vol", &[("other", "git://127.0.0.1:1/r.git")], &[THRESHOLD]);
            v.commit_file("small.txt", b"small");
            let mut pusher = v.pusher("other", Arc::new(NullSink));
            assert!(matches!(
                pusher.push_once(),
                PushOutcome::Failed {
                    stage: PushStage::Git,
                    ..
                }
            ));
            // With LFS content it cannot upload, and says what to set.
            v.commit_file("big.bin", b"large content");
            let outcome = pusher.push_once();
            assert!(
                matches!(&outcome, PushOutcome::Failed { stage: PushStage::Lfs, kind: PushFailureKind::Local, message, .. }
                    if message.contains("set remote.other.lfsurl")),
                "{outcome:?}"
            );
        }

        #[test]
        fn an_http_lfs_failure_pushes_no_ref_and_a_fix_pushes_everything() {
            let remote_dir = tempfile::tempdir().unwrap();
            bare(remote_dir.path());
            let server = FakeLfs::start();
            server.with(|st| st.batch_status = Some(401));
            let url = remote_dir.path().display().to_string();
            let lfs_url = server.url();
            let mut v = Vol::with_config(
                "vol",
                &[("origin", &url)],
                &[THRESHOLD, ("remote.origin.lfsurl", &lfs_url)],
            );
            let head = v.commit_file("big.bin", b"large content");
            let p = pointer_at(&v, "big.bin");
            let mut pusher = v.pusher("origin", Arc::new(NullSink));
            let outcome = pusher.push_once();
            assert!(
                matches!(
                    &outcome,
                    PushOutcome::Failed {
                        stage: PushStage::Lfs,
                        kind: PushFailureKind::Auth,
                        ..
                    }
                ),
                "{outcome:?}"
            );
            assert_eq!(
                remote_ref(remote_dir.path(), "refs/heads/agentfs/vol"),
                None
            );
            server.with(|st| st.batch_status = None);
            let outcome = pusher.push_once();
            assert!(outcome.is_success(), "{outcome:?}");
            assert_eq!(
                remote_ref(remote_dir.path(), "refs/heads/agentfs/vol"),
                Some(head)
            );
            assert_eq!(server.object(&p.oid.to_string()).unwrap(), b"large content");
            // No object went to the git remote's own LFS directory.
            assert!(!object_in(remote_dir.path(), &p).exists());
            let batch = server
                .seen()
                .into_iter()
                .filter(|s| s.path.ends_with("/objects/batch"))
                .count();
            assert_eq!(batch, 2);
            // The branch is up to date now: no batch request at all.
            assert!(pusher.push_once().is_success());
            assert_eq!(
                server
                    .seen()
                    .into_iter()
                    .filter(|s| s.path.ends_with("/objects/batch"))
                    .count(),
                2
            );
        }
    }

    mod telemetry {
        use opentelemetry::trace::Status;

        use super::*;
        use crate::telemetry::testing::{Harness, span_attr};
        use crate::telemetry::{names, spans};

        fn origin() -> &'static [(&'static str, &'static str)] {
            &[("remote", "origin")]
        }

        #[test]
        fn push_span_and_backlog_gauges() {
            let remote_dir = tempfile::tempdir().unwrap();
            bare(remote_dir.path());
            let mut v = Vol::new(
                "vol",
                &[("origin", &remote_dir.path().display().to_string())],
            );
            let h = Harness::new();
            h.telemetry.observe_volume(&v.state, &v.settings);
            v.commit();
            let head = v.commit();
            assert_eq!(h.gauge_u64(names::PUSH_UNPUSHED_COMMITS, origin()), Some(2));
            let age = h
                .gauge_f64(names::PUSH_OLDEST_UNPUSHED_AGE, origin())
                .unwrap();
            assert!((0.0..60.0).contains(&age), "{age}");
            assert_eq!(h.gauge_u64(names::PUSH_DIVERGED, origin()), Some(0));

            let mut pusher =
                v.pusher_with("origin", h.telemetry.sink(Arc::new(NullSink)), &h.telemetry);
            assert!(pusher.push_once().is_success());
            assert_eq!(h.gauge_u64(names::PUSH_UNPUSHED_COMMITS, origin()), Some(0));
            assert_eq!(
                h.gauge_f64(names::PUSH_OLDEST_UNPUSHED_AGE, origin()),
                Some(0.0)
            );
            assert_eq!(h.gauge_u64(names::PUSH_DIVERGED, origin()), Some(0));
            assert_eq!(
                h.sum(
                    names::PUSH_FAILURES,
                    &[("remote", "origin"), ("reason", "auth"), ("stage", "git")]
                ),
                None
            );

            let pushes = h.spans(spans::PUSH);
            assert_eq!(pushes.len(), 1);
            let span = &pushes[0];
            assert_eq!(span_attr(span, "agentfs.remote").as_deref(), Some("origin"));
            assert_eq!(
                span_attr(span, "agentfs.push.outcome").as_deref(),
                Some("pushed")
            );
            assert_eq!(span_attr(span, "agentfs.push.head"), Some(head.to_string()));
            assert_eq!(span_attr(span, "agentfs.push.previous"), None);
            assert_eq!(span_attr(span, "agentfs.push.reason"), None);
            assert_eq!(span.status, Status::Unset);

            assert!(pusher.push_once().is_success());
            let pushes = h.spans(spans::PUSH);
            assert_eq!(
                span_attr(&pushes[1], "agentfs.push.outcome").as_deref(),
                Some("up_to_date")
            );
            assert_eq!(
                span_attr(&pushes[1], "agentfs.push.previous"),
                Some(head.to_string())
            );
        }

        #[test]
        fn the_push_span_counts_lfs_objects() {
            let remote_dir = tempfile::tempdir().unwrap();
            bare(remote_dir.path());
            let mut v = Vol::with_config(
                "vol",
                &[("origin", &remote_dir.path().display().to_string())],
                &[("agentfs.lfsThreshold", "4")],
            );
            v.commit_file("a.bin", b"aaaaaaaa");
            v.commit_file("b.bin", b"bbbbbbbbbb");
            let h = Harness::new();
            let mut pusher =
                v.pusher_with("origin", h.telemetry.sink(Arc::new(NullSink)), &h.telemetry);
            assert!(pusher.push_once().is_success());
            let span = &h.spans(spans::PUSH)[0];
            assert_eq!(span_attr(span, "agentfs.lfs.objects").as_deref(), Some("2"));
            assert_eq!(
                span_attr(span, "agentfs.lfs.uploaded").as_deref(),
                Some("2")
            );
            assert_eq!(span_attr(span, "agentfs.lfs.bytes").as_deref(), Some("18"));
            // A failed upload: the stage is on the span and the counter.
            let cache = v.com.lfs_cache().clone();
            v.commit_file("c.bin", b"cccccccccccc");
            let objects = fs::read_dir(v.state.lfs_objects()).unwrap().count();
            assert!(objects > 0);
            let head = v.com.history().head().unwrap().unwrap();
            let c = head
                .tree()
                .unwrap()
                .get_path(Path::new("c.bin"))
                .unwrap()
                .id();
            let c =
                crate::lfs::Pointer::parse(v.com.history().repo().find_blob(c).unwrap().content())
                    .unwrap();
            fs::remove_file(cache.path(&c.oid)).unwrap();
            assert!(!pusher.push_once().is_success());
            let span = &h.spans(spans::PUSH)[1];
            assert_eq!(
                span_attr(span, "agentfs.push.stage").as_deref(),
                Some("lfs")
            );
            assert_eq!(span_attr(span, "agentfs.lfs.objects"), None);
            assert_eq!(
                h.sum(
                    names::PUSH_FAILURES,
                    &[("remote", "origin"), ("reason", "local"), ("stage", "lfs")]
                ),
                Some(1)
            );
        }

        #[test]
        fn push_failures_by_reason() {
            let missing = tempfile::tempdir().unwrap().path().join("nope");
            let mut v = Vol::new("vol", &[("gone", &missing.display().to_string())]);
            v.commit();
            let h = Harness::new();
            let mut pusher =
                v.pusher_with("gone", h.telemetry.sink(Arc::new(NullSink)), &h.telemetry);
            for _ in 0..2 {
                assert!(!pusher.push_once().is_success());
            }
            assert_eq!(
                h.sum(
                    names::PUSH_FAILURES,
                    &[
                        ("remote", "gone"),
                        ("reason", "unreachable"),
                        ("stage", "git")
                    ]
                ),
                Some(2)
            );
            let pushes = h.spans(spans::PUSH);
            assert_eq!(pushes.len(), 2);
            for span in &pushes {
                assert!(
                    matches!(span.status, Status::Error { .. }),
                    "{:?}",
                    span.status
                );
                assert_eq!(
                    span_attr(span, "agentfs.push.reason").as_deref(),
                    Some("unreachable")
                );
                assert_eq!(
                    span_attr(span, "agentfs.push.outcome").as_deref(),
                    Some("failed")
                );
            }
            let logs: Vec<_> = h
                .logs()
                .into_iter()
                .filter(|r| r.event_name() == Some("agentfs.push_failed"))
                .collect();
            assert_eq!(logs.len(), 2);
            for (log, span) in logs.iter().zip(&pushes) {
                let trace = log.trace_context().expect("trace context");
                assert_eq!(trace.trace_id, span.span_context.trace_id());
                assert_eq!(trace.span_id, span.span_context.span_id());
            }
        }

        #[test]
        fn diverged_gauge() {
            let remote_dir = tempfile::tempdir().unwrap();
            let remote = remote_dir.path();
            bare(remote);
            let mut v = Vol::new("vol", &[("origin", &remote.display().to_string())]);
            let h = Harness::new();
            h.telemetry.observe_volume(&v.state, &v.settings);
            let pushed = v.commit();
            let mut pusher =
                v.pusher_with("origin", h.telemetry.sink(Arc::new(NullSink)), &h.telemetry);
            assert!(pusher.push_once().is_success());

            write_remote(remote, "refs/heads/agentfs/vol");
            let ours = v.commit();
            let outcome = pusher.push_once();
            let PushOutcome::Diverged {
                diverged_ref: Some(saved_as),
                ..
            } = &outcome
            else {
                panic!("{outcome:?}");
            };
            assert_eq!(h.gauge_u64(names::PUSH_DIVERGED, origin()), Some(1));
            assert_eq!(h.gauge_u64(names::PUSH_UNPUSHED_COMMITS, origin()), Some(1));
            assert_eq!(
                h.sum(
                    names::PUSH_FAILURES,
                    &[
                        ("remote", "origin"),
                        ("reason", "non_fast_forward"),
                        ("stage", "git")
                    ]
                ),
                Some(1)
            );
            let pushes = h.spans(spans::PUSH);
            let span = pushes.last().unwrap();
            assert_eq!(
                span_attr(span, "agentfs.push.outcome").as_deref(),
                Some("diverged")
            );
            assert_eq!(
                span_attr(span, "agentfs.push.diverged_ref").as_ref(),
                Some(saved_as)
            );
            assert_eq!(span_attr(span, "agentfs.push.head"), Some(ours.to_string()));
            assert!(matches!(span.status, Status::Error { .. }));
            assert!(
                h.logs()
                    .iter()
                    .any(|r| r.event_name() == Some("agentfs.diverged")),
                "the divergence is logged"
            );

            // The operator resets the remote branch to an ancestor.
            Repository::open_bare(remote)
                .unwrap()
                .reference("refs/heads/agentfs/vol", pushed, true, "operator reset")
                .unwrap();
            assert!(pusher.push_once().is_success());
            assert_eq!(h.gauge_u64(names::PUSH_DIVERGED, origin()), Some(0));
            assert_eq!(h.gauge_u64(names::PUSH_UNPUSHED_COMMITS, origin()), Some(0));
        }

        #[test]
        fn backlog_and_misconfigured() {
            let v = Vol::new("vol", &[("origin", "/nowhere")]);
            assert_eq!(
                backlog(&v.state, &v.settings).unwrap(),
                [RemoteBacklog {
                    remote: "origin".into(),
                    unpushed: 0,
                    oldest_unpushed_s: None,
                    diverged: false,
                }]
            );

            let mut config = git2::Config::open(&v.state.git().join("config")).unwrap();
            config
                .set_str("remote.team/backup.url", "/nowhere")
                .unwrap();
            let settings = crate::history::History::open(&v.state.git())
                .unwrap()
                .settings()
                .clone();
            assert_eq!(settings.invalid_remotes.len(), 1);
            let h = Harness::new();
            h.telemetry.observe_volume(&v.state, &settings);
            assert_eq!(
                h.gauge_u64(names::REMOTE_MISCONFIGURED, &[("remote", "team/backup")]),
                Some(1)
            );
            assert_eq!(
                h.gauge_u64(names::REMOTE_MISCONFIGURED, origin()),
                None,
                "valid remotes are not reported"
            );
            assert_eq!(h.gauge_u64(names::PUSH_UNPUSHED_COMMITS, origin()), Some(0));
        }

        #[test]
        fn backlog_reports_the_oldest_unpushed_commit() {
            let remote_dir = tempfile::tempdir().unwrap();
            bare(remote_dir.path());
            let mut v = Vol::new(
                "vol",
                &[("origin", &remote_dir.path().display().to_string())],
            );
            let first = v.commit();
            let mut pusher = v.pusher("origin", Arc::new(NullSink));
            assert!(pusher.push_once().is_success());
            let second = v.commit();
            v.commit();
            let backlog = backlog(&v.state, &v.settings).unwrap();
            assert_eq!(backlog[0].unpushed, 2);
            let repo = v.com.history().repo();
            let time = |oid| repo.find_commit(oid).unwrap().committer().when().seconds();
            assert_eq!(backlog[0].oldest_unpushed_s, Some(time(second)));
            assert!(time(first) <= time(second));
            // `status` still reports the count.
            assert_eq!(status(&v.state, &v.settings).unwrap()[0].unpushed, Some(2));
        }
    }
}
