//! Telling the integrator what the patch queue decided (bead agentfs-mia.5;
//! design §19.1 "Integrators decide when patches are applied": the
//! *notify* primitive).
//!
//! After every queue run ([`super::queue::Queue::process`], at mount or by
//! `agentfs patch apply`) and every judging under a daemon
//! ([`super::queue::Queue::judge`]), [`deliver`] compares the verdicts in
//! the status just written with those of the last status whose notice was
//! delivered, and when anything changed sends one [`Notice`] to every
//! configured [`Notifier`]:
//!
//! - [`HookNotifier`]: the operator's `$GIT_DIR/hooks/patch-notify`, when
//!   it exists, run through the command runner shared with fork delivery
//!   ([`crate::command::run_with_stdin`]) with the notice as JSON on
//!   stdin; exit 0 is delivered, anything else failed;
//! - [`WebhookNotifier`]: a POST of the notice to `agentfs.patchNotifyUrl`
//!   with `Idempotency-Key: <volume>:<status commit>` and, with
//!   `agentfs.patchNotifyCredentialFile`, a bearer token, through the one
//!   HTTP POST fork delivery's webhook uses ([`crate::lfs::client::post`]);
//!   a 2xx is delivered, anything else failed.
//!
//! The last delivered status is kept in `meta/notify.json`
//! ([`NotifyCursor`], [`crate::volume::StateDir::notify_state`]). It moves
//! only when every method delivered; a failure raises `NotifyFailed` and
//! leaves it (noting the patches that run landed, which no status keeps),
//! so the next queue event sends the same changes again (events, no timer,
//! no retry loop). It is a cache: lost or unreadable, every queued patch's
//! verdict is sent again (landings it noted are not). Delivery is at least once; a receiver dedupes on
//! (volume, inbox, patch, verdict).

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use git2::{Oid, Repository};
use serde::{Deserialize, Serialize};

use super::status::{StatusDoc, read_status};
use super::{PatchError, Verdict};
use crate::command::{Finished, RunError, run_with_stdin};
use crate::config::Settings;
use crate::events::{Event, Events};
use crate::history::History;
use crate::volume::StateDir;

/// The notice's schema version: a receiver refuses one it does not know.
pub const NOTICE_SCHEMA: u32 = 1;

/// The `patch-notify` hook, under the state area's git directory.
pub const NOTIFY_HOOK: &str = "hooks/patch-notify";

/// What the hook and the webhook are sent, as JSON (schema 1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Notice {
    /// [`NOTICE_SCHEMA`].
    pub schema: u32,
    pub volume: String,
    /// The branch's full ref name.
    pub branch: String,
    /// The branch head the status was written at.
    pub head: Option<String>,
    /// The status commit (`refs/agentfs/<volume>/status`) this notice
    /// reports.
    pub status: String,
    /// What changed since the last delivered notice, by inbox number.
    pub changes: Vec<VerdictChange>,
    /// Set when the notice answers an apply call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub apply: Option<ApplyAnswer>,
}

/// One queued patch whose verdict changed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerdictChange {
    pub inbox: u64,
    /// The patch (its tip) as queued now, else as last delivered.
    pub patch: String,
    /// None: newly queued / gone from the queue (landed: see `landed`).
    /// A queued patch that was not reached also has none.
    pub from: Option<Verdict>,
    pub to: Option<Verdict>,
    /// The branch commit that landed it, when it landed in this run.
    #[serde(default)]
    pub landed: Option<String>,
}

/// What an apply call was answered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ApplyAnswer {
    /// Nothing was attached: the queue ran, and these landed.
    Landed { landed: Vec<u64> },
    /// A daemon holds the volume: judged only, lands at the next mount.
    Attached { message: String },
}

/// One way of telling the integrator.
pub trait Notifier {
    /// `hook` or `webhook`: `NotifyFailed`'s method.
    fn name(&self) -> &'static str;
    fn notify(&self, notice: &Notice) -> Result<(), NotifyError>;
}

#[derive(Debug, thiserror::Error)]
pub enum NotifyError {
    #[error("the patch-notify hook {0}")]
    Hook(String),
    #[error("the notify webhook {0}")]
    Webhook(String),
}

/// `$GIT_DIR/hooks/patch-notify`, run without a shell.
pub struct HookNotifier {
    hook: PathBuf,
    timeout: Duration,
}

impl HookNotifier {
    pub fn new(hook: PathBuf, timeout: Duration) -> Self {
        Self { hook, timeout }
    }
}

