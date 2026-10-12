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
//! - **Anything else** (unreachable, authentication, refused) raises
//!   [`Event::PushFailed`] and backs off: the delay doubles from
//!   `pushInterval` up to `max(pushInterval, 15m)`.
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
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write as _};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use git2::{Cred, CredentialType, ErrorClass, ErrorCode, Oid, PushOptions, RemoteCallbacks};
use git2::{Error as GitError, Repository};
use serde::{Deserialize, Serialize};

use crate::config::{RemoteSettings, Settings};
use crate::credentials::CredentialFile;
use crate::events::{Event, Events, PushFailureKind};
use crate::history::init_libgit2;
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
        let dir = path.parent().unwrap_or(Path::new("."));
        let tmp = path.with_extension("json.tmp");
        let json = serde_json::to_vec_pretty(self).map_err(io::Error::other)?;
        {
            let mut file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)?;
            file.write_all(&json)?;
            file.sync_all()?;
        }
        fs::rename(&tmp, path)?;
        File::open(dir)?.sync_all()
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
    },
    /// Another process holds this remote's push lock.
    Busy,
}

impl PushOutcome {
    pub fn is_success(&self) -> bool {
        matches!(
            self,
            Self::NothingToPush | Self::UpToDate { .. } | Self::Pushed { .. }
        )
    }
}

/// What libgit2 reported for one push call.
struct Attempt {
    result: Result<(), GitError>,
    /// The remote's ref list was read and the update commands computed.
    negotiated: bool,
    /// The credentials callback refused or ran out of attempts.
    auth_failed: bool,
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
    credentials: Option<CredentialFile>,
    events: Events,
    state: PushState,
}

