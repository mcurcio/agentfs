//! Pushing the volume's refs to its remotes (design §11).
//!
//! Each configured remote gets a [`RemotePusher`] with its own repository
//! handle and its own persisted [`PushState`] in
//! `<state>/meta/push/<remote>.json`, so one failing remote never blocks or
//! colours another.
//!
//! A push sends only the volume's own refs (design §11.4): the history
//! branch and `refs/agentfs/<volume>/*`, as explicit `<oid>:<ref>` refspecs
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
//!   local head is pushed once per divergence to
//!   `refs/agentfs/<volume>/diverged/<UTC timestamp>`, and every attempt
//!   raises [`Event::PushFailed`] until an operator resolves it.
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
//! from pushing the same remote at once.
//!
//! libgit2's local transport updates a remote ref without compare-and-swap
//! once the ref exists, so a writer moving a local or `file://` remote
//! between libgit2's ref listing and its update can be overwritten. Smart
//! HTTP and SSH servers compare and swap.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[cfg(test)]
use git2::CredentialType;
use git2::{Error as GitError, Repository};
use git2::{ErrorClass, ErrorCode, Oid, PushOptions};
use opentelemetry::KeyValue;
use serde::{Deserialize, Serialize};

use crate::config::{RemoteSettings, Settings};
use crate::events::{Event, Events, PushFailureKind, PushStage};
use crate::history::init_libgit2;
use crate::lfs::upload::objects_to_upload;
use crate::lfs::{Direction, LfsError, ObjectCache, Transfer, UploadReport, transfer_for};
#[cfg(test)]
use crate::remote_auth::CredentialAnswer;
use crate::remote_auth::{AuthState, RemoteAuth};
use crate::telemetry::{Telemetry, spans};
use crate::volume::StateDir;

/// The longest delay between retries of a failing remote, unless
/// `pushInterval` is longer.
pub const PUSH_BACKOFF_CAP: Duration = Duration::from_secs(900);

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
    UpToDate {
        head: Oid,
    },
    Pushed {
        from: Option<Oid>,
        to: Oid,
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
}

impl QueuePush {
    /// One line for `agentfs push`.
    pub fn summary(&self) -> String {
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
        }
    }
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
}