impl Notifier for HookNotifier {
    fn name(&self) -> &'static str {
        "hook"
    }

    fn notify(&self, notice: &Notice) -> Result<(), NotifyError> {
        let stdin = serde_json::to_vec(notice).map_err(|e| NotifyError::Hook(e.to_string()))?;
        let argv = [self.hook.to_string_lossy().into_owned()];
        let hook = self.hook.display();
        match run_with_stdin(&argv, &stdin, self.timeout) {
            Ok(Finished::Code(0)) => Ok(()),
            Ok(Finished::Code(code)) => {
                Err(NotifyError::Hook(format!("{hook} exited with {code}")))
            }
            Ok(Finished::Signal(signal)) => Err(NotifyError::Hook(format!(
                "{hook} was killed by signal {signal}"
            ))),
            Err(e @ (RunError::NotStarted(_) | RunError::TimedOut(_) | RunError::Wait(_))) => {
                Err(NotifyError::Hook(format!("{hook}: {e}")))
            }
        }
    }
}

/// A POST of the notice to `agentfs.patchNotifyUrl`.
pub struct WebhookNotifier {
    url: String,
    credential_file: Option<PathBuf>,
    timeout: Duration,
}

impl WebhookNotifier {
    pub fn new(url: String, credential_file: Option<PathBuf>, timeout: Duration) -> Self {
        Self {
            url,
            credential_file,
            timeout,
        }
    }
}

impl Notifier for WebhookNotifier {
    fn name(&self) -> &'static str {
        "webhook"
    }

    fn notify(&self, notice: &Notice) -> Result<(), NotifyError> {
        let body = serde_json::to_vec(notice).map_err(|e| NotifyError::Webhook(e.to_string()))?;
        let mut headers = vec![(
            "Idempotency-Key",
            format!("{}:{}", notice.volume, notice.status),
        )];
        if let Some(file) = &self.credential_file {
            let token = fs::read_to_string(file).map_err(|e| {
                NotifyError::Webhook(format!(
                    "credential file {} cannot be read: {e}",
                    file.display()
                ))
            })?;
            headers.push(("Authorization", format!("Bearer {}", token.trim())));
        }
        match crate::lfs::client::post_json(&self.url, &body, &headers, self.timeout) {
            Ok(200..=299) => Ok(()),
            Ok(status) => Err(NotifyError::Webhook(format!(
                "{} answered {status}",
                self.url
            ))),
            Err(e) => Err(NotifyError::Webhook(e)),
        }
    }
}

/// Every configured method (hook if present, webhook if set), in that
/// order.
pub fn notifiers(settings: &Settings, git_dir: &Path) -> Vec<Box<dyn Notifier>> {
    let mut out: Vec<Box<dyn Notifier>> = Vec::new();
    let hook = git_dir.join(NOTIFY_HOOK);
    if hook.exists() {
        out.push(Box::new(HookNotifier::new(
            hook,
            settings.patch_notify_timeout,
        )));
    }
    if let Some(url) = &settings.patch_notify_url {
        out.push(Box::new(WebhookNotifier::new(
            url.clone(),
            settings.patch_notify_credential_file.clone(),
            settings.patch_notify_timeout,
        )));
    }
    out
}

/// `meta/notify.json`: the status commit whose changes were last
/// delivered to every method.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct NotifyCursor {
    pub delivered_status: Option<String>,
    /// Patches landed (inbox number, branch commit) by runs whose notice
    /// failed: a status does not keep them, so they are sent again from
    /// here until a notice is delivered.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub undelivered_landed: Vec<(u64, String)>,
}

impl NotifyCursor {
    /// The cursor at `path`; empty if it is missing or unreadable.
    pub fn load(path: &Path) -> Self {
        fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    /// Writes atomically.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        crate::fsutil::write_json_atomic(path, self)
    }
}

