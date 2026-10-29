//! Fetching the volume's patch queue from the patch remote (design §12).
//!
//! Only `refs/agentfs/<volume>/inbox/*` and `refs/agentfs/<volume>/approved/*`
//! (`agentfs patch approve`, bead agentfs-mia.2) are fetched, forced (a
//! producer may replace a queued patch, an approver approve another tip),
//! with every commit a patch needs, into the same names in the history
//! repository. Refs of either kind the remote no longer has are deleted
//! locally, so a patch withdrawn on the remote is not applied and a
//! withdrawn approval no longer counts.
//!
//! The fetch is best effort: it runs on its own thread with its own
//! repository handle, and the caller waits at most
//! `agentfs.patchFetchTimeout`. On the time limit the fetch is told to stop
//! at its next progress callback and the caller goes on with the inbox refs
//! it already has; libgit2's connect and I/O timeouts
//! (`history::init_libgit2`) bound how long the abandoned thread can
//! linger. Authentication is the push's ([`crate::remote_auth`]): the
//! remote's credentials, SSH key and known hosts, for its push URL.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use git2::{AutotagOption, ErrorCode, FetchOptions, FetchPrune, Repository};

use super::{approved_prefix, inbox_prefix};
use crate::config::RemoteSettings;
use crate::events::PushFailureKind;
use crate::history::init_libgit2;
use crate::push::{classify, redact_url};
use crate::remote_auth::{AuthState, RemoteAuth};

/// How one fetch went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchOutcome {
    pub remote: String,
    pub ok: bool,
    /// Why it failed.
    pub kind: Option<PushFailureKind>,
    pub message: Option<String>,
    /// Inbox refs the remote has.
    pub remote_inbox: usize,
    /// Local inbox and approved refs deleted because the remote no longer
    /// has them.
    pub pruned: usize,
}

impl FetchOutcome {
    fn failed(remote: &str, kind: PushFailureKind, message: String) -> Self {
        Self {
            remote: remote.to_owned(),
            ok: false,
            kind: Some(kind),
            message: Some(message),
            remote_inbox: 0,
            pruned: 0,
        }
    }
}

/// Fetches `volume`'s inbox refs from `remote` into the repository at
/// `git_dir`, waiting at most `timeout`.
pub fn fetch_inbox(
    git_dir: &Path,
    remote: &RemoteSettings,
    volume: &str,
    timeout: Duration,
) -> FetchOutcome {
    let cancel = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel();
    let git_dir: PathBuf = git_dir.to_owned();
    let settings = remote.clone();
    let volume = volume.to_owned();
    let stop = cancel.clone();
    let spawned = std::thread::Builder::new()
        .name("agentfs-patch-fetch".into())
        .spawn(move || {
            let outcome = fetch_now(&git_dir, &settings, &volume, &stop);
            let _ = tx.send(outcome);
        });
    if let Err(e) = spawned {
        return FetchOutcome::failed(
            &remote.name,
            PushFailureKind::Local,
            format!("starting the fetch thread: {e}"),
        );
    }
    match rx.recv_timeout(timeout) {
        Ok(outcome) => outcome,
        Err(_) => {
            cancel.store(true, Ordering::SeqCst);
            FetchOutcome::failed(
                &remote.name,
                PushFailureKind::Unreachable,
                format!(
                    "no answer from {} within agentfs.patchFetchTimeout ({timeout:?})",
                    redact_url(remote.push_url())
                ),
            )
        }
    }
}

/// The fetch itself, on the calling thread.
fn fetch_now(
    git_dir: &Path,
    remote: &RemoteSettings,
    volume: &str,
    cancel: &AtomicBool,
) -> FetchOutcome {
    init_libgit2();
    let name = &remote.name;
    let url = remote.push_url();
    let repo = match Repository::open_bare(git_dir) {
        Ok(repo) => repo,
        Err(e) => return FetchOutcome::failed(name, PushFailureKind::Local, e.to_string()),
    };
    let prefix = inbox_prefix(volume);
    let approved = approved_prefix(volume);
    let refspecs = [
        format!("+{prefix}*:{prefix}*"),
        format!("+{approved}*:{approved}*"),
    ];
    let auth = RemoteAuth::new(remote);
    let state = AuthState::new();
    let _scope = auth.ssh_scope(&state);
    let mut git_remote = match repo.remote_anonymous(url) {
        Ok(r) => r,
        Err(e) => return FetchOutcome::failed(name, PushFailureKind::Local, e.to_string()),
    };
    let result = {
        let mut callbacks = auth.callbacks(&state);
        callbacks.transfer_progress(|_| !cancel.load(Ordering::SeqCst));
        callbacks.sideband_progress(|_| !cancel.load(Ordering::SeqCst));
        callbacks.update_tips(|_, _, _| !cancel.load(Ordering::SeqCst));
        let mut options = FetchOptions::new();
        options
            .remote_callbacks(callbacks)
            .proxy_options(crate::proxy::git_options(remote.proxy.as_ref(), url))
            .prune(FetchPrune::Off)
            .update_fetchhead(false)
            .download_tags(AutotagOption::None);
        git_remote.fetch(&refspecs, Some(&mut options), None)
    };
    if let Err(e) = result {
        let message = scrub(e.message(), url);
        if let Some(why) = state.host_key_failure() {
            return FetchOutcome::failed(name, PushFailureKind::UntrustedHost, why);
        }
        let kind = if state.auth_failed() {
            PushFailureKind::Auth
        } else if e.code() == ErrorCode::User || cancel.load(Ordering::SeqCst) {
            PushFailureKind::Unreachable
        } else {
            match classify(&e, false) {
                PushFailureKind::Refused | PushFailureKind::NonFastForward => {
                    PushFailureKind::Unreachable
                }
                kind => kind,
            }
        };
        return FetchOutcome::failed(name, kind, message);
    }
    if cancel.load(Ordering::SeqCst) {
        return FetchOutcome::failed(name, PushFailureKind::Unreachable, "cancelled".into());
    }
    // The advertisement stays readable after the fetch disconnects.
    let on_remote: BTreeSet<String> = match git_remote.list() {
        Ok(heads) => heads
            .iter()
            .map(|h| h.name().to_owned())
            .filter(|n| n.starts_with(&prefix) || n.starts_with(&approved))
            .collect(),
        Err(e) => return FetchOutcome::failed(name, PushFailureKind::Local, e.to_string()),
    };
    let mut pruned = 0;
    let mut local = Vec::new();
    for glob in [&prefix, &approved] {
        match repo.references_glob(&format!("{glob}*")) {
            Ok(refs) => local.extend(
                refs.filter_map(Result::ok)
                    .filter_map(|r| r.name().ok().map(str::to_owned)),
            ),
            Err(e) => return FetchOutcome::failed(name, PushFailureKind::Local, e.to_string()),
        }
    }
    for local in local {
        if !on_remote.contains(&local)
            && let Ok(mut r) = repo.find_reference(&local)
            && r.delete().is_ok()
        {
            pruned += 1;
        }
    }
    let remote_inbox = on_remote.iter().filter(|n| n.starts_with(&prefix)).count();
    FetchOutcome {
        remote: name.clone(),
        ok: true,
        kind: None,
        message: None,
        remote_inbox,
        pruned,
    }
}

/// `message` without the URL's userinfo.
fn scrub(message: &str, url: &str) -> String {
    crate::push::scrub_userinfo(message, url)
}