/// Pushes one remote. Owns its own repository handle, so each remote can
/// run on its own thread.
pub struct RemotePusher {
    repo: Repository,
    state_path: PathBuf,
    lock_path: PathBuf,
    volume: String,
    branch_ref: String,
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
        })
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
    /// success, the backoff after failures.
    pub fn next_delay(&self) -> Duration {
        backoff(
            self.push_interval,
            self.state.consecutive_failures,
            PUSH_BACKOFF_CAP,
        )
    }

    /// Commits on the branch the remote is not known to have; `None` if
    /// the branch does not exist yet.
    pub fn unpushed(&self) -> Result<Option<usize>, GitError> {
        unpushed(&self.repo, &self.branch_ref, self.state.last_pushed_oid())
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
        let outcome = self.branch_push(head_out, lfs_out);
        // The queue push follows every branch attempt, failed or not
        // (design §12): the status ref reaches the remote even while the
        // branch cannot; applied refs wait for a branch push that worked.
        if self.patch_remote {
            self.queue = self.queue_push(outcome.is_success());
        }
        outcome
    }

    /// The branch push of [`RemotePusher::push_once_inner`], with the push
    /// lock held and the state loaded.
    fn branch_push(
        &mut self,
        head_out: &mut Option<Oid>,
        lfs_out: &mut Option<UploadReport>,
    ) -> PushOutcome {
        let head = match self.repo.refname_to_id(&self.branch_ref) {
            Ok(head) => head,
            Err(e) if e.code() == ErrorCode::NotFound => return PushOutcome::NothingToPush,
            Err(e) => return self.failed(PushFailureKind::Local, e.to_string(), None),
        };
        *head_out = Some(head);
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
        let attempt = self.attempt(&refspecs, &[]);
        let remote_branch = attempt
            .updates
            .iter()
            .find(|(name, _)| *name == self.branch_ref)
            .map(|(_, oid)| *oid)
            .filter(|oid| !oid.is_zero());

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
    /// none: then no LFS endpoint is resolved or contacted. Every object
    /// must be in the local cache.
    fn upload_lfs(&mut self, head: Oid) -> Result<Option<UploadReport>, LfsError> {
        let objects = objects_to_upload(&self.repo, head, self.state.last_pushed_oid())?;
        if objects.is_empty() {
            return Ok(None);
        }
        let missing: Vec<String> = objects
            .iter()
            .filter(|(p, _)| !self.lfs.contains(p))
            .map(|(p, path)| format!("{path} ({})", p.oid))
            .collect();
        if !missing.is_empty() {
            return Err(LfsError::local(format!(
                "{} LFS object(s) the unpushed commits reference are missing from the local \
                 cache (<state>/lfs/objects), so they cannot be uploaded: {}",
                missing.len(),
                missing
                    .iter()
                    .take(5)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
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
    }

    /// This remote's push lock file: held while a push runs, and by a
    /// repack ([`crate::repack`]) so it never deletes a pack a push reads.
    pub fn lock_path(&self) -> &Path {
        &self.lock_path
    }

    /// Takes this remote's push lock without waiting.
    fn lock(&self) -> io::Result<Option<File>> {
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&self.lock_path)?;
        match rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => Ok(Some(file)),
            Err(rustix::io::Errno::WOULDBLOCK) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// `<oid>:<ref>` for the branch and every
    /// `refs/agentfs/<volume>/diverged/*` ref, never forced. The patch
    /// queue's refs have their own push ([`RemotePusher::queue_push`]).
    fn refspecs(&self, head: Oid) -> Result<Vec<String>, GitError> {
        let mut refspecs = vec![format!("{head}:{}", self.branch_ref)];
        let glob = format!("refs/agentfs/{}/diverged/*", self.volume);
        for reference in self.repo.references_glob(&glob)? {
            let reference = reference?;
            let (Ok(name), Ok(target)) = (reference.name(), reference.resolve()) else {
                continue;
            };
            if let Some(oid) = target.target() {
                refspecs.push(format!("{oid}:{name}"));
            }
        }
        Ok(refspecs)
    }

    /// The patch queue push (design §11.3, §12), after the branch push,
    /// for the patch remote only. Two pushes:
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
                if branch_ok {
                    self.state.queue_synced_ms = Some(now_ms());
                    self.state.queue_synced_status = Some(status.to_string());
                }
            }
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
        let remote = self.list_remote()?;

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
        if !specs.is_empty()
            && let Some(error) = self.attempt_error(&self.attempt(&specs, &[]))
        {
            report.status = false;
            report.applied = 0;
            return Err(error);
        }
        if !branch_ok {
            return Ok(());
        }

        // Push 2: every remote inbox entry whose number is applied, the
        // applied patch or a stale one (replaced after it was applied),
        // leased on the oid listed.
        let deletes: Vec<(String, Oid)> = applied
            .keys()
            .map(|n| crate::patch::inbox_ref(&self.volume, *n))
            .filter_map(|name| remote.get(&name).map(|oid| (name, *oid)))
            .collect();
        if deletes.is_empty() {
            return Ok(());
        }
        let specs: Vec<String> = deletes.iter().map(|(name, _)| format!(":{name}")).collect();
        if let Some(error) = self.attempt_error(&self.attempt(&specs, &deletes)) {
            return Err(error);
        }
        report.deleted = deletes.len();
        Ok(())
    }

    /// The remote's `refs/agentfs/<volume>/*` refs, as its receive side
    /// advertises them.
    fn list_remote(&self) -> Result<BTreeMap<String, Oid>, (PushFailureKind, String)> {
        let auth = AuthState::default();
        let listed = (|| {
            let mut remote = self.repo.remote_anonymous(self.remote.push_url())?;
            let callbacks = self.auth.callbacks(&auth);
            let connection = remote.connect_auth(git2::Direction::Push, Some(callbacks), None)?;
            let prefix = format!("refs/agentfs/{}/", self.volume);
            Ok(connection
                .list()?
                .iter()
                .filter(|head| head.name().starts_with(&prefix))
                .map(|head| (head.name().to_owned(), head.oid()))
                .collect())
        })();
        listed.map_err(|e: GitError| {
            let attempt = Attempt {
                result: Err(e),
                negotiated: false,
                auth_failed: auth.auth_failed.get(),
                host_key_failure: auth.host_key_failure.borrow().clone(),
                updates: Vec::new(),
                rejected: Vec::new(),
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
        let negotiated = Cell::new(false);
        let auth = AuthState::default();
        let updates = RefCell::new(Vec::new());
        let rejected = RefCell::new(Vec::new());
        let result = (|| {
            let mut remote = self.repo.remote_anonymous(self.remote.push_url())?;
            let mut callbacks = self.auth.callbacks(&auth);
            callbacks.push_negotiation(|ups| {
                negotiated.set(true);
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
                }
                updates.borrow_mut().extend(ups.iter().map(|u| {
                    (
                        String::from_utf8_lossy(u.dst_refname_bytes()).into_owned(),
                        u.src(),
                    )
                }));
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
            remote.push(refspecs, Some(&mut options))
        })();
        Attempt {
            result,
            negotiated: negotiated.get(),
            auth_failed: auth.auth_failed.get(),
            host_key_failure: auth.host_key_failure.into_inner(),
            updates: updates.into_inner(),
            rejected: rejected.into_inner(),
        }
    }

    #[cfg(test)]
    fn answer(
        &self,
        url: &str,
        username: Option<&str>,
        allowed: CredentialType,
        calls: &Cell<u32>,
    ) -> Result<CredentialAnswer, String> {
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
            }
        }
    }

    /// Someone else wrote the branch: save the local head once per
    /// divergence, then fail like any other attempt.
    ///
    /// The ref name is chosen and saved in the state file as
    /// `pending_ref` before it is pushed, so a crash between the push and
    /// the state update retries the same name (the same or a descendant
    /// head: a no-op or a fast-forward) instead of saving the head again
    /// under a new timestamp.
    fn diverged(&mut self, head: Oid, remote_head: Option<Oid>, message: String) -> PushOutcome {
        let now = now_ms();
        let previous = self.state.diverged.take();
        let since_ms = previous.as_ref().map_or(now, |d| d.since_ms);
        let mut diverged_ref = previous.as_ref().and_then(|d| d.diverged_ref.clone());
        let mut pending_ref = previous.as_ref().and_then(|d| d.pending_ref.clone());
        let mut message = message;
        if diverged_ref.is_none() {
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
                Ok(()) => {
                    let attempt = self.attempt(&[format!("{head}:{name}")], &[]);
                    match (&attempt.result, attempt.rejected.first()) {
                        (Ok(()), None) => {
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
                        }
                        (Ok(()), Some((_, msg))) => {
                            // The remote refused this name; pick a fresh
                            // one next time.
                            message = format!("{message}; saving the head as {name}: {msg}");
                            pending_ref = None;
                        }
                        (Err(e), _) => {
                            message =
                                format!("{message}; saving the head as {name}: {}", e.message());
                        }
                    }
                }
            }
        }
        self.state.diverged = Some(Divergence {
            since_ms,
            local_head: head.to_string(),
            remote_head: remote_head.map(|o| o.to_string()),
            diverged_ref: diverged_ref.clone(),
            pending_ref,
        });
        self.failed(PushFailureKind::NonFastForward, message, Some(head));
        PushOutcome::Diverged {
            local_head: head,
            remote_head,
            diverged_ref,
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

/// A pusher for every configured remote (see [`RemotePusher::open`]).
pub fn open_all(
    state: &StateDir,
    settings: &Settings,
    events: Events,
    telemetry: &Telemetry,
) -> Result<Vec<RemotePusher>, PushError> {
    settings
        .remotes
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
        .iter()
        .map(|remote| {
            let push_state = PushState::load(&state.push_state(&remote.name));
            let unpushed = unpushed(&repo, &settings.branch_ref, push_state.last_pushed_oid())?
                .map(|w| w.count);
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
        .iter()
        .map(|remote| {
            let push_state = PushState::load(&state.push_state(&remote.name));
            let walk = unpushed(&repo, &settings.branch_ref, push_state.last_pushed_oid())?;
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
///   SSH host key refused by libgit2 itself) is an untrusted host;
/// - libssh2 failing to authenticate with the key it was given (a key it
///   cannot load, or one the server refuses outright) is an authentication
///   failure.
pub fn classify(err: &GitError, negotiated: bool) -> PushFailureKind {
    match err.code() {
        ErrorCode::Auth => return PushFailureKind::Auth,
        ErrorCode::NotFastForward => return PushFailureKind::NonFastForward,
        ErrorCode::Certificate => return PushFailureKind::UntrustedHost,
        ErrorCode::Timeout => return PushFailureKind::Unreachable,
        _ => {}
    }
    if err.class() == ErrorClass::Ssh
        && err
            .message()
            .starts_with("failed to authenticate SSH session")
    {
        return PushFailureKind::Auth;
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
        // libssh2 could not authenticate with the key it was given.
        assert_eq!(
            classify(
                &GitError::new(
                    ErrorCode::GenericError,
                    ErrorClass::Ssh,
                    "failed to authenticate SSH session: Unable to extract public key"
                ),
                false
            ),
            Auth
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
                    to: head
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
                    to: next
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
                to: c2
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

    /// Applied refs wait for the branch; the status goes anyway.
    #[test]
    fn applied_refs_wait_for_the_branch_push() {
        let remote_dir = tempfile::tempdir().unwrap();
        let remote = remote_dir.path();
        bare(remote);
        let mut v = Vol::new("vol", &[("origin", &remote.display().to_string())]);
        let c1 = v.commit();
        let mut pusher = v.pusher("origin", Arc::new(NullSink));
        assert!(pusher.push_once().is_success());
        write_remote(remote, "refs/heads/agentfs/vol");
        let c2 = v.commit();
        let repo = v.com.history().repo();
        repo.reference("refs/agentfs/vol/applied/0000000001", c1, false, "t")
            .unwrap();
        repo.reference("refs/agentfs/vol/status", c2, false, "t")
            .unwrap();
        let outcome = pusher.push_once();
        assert!(
            matches!(outcome, PushOutcome::Diverged { .. }),
            "{outcome:?}"
        );
        let queue = pusher.queue().unwrap();
        assert!(
            queue.status && queue.held_back && queue.error.is_none(),
            "{queue:?}"
        );
        assert_eq!(queue.applied, 0);
        assert_eq!(remote_ref(remote, "refs/agentfs/vol/status"), Some(c2));
        assert_eq!(
            remote_ref(remote, "refs/agentfs/vol/applied/0000000001"),
            None
        );
        assert_eq!(pusher.state().queue_synced_status, None);
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
                to: ours
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
                to: second
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
        let held = File::create(v.state.push_lock("origin")).unwrap();
        rustix::fs::flock(&held, rustix::fs::FlockOperation::LockExclusive).unwrap();
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
        v.settings.remotes[0].credentials = Some(cred);
        let pusher = v.pusher("origin", Arc::new(NullSink));
        let calls = Cell::new(0);
        let url = "https://git.example/o/r.git";
        assert_eq!(
            pusher.answer(url, None, CredentialType::USER_PASS_PLAINTEXT, &calls),
            Ok(CredentialAnswer::UserPass(Credential {
                username: crate::credentials::DEFAULT_USERNAME.into(),
                password: "tok".into()
            }))
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
    /// name are ever parsed before libssh2.
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
        let r = &mut v.settings.remotes[0];
        r.url = SSH_URL.into();
        r.ssh_key = Some(key.clone());
        r.known_hosts = Some(kh.clone());
        (v, key, kh)
    }

    #[test]
    fn ssh_remotes_get_one_key_per_connection() {
        let (v, _key, _kh) = ssh_vol(&crate::known_hosts::tests::ed25519(1));
        let pusher = v.pusher("origin", Arc::new(NullSink));
        let calls = Cell::new(0);
        // The user name asked before authenticating is not an attempt.
        for _ in 0..2 {
            assert_eq!(
                pusher.answer(SSH_URL, None, CredentialType::USERNAME, &calls),
                Ok(CredentialAnswer::Username("git".into()))
            );
        }
        assert_eq!(calls.get(), 0);
        let offered = CredentialType::SSH_KEY
            | CredentialType::SSH_CUSTOM
            | CredentialType::SSH_MEMORY
            | CredentialType::USER_PASS_PLAINTEXT;
        match pusher.answer(SSH_URL, Some("deploy"), offered, &calls) {
            Ok(CredentialAnswer::SshKey { username, key }) => {
                assert_eq!(username, "deploy");
                assert_eq!(key.text, openssh_key());
            }
            other => panic!("{other:?}"),
        }
        let again = pusher
            .answer(SSH_URL, Some("deploy"), offered, &calls)
            .unwrap_err();
        assert!(again.contains("rejected the SSH key"), "{again}");

        // No user from the URL or libgit2: `git`.
        let calls = Cell::new(0);
        assert!(matches!(
            pusher.answer(SSH_URL, None, offered, &calls),
            Ok(CredentialAnswer::SshKey { username, .. }) if username == "git"
        ));
        // Never for another host or port.
        for url in [
            "ssh://git@evil.example:2222/o/r.git",
            "ssh://git@git.example/o/r.git",
        ] {
            let calls = Cell::new(0);
            let err = pusher.answer(url, None, offered, &calls).unwrap_err();
            assert!(err.contains("bound to host"), "{err}");
        }
        // Never a password, even when the server offers only that.
        let calls = Cell::new(0);
        let err = pusher
            .answer(
                SSH_URL,
                None,
                CredentialType::USER_PASS_PLAINTEXT | CredentialType::SSH_INTERACTIVE,
                &calls,
            )
            .unwrap_err();
        assert!(err.contains("does not accept public keys"), "{err}");
    }

    #[test]
    fn ssh_key_problems_are_auth_errors() {
        let (mut v, key, _kh) = ssh_vol(&crate::known_hosts::tests::ed25519(1));
        let offered = CredentialType::SSH_KEY | CredentialType::SSH_MEMORY;
        fs::write(&key, "ssh-ed25519 AAAA me@host\n").unwrap();
        let pusher = v.pusher("origin", Arc::new(NullSink));
        let err = pusher
            .answer(SSH_URL, None, offered, &Cell::new(0))
            .unwrap_err();
        assert!(err.contains("no private key"), "{err}");
        assert!(err.contains(&key.display().to_string()), "{err}");
        fs::remove_file(&key).unwrap();
        assert!(
            pusher
                .answer(SSH_URL, None, offered, &Cell::new(0))
                .unwrap_err()
                .contains("reading credentials")
        );
        v.settings.remotes[0].ssh_key = None;
        let pusher = v.pusher("origin", Arc::new(NullSink));
        let err = pusher
            .answer(SSH_URL, None, offered, &Cell::new(0))
            .unwrap_err();
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

        /// A branch push that fails before git runs (stage lfs) still
        /// pushes the queue's status, and holds the applied refs back.
        #[test]
        fn a_failed_lfs_stage_still_pushes_the_queue_status() {
            let remote_dir = tempfile::tempdir().unwrap();
            bare(remote_dir.path());
            let url = remote_dir.path().display().to_string();
            let mut v = Vol::with_config("vol", &[("origin", &url)], &[THRESHOLD]);
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
            assert!(pusher.push_once().is_success());
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