/// The changes from `previous` (the last delivered) to `now`. `landed`
/// are the patches the run that wrote `now` landed, with their commits.
/// A verdict counts as changed on any difference, its detail included
/// (held `conflict` to held `refused`, another message).
pub fn changes(
    previous: Option<&StatusDoc>,
    now: &StatusDoc,
    landed: &[(u64, Oid)],
) -> Vec<VerdictChange> {
    let before: BTreeMap<u64, (&str, Option<&Verdict>)> = previous
        .map(|p| {
            p.queue
                .iter()
                .map(|q| (q.inbox, (q.patch.as_str(), q.verdict.as_ref())))
                .collect()
        })
        .unwrap_or_default();
    let landed: BTreeMap<u64, Oid> = landed.iter().copied().collect();
    let mut out: BTreeMap<u64, VerdictChange> = BTreeMap::new();
    for q in &now.queue {
        let was = before.get(&q.inbox);
        if was.is_some_and(|(patch, verdict)| *patch == q.patch && *verdict == q.verdict.as_ref()) {
            continue;
        }
        out.insert(
            q.inbox,
            VerdictChange {
                inbox: q.inbox,
                patch: q.patch.clone(),
                from: was.and_then(|(_, v)| v.cloned()),
                to: q.verdict.clone(),
                landed: None,
            },
        );
    }
    let queued_now = |n: &u64| now.queue.iter().any(|q| q.inbox == *n);
    for (n, (patch, verdict)) in &before {
        if !queued_now(n) {
            out.insert(
                *n,
                VerdictChange {
                    inbox: *n,
                    patch: (*patch).to_owned(),
                    from: verdict.cloned(),
                    to: None,
                    landed: landed.get(n).map(Oid::to_string),
                },
            );
        }
    }
    // Queued and landed between two notices: the patch is not in either
    // status ([`deliver`] reads it from the commit).
    for (n, commit) in &landed {
        if !before.contains_key(n) && !queued_now(n) {
            out.insert(
                *n,
                VerdictChange {
                    inbox: *n,
                    patch: String::new(),
                    from: None,
                    to: None,
                    landed: Some(commit.to_string()),
                },
            );
        }
    }
    out.into_values().collect()
}

/// What [`deliver`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Delivery {
    /// A notice was sent (to at least one method).
    pub sent: bool,
    /// The methods that failed (`NotifyFailed` was raised for each).
    pub failed: Vec<String>,
}

/// Compares the status commit `now` with the last delivered one, sends
/// the changes to each notifier, and moves the cursor on full success;
/// emits `NotifyFailed` per failed method (see the module docs). With
/// nothing changed, or no method configured, nothing is sent and the
/// cursor moves to `now`.
pub fn deliver(
    state: &StateDir,
    repo: &Repository,
    now: Oid,
    landed: &[(u64, Oid)],
    apply: Option<ApplyAnswer>,
    notifiers: &[Box<dyn Notifier>],
    events: &Events,
) -> Result<Delivery, PatchError> {
    let path = state.notify_state();
    let mut cursor = NotifyCursor::load(&path);
    let previous = cursor
        .delivered_status
        .as_deref()
        .and_then(|s| Oid::from_str(s).ok())
        .and_then(|oid| read_status(repo, oid).ok());
    let doc = read_status(repo, now)?;
    let mut all_landed: BTreeMap<u64, Oid> = cursor
        .undelivered_landed
        .iter()
        .filter_map(|(n, c)| Oid::from_str(c).ok().map(|c| (*n, c)))
        .collect();
    all_landed.extend(landed.iter().copied());
    let all_landed: Vec<(u64, Oid)> = all_landed.into_iter().collect();
    let mut changes = changes(previous.as_ref(), &doc, &all_landed);
    for change in changes.iter_mut().filter(|c| c.patch.is_empty()) {
        change.patch = change
            .landed
            .as_deref()
            .and_then(|c| Oid::from_str(c).ok())
            .and_then(|c| repo.find_commit(c).ok())
            .and_then(|c| super::inbox_from_message(c.message_raw().unwrap_or_default()))
            .map(|(_, patch)| patch.to_string())
            .unwrap_or_default();
    }
    let mut delivery = Delivery::default();
    if !changes.is_empty() && !notifiers.is_empty() {
        let notice = Notice {
            schema: NOTICE_SCHEMA,
            volume: doc.volume.clone(),
            branch: doc.branch.clone(),
            head: doc.head.clone(),
            status: now.to_string(),
            changes,
            apply,
        };
        delivery.sent = true;
        for notifier in notifiers {
            if let Err(e) = notifier.notify(&notice) {
                events.emit(Event::NotifyFailed {
                    method: notifier.name(),
                    message: e.to_string(),
                });
                delivery.failed.push(notifier.name().to_owned());
            }
        }
    }
    if delivery.failed.is_empty() {
        cursor.delivered_status = Some(now.to_string());
        cursor.undelivered_landed.clear();
    } else {
        cursor.undelivered_landed = all_landed
            .iter()
            .map(|(n, c)| (*n, c.to_string()))
            .collect();
    }
    cursor.save(&path)?;
    Ok(delivery)
}