impl RemotePusher {
    pub fn open(
        state: &StateDir,
        settings: &Settings,
        remote: &RemoteSettings,
        events: Events,
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
            credentials: remote
                .credentials
                .as_ref()
                .map(|path| CredentialFile::new(path, remote.push_url())),
            remote: remote.clone(),
            events,
        })
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
    }

    /// One push attempt. Never fails: every problem becomes an outcome and,
    /// except for [`PushOutcome::Busy`], an event.
    pub fn push_once(&mut self) -> PushOutcome {
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

        let head = match self.repo.refname_to_id(&self.branch_ref) {
            Ok(head) => head,
            Err(e) if e.code() == ErrorCode::NotFound => return PushOutcome::NothingToPush,
            Err(e) => return self.failed(PushFailureKind::Local, e.to_string(), None),
        };
        let refspecs = match self.refspecs(head) {
            Ok(refspecs) => refspecs,
            Err(e) => return self.failed(PushFailureKind::Local, e.to_string(), Some(head)),
        };
        let attempt = self.attempt(&refspecs);
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
                let kind = if attempt.auth_failed {
                    PushFailureKind::Auth
                } else {
                    classify(e, attempt.negotiated)
                };
                self.failed(kind, e.message().to_owned(), Some(head))
            }
        }
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

    /// `<oid>:<ref>` for the branch and every `refs/agentfs/<volume>/*` ref,
    /// never forced.
    fn refspecs(&self, head: Oid) -> Result<Vec<String>, GitError> {
        let mut refspecs = vec![format!("{head}:{}", self.branch_ref)];
        let glob = format!("refs/agentfs/{}/*", self.volume);
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

    /// One libgit2 push of `refspecs`.
    fn attempt(&self, refspecs: &[String]) -> Attempt {
        let negotiated = Cell::new(false);
        let auth_failed = Cell::new(false);
        let credential_calls = Cell::new(0u32);
        let updates = RefCell::new(Vec::new());
        let rejected = RefCell::new(Vec::new());
        let result = (|| {
            let mut remote = self.repo.remote_anonymous(self.remote.push_url())?;
            let mut callbacks = RemoteCallbacks::new();
            callbacks.credentials(|url, username, allowed| {
                let result = self.credential(url, username, allowed, &credential_calls);
                if result.is_err() {
                    auth_failed.set(true);
                }
                result
            });
            callbacks.push_negotiation(|ups| {
                negotiated.set(true);
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
            auth_failed: auth_failed.get(),
            updates: updates.into_inner(),
            rejected: rejected.into_inner(),
        }
    }

    /// The credentials callback: one credential per connection, from the
    /// remote's credential file, for its own host only (design §11.1).
    fn credential(
        &self,
        url: &str,
        username: Option<&str>,
        allowed: CredentialType,
        calls: &Cell<u32>,
    ) -> Result<Cred, GitError> {
        let auth_error =
            |message: String| GitError::new(ErrorCode::Auth, ErrorClass::Callback, message);
        calls.set(calls.get() + 1);
        if calls.get() > 1 {
            return Err(auth_error(format!(
                "{url} rejected the credentials for remote {}",
                self.remote.name
            )));
        }
        let Some(file) = &self.credentials else {
            return Err(auth_error(format!(
                "{url} asks for credentials and remote.{}.agentfsCredentials is not set",
                self.remote.name
            )));
        };
        if !allowed.contains(CredentialType::USER_PASS_PLAINTEXT) {
            return Err(auth_error(format!(
                "{url} asks for {allowed:?}; only username and password (token) \
                 credentials are supported"
            )));
        }
        let credential = file
            .credential_for(url, username)
            .map_err(|e| auth_error(e.to_string()))?;
        Cred::userpass_plaintext(&credential.username, &credential.password)
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
        if let Err(e) = self.state.save(&self.state_path) {
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
            match self.state.save(&self.state_path) {
                Err(e) => {
                    // Without a durable name, pushing could save the head
                    // twice; try again at the next attempt.
                    message = format!(
                        "{message}; not saving the head as {name}: saving {}: {e}",
                        self.state_path.display()
                    );
                }
                Ok(()) => {
                    let attempt = self.attempt(&[format!("{head}:{name}")]);
                    match (&attempt.result, attempt.rejected.first()) {
                        (Ok(()), None) => {
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
        // libgit2 quotes the URL in its messages; never persist or emit the
        // userinfo it may carry.
        let message = scrub_userinfo(&message, self.remote.push_url());
        self.state.consecutive_failures = self.state.consecutive_failures.saturating_add(1);
        self.state.last_error = Some(LastError {
            kind,
            message: message.clone(),
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
        });
        PushOutcome::Failed {
            kind,
            message,
            retry_in,
        }
    }
}

/// A pusher for every configured remote.
pub fn open_all(
    state: &StateDir,
    settings: &Settings,
    events: Events,
) -> Result<Vec<RemotePusher>, PushError> {
    settings
        .remotes
        .iter()
        .map(|remote| RemotePusher::open(state, settings, remote, events.clone()))
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
            let unpushed = unpushed(&repo, &settings.branch_ref, push_state.last_pushed_oid())?;
            Ok(RemoteStatus {
                name: remote.name.clone(),
                url: redact_url(remote.push_url()),
                state: push_state,
                unpushed,
            })
        })
        .collect()
}

fn unpushed(
    repo: &Repository,
    branch_ref: &str,
    last_pushed: Option<Oid>,
) -> Result<Option<usize>, GitError> {
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
    for oid in walk {
        oid?;
        count += 1;
    }
    Ok(Some(count))
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
fn scrub_userinfo(message: &str, url: &str) -> String {
    match userinfo(url) {
        Some(info) => message.replace(info, "***"),
        None => message.to_owned(),
    }
}

/// Maps a failed push to a failure kind. `negotiated` says whether libgit2
/// got as far as reading the remote's refs; anything failing before that is
/// treated as the remote being unreachable.
pub fn classify(err: &GitError, negotiated: bool) -> PushFailureKind {
    match err.code() {
        ErrorCode::Auth => return PushFailureKind::Auth,
        ErrorCode::NotFastForward => return PushFailureKind::NonFastForward,
        ErrorCode::Certificate | ErrorCode::Timeout => return PushFailureKind::Unreachable,
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

    use super::*;
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
            let dir = tempfile::tempdir().unwrap();
            let state = StateDir::new(dir.path().join("state"));
            volume::init(&state, volume, None).unwrap();
            let mut config = git2::Config::open(&state.git().join("config")).unwrap();
            for (name, url) in remotes {
                config.set_str(&format!("remote.{name}.url"), url).unwrap();
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

        fn head(&self) -> Oid {
            self.com.history().head().unwrap().unwrap().id()
        }

        fn pusher(&self, remote: &str, events: Events) -> RemotePusher {
            let r = self
                .settings
                .remotes
                .iter()
                .find(|r| r.name == remote)
                .unwrap();
            RemotePusher::open(&self.state, &self.settings, r, events).unwrap()
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
        assert_eq!(
            classify(&e(ErrorCode::Certificate, ErrorClass::Ssl), true),
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
            "refs/agentfs/vol/status",
            "refs/heads/other",
            "refs/agentfs/other/x",
            "refs/agentfs/volx/y",
        ] {
            repo.reference(name, head, false, "test").unwrap();
        }
        let mut pusher = v.pusher("origin", Arc::new(NullSink));
        assert!(pusher.push_once().is_success());
        assert_eq!(
            remote_refs(remote_dir.path()),
            ["refs/agentfs/vol/status", "refs/heads/agentfs/vol"]
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
        v.commit();
        let outcome = pusher.push_once();
        assert!(
            matches!(
                outcome,
                PushOutcome::Failed {
                    kind: PushFailureKind::NonFastForward,
                    ..
                }
            ),
            "{outcome:?}"
        );
        assert!(diverged_refs(remote, "vol").is_empty());
        assert_eq!(remote_ref(remote, "refs/agentfs/vol/status"), Some(theirs));
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
        for mut pusher in open_all(&v.state, &v.settings, Arc::new(NullSink)).unwrap() {
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
            .credential(
                "https://h/r",
                None,
                CredentialType::USER_PASS_PLAINTEXT,
                &calls,
            )
            .err()
            .unwrap();
        assert_eq!(err.code(), ErrorCode::Auth);
        assert_eq!(classify(&err, true), PushFailureKind::Auth);
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
        assert!(
            pusher
                .credential(url, None, CredentialType::USER_PASS_PLAINTEXT, &calls)
                .is_ok()
        );
        let again = pusher
            .credential(url, None, CredentialType::USER_PASS_PLAINTEXT, &calls)
            .err()
            .unwrap();
        assert_eq!(again.code(), ErrorCode::Auth);
        // Never for another host, and never for SSH keys.
        let calls = Cell::new(0);
        assert!(
            pusher
                .credential(
                    "https://evil.example/o/r.git",
                    None,
                    CredentialType::USER_PASS_PLAINTEXT,
                    &calls
                )
                .is_err()
        );
        let calls = Cell::new(0);
        assert!(
            pusher
                .credential(url, None, CredentialType::SSH_KEY, &calls)
                .is_err()
        );
    }
}