/// [`deliver`] to every configured method ([`notifiers`], from
/// `history`'s settings and the state area's git directory) of the status
/// `now` written in `history`.
pub fn tell(
    state: &StateDir,
    history: &History,
    now: Oid,
    landed: &[(u64, Oid)],
    apply: Option<ApplyAnswer>,
    events: &Events,
) -> Result<Delivery, PatchError> {
    let notifiers = notifiers(history.settings(), &state.git());
    deliver(
        state,
        history.repo(),
        now,
        landed,
        apply,
        &notifiers,
        events,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::patch::BlockReason;
    use crate::patch::status::{QueueEntry, STATUS_VERSION};

    fn held(reason: BlockReason) -> Verdict {
        Verdict::Held {
            reason,
            message: "m".into(),
            paths: vec!["b.txt".into()],
        }
    }

    fn doc(queue: &[(u64, &str, Option<Verdict>)]) -> StatusDoc {
        StatusDoc {
            version: STATUS_VERSION,
            volume: "vol".into(),
            branch: "refs/heads/agentfs/vol".into(),
            head: None,
            updated: "20261029T120000Z".into(),
            last_applied: None,
            applied_now: Vec::new(),
            duplicate_now: Vec::new(),
            queue: queue
                .iter()
                .map(|(n, p, v)| QueueEntry {
                    inbox: *n,
                    patch: (*p).into(),
                    verdict: v.clone(),
                })
                .collect(),
            blocked: None,
            ignored: Vec::new(),
            fetch: None,
        }
    }

    #[test]
    fn nothing_changed_is_no_change() {
        let d = doc(&[(2, "p2", Some(held(BlockReason::Conflict)))]);
        assert!(changes(Some(&d), &d, &[]).is_empty());
    }

    /// P3: a change of detail inside one kind is a change.
    #[test]
    fn a_verdicts_detail_is_a_change() {
        let before = doc(&[(2, "p2", Some(held(BlockReason::Conflict)))]);
        let after = doc(&[(2, "p2", Some(held(BlockReason::Refused)))]);
        let c = changes(Some(&before), &after, &[]);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].from, Some(held(BlockReason::Conflict)));
        assert_eq!(c[0].to, Some(held(BlockReason::Refused)));
    }

    #[test]
    fn new_replaced_gone_and_landed() {
        let commit = Oid::from_str(&"c".repeat(40)).unwrap();
        let before = doc(&[
            (2, "p2", Some(held(BlockReason::Conflict))),
            (3, "p3", Some(Verdict::Ready)),
            (4, "p4", None),
        ]);
        let after = doc(&[
            (3, "p3-new", Some(Verdict::Ready)),
            (5, "p5", Some(Verdict::Ready)),
        ]);
        let c = changes(Some(&before), &after, &[(2, commit), (6, commit)]);
        let summary: Vec<_> = c
            .iter()
            .map(|c| {
                (
                    c.inbox,
                    c.patch.as_str(),
                    c.from.as_ref().map(Verdict::kind),
                    c.to.as_ref().map(Verdict::kind),
                    c.landed.is_some(),
                )
            })
            .collect();
        assert_eq!(
            summary,
            [
                (2, "p2", Some("held"), None, true),
                (3, "p3-new", Some("ready"), Some("ready"), false),
                (4, "p4", None, None, false),
                (5, "p5", None, Some("ready"), false),
                (6, "", None, None, true),
            ]
        );
        // The first notice: everything queued is new, one not reached too.
        let first = changes(None, &before, &[]);
        assert_eq!(first.len(), 3, "{first:?}");
    }

    #[test]
    fn the_notice_is_schema_1() {
        let notice = Notice {
            schema: NOTICE_SCHEMA,
            volume: "vol".into(),
            branch: "refs/heads/agentfs/vol".into(),
            head: Some("h".into()),
            status: "s".into(),
            changes: vec![VerdictChange {
                inbox: 1,
                patch: "p".into(),
                from: None,
                to: Some(Verdict::Ready),
                landed: None,
            }],
            apply: Some(ApplyAnswer::Attached {
                message: "workload attached; lands at next mount".into(),
            }),
        };
        let json = serde_json::to_value(&notice).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "schema": 1,
                "volume": "vol",
                "branch": "refs/heads/agentfs/vol",
                "head": "h",
                "status": "s",
                "changes": [{
                    "inbox": 1, "patch": "p", "from": null,
                    "to": {"kind": "ready"}, "landed": null,
                }],
                "apply": {"kind": "attached", "message": "workload attached; lands at next mount"},
            })
        );
        let back: Notice = serde_json::from_value(json).unwrap();
        assert_eq!(back, notice);
        let landed = serde_json::to_value(ApplyAnswer::Landed { landed: vec![1] }).unwrap();
        assert_eq!(landed, serde_json::json!({"kind": "landed", "landed": [1]}));
    }
}
