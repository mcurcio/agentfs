//! Applying the patch queue (design §12).
//!
//! [`Queue::process`] runs with the volume open and nothing serving the
//! tree: at mount, after journal replay and before the FUSE layer starts,
//! or from `agentfs patch apply` with the state lock held. It:
//!
//! 1. commits what is journaled, so the branch head is the tree's state;
//! 2. reads the local inbox refs (fetched by [`super::fetch`]) and scans
//!    the branch for `Agentfs-Inbox` trailers (incrementally from the
//!    head cached in `meta/patch.json`, when that is still on the branch);
//! 3. finishes the bookkeeping of inbox entries the branch already applies
//!    (a crash between the commit and the ref moves), and reports other
//!    entries at or below the last applied number as ignored, except a
//!    number this queue set aside before (a local
//!    `refs/agentfs/<volume>/held/<n>` mark, [`super::held_ref`]), which
//!    is judged again whatever commit it names now; a mark whose number the
//!    branch applied, or whose inbox entry is gone (`withdrawn`), is
//!    deleted;
//! 4. judges the rest in order: the set-aside numbers and every entry above
//!    the last applied one, which must each be the next number (one past
//!    the highest applied or judged so far; a missing one stops the run
//!    with `gap`). Each patch is a
//!    branch ([`PatchBranch`]): a line of single-parent commits on a base,
//!    the merge base of its tip and the head, changing at most
//!    [`super::MAX_PATCH_PATHS`] paths and no gitlinks. It waits behind an
//!    earlier set-aside barrier ([`super::BARRIER_TRAILER`]) or an earlier
//!    set-aside patch changing one of its paths; else it is merged onto
//!    the head by the context's [`Integrator`] (`git merge-tree`, a
//!    three-way merge against the base), judged by the context's
//!    [`AcceptanceRule`], written into the
//!    backing tree with the restore engine ([`crate::restore`]), and
//!    recorded as one atomic record of a patch operation, so it becomes one
//!    commit carrying `Agentfs-Inbox: <n>`. A patch that changes nothing
//!    gets a marker commit with the head's tree;
//! 5. moves the refs (`applied/<n>` = the patch's tip, `inbox/<n>` and
//!    `held/<n>` deleted). A patch that cannot land is set aside whole
//!    with a [`Verdict`] (held, for the reasons
//!    [`BlockReason::holds_whole`] names; waiting; awaiting the rule; not
//!    judged, when the rule could not judge), marked `held/<n>` before the
//!    next number is judged, and the queue goes on; the run stops only at a
//!    gap, unrecorded changes or a failure ([`Blocked`]). Set-aside patches
//!    are judged again at every run (bead agentfs-mia.2);
//! 6. writes the status ref and `meta/patch.json`.
//!
//! Every crash point converges on one commit per patch: a crash after the
//! tree write leaves the tree holding the patch's content, which the next
//! run's check keeps as steps with nothing to write and records; a crash
//! after the record is committed by journal replay and found by the
//! trailer scan; a crash after the commit leaves only the ref moves, which
//! step 3 does. The patch being written is noted in `meta/patch.json`
//! ([`InFlight`]) first, so if a crash after the tree write is followed by
//! the producer replacing, skipping or withdrawing that patch, the next run
//! puts the paths it wrote back to the branch head's state (or, when that
//! cannot be done, records them as they stand, as a patch operation
//! without an inbox number) before it applies anything: the tree never
//! keeps content that history does not have.
//!
//! A write that fails part way is recorded as it stands, as a patch
//! operation without an inbox number, so the tree matches history and the
//! next merge picks up from there. A patch whose atomic record is refused
//! (at record or commit time) is written back out of the tree.

use std::collections::BTreeMap;

use git2::{Oid, Repository};
use opentelemetry::KeyValue;

use super::accept::{Acceptance, AcceptanceRule, Candidate};
use super::fetch::FetchOutcome;
use super::integrate::{Integrated, Integrator, PatchBranch, Refusal};
use super::status::{
    AppliedEntry, DuplicateEntry, FetchStatus, InFlight, PatchState, QueueEntry, STATUS_VERSION,
    StatusDoc, write_status_ref,
};
use super::{
    BlockReason, Blocked, Ignored, PatchError, Verdict, WaitReason, applied_ref, held_prefix,
    held_ref, inbox_from_message, inbox_prefix, inbox_ref, seq_of_ref,
};
use crate::confined::Dir;
use crate::events::Event;
use crate::history::InboxCommit;
use crate::lfs::fetch::LfsObjects;
use crate::paths::RepoPath;
use crate::record::{Caller, Operation, OperationKind, PatchAuthor};
use crate::restore::{self, Applied, NotLive, Plan, RecordMode, RestoreError, Step};
use crate::telemetry::{ActiveSpan, Telemetry, spans};
use crate::volume::{Committer, OperationSpec, Recorder, StateDir};

/// Inputs per record when a partial write is recorded.
const PARTIAL_RECORD_CHUNK: usize = 4096;

/// What the queue runs against.
pub struct QueueContext<'a> {
    pub state: &'a StateDir,
    pub recorder: &'a Recorder,
    pub committer: &'a Committer,
    /// The backing tree, written directly: nothing serves it while the
    /// queue runs.
    pub tree: &'a Dir,
    /// Where LFS content comes from (the cache, else the remotes).
    pub objects: &'a dyn LfsObjects,
    pub telemetry: &'a Telemetry,
    /// The applying process: the operations' actor and operator.
    pub caller: Caller,
    /// How patches are merged onto the head (bead agentfs-mia.3 adds a
    /// choice).
    pub integrator: &'a dyn Integrator,
    /// Whether a cleanly merged patch may land (`agentfs.patchAccept`,
    /// [`super::accept::rule`]).
    pub rule: &'a dyn AcceptanceRule,
}

/// What one [`Queue::process`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueReport {
    /// Applied in this run: inbox number and branch commit, in order.
    pub applied: Vec<(u64, Oid)>,
    /// Entries the branch already applied, whose refs this run moved.
    pub already: Vec<u64>,
    /// (inbox number, the inbox number that applied the same patch commit
    /// first), this run: consumed with a marker commit, not applied again.
    pub duplicates: Vec<(u64, u64)>,
    pub blocked: Option<Blocked>,
    /// Patches still queued, the set-aside and blocked ones included.
    pub queue_depth: usize,
    pub ignored: Vec<Ignored>,
    /// The highest inbox number applied on the branch.
    pub last_applied: u64,
    /// The status commit written.
    pub status: Oid,
    /// Set aside in this run, with their verdicts, ascending.
    pub set_aside: Vec<(u64, Verdict)>,
    /// held/<n> marks deleted because the inbox entry was withdrawn.
    pub withdrawn: Vec<u64>,
}

/// How landing one patch went.
enum Landing {
    Landed(Oid),
    /// Not landed; `failed` after a write, else refused by the check
    /// before anything was written.
    Refused(Blocked),
}

/// How judging one patch went.
enum Judged {
    /// Landed as this branch commit.
    Landed(Oid),
    /// Set aside: the queue goes on without it.
    Aside(Aside),
    /// The run stops at it.
    Stop(Blocked),
}

/// A patch set aside, as later patches of the same run see it.
struct Aside {
    verdict: Verdict,
    /// The paths it changes, when its branch could be read.
    paths: Option<Vec<RepoPath>>,
    barrier: bool,
}

/// The patch queue of one open volume.
pub struct Queue<'a> {
    cx: QueueContext<'a>,
}

impl<'a> Queue<'a> {
    pub fn new(cx: QueueContext<'a>) -> Self {
        Self { cx }
    }

    /// Applies what is queued (see the module docs). `fetch` is how the
    /// fetch before it went, for the status document. Fails only when the
    /// history or the state area cannot be read; a patch that cannot be
    /// applied is set aside or blocks the queue instead.
    ///
    /// A failure is recorded as blocked (`failed`, inbox 0) in
    /// `meta/patch.json` and, if it can be, the status ref
    /// ([`record_failure`]), so the gauges and `agentfs status` show it.
    pub fn process(&mut self, fetch: Option<&FetchOutcome>) -> Result<QueueReport, PatchError> {
        self.process_inner(fetch).inspect_err(|e| {
            let message = format!("the patch queue could not run: {e}");
            record_failure(self.cx.state, Some(self.cx.committer), &message);
        })
    }

    fn process_inner(&mut self, fetch: Option<&FetchOutcome>) -> Result<QueueReport, PatchError> {
        let committer = self.cx.committer;
        committer.commit_pending()?;
        let history = committer.history();
        let repo = history.repo();
        let volume = history.settings().volume.clone();
        let state_path = self.cx.state.patch_state();
        let mut state = PatchState::load(&state_path);

        // The local queue.
        let prefix = inbox_prefix(&volume);
        let mut inbox: BTreeMap<u64, Oid> = BTreeMap::new();
        let mut ignored = Vec::new();
        for reference in repo.references_glob(&format!("{prefix}*"))? {
            let reference = reference?;
            let Ok(name) = reference.name().map(str::to_owned) else {
                continue;
            };
            let target = reference.resolve().ok().and_then(|r| r.target());
            match (seq_of_ref(&prefix, &name), target) {
                (Some(n), Some(oid)) => {
                    inbox.insert(n, oid);
                }
                (n, _) => ignored.push(Ignored {
                    ref_name: name,
                    inbox: n,
                    patch: target.map(|o| o.to_string()),
                    why: "malformed".into(),
                }),
            }
        }
        let marks = held_marks(repo, &volume)?;

        // What the branch applies already. Held marks and an interrupted
        // write are judged by the trailers of the whole branch.
        let head = history.head()?.map(|c| c.id());
        let cached = state
            .scanned_head
            .as_deref()
            .and_then(|s| Oid::from_str(s).ok())
            .filter(|cached| match head {
                Some(head) => {
                    *cached == head || repo.graph_descendant_of(head, *cached).unwrap_or(false)
                }
                None => false,
            })
            .filter(|_| marks.is_empty() && state.in_flight.is_none());
        let mut found = history.find_inbox_commits(cached)?;
        let mut full_scan = cached.is_none();
        let mut last_applied = found.keys().copied().max().unwrap_or(0);
        let mut last = found.get(&last_applied).copied();
        if !full_scan && state.last_applied > last_applied {
            last_applied = state.last_applied;
            last = cached_last(&state);
        }

        // Set aside before (bead agentfs-mia.2): a mark whose number the
        // branch applied is left over from a crash after landing; one
        // whose inbox entry is gone was withdrawn.
        let mut set_aside_before = std::collections::BTreeSet::new();
        let mut withdrawn = Vec::new();
        for (n, oid) in marks {
            if found.contains_key(&n) {
                drop_held(repo, &volume, n)?;
            } else if !inbox.contains_key(&n) {
                drop_held(repo, &volume, n)?;
                withdrawn.push(n);
                ignored.push(Ignored {
                    ref_name: inbox_ref(&volume, n),
                    inbox: Some(n),
                    patch: Some(oid.to_string()),
                    why: "withdrawn".into(),
                });
            } else {
                set_aside_before.insert(n);
            }
        }

        // Entries at or below it: finish their bookkeeping, or ignore them;
        // a set-aside one is judged again.
        let mut already = Vec::new();
        let below: Vec<(u64, Oid)> = inbox
            .range(..=last_applied)
            .filter(|(n, _)| !set_aside_before.contains(*n))
            .map(|(n, o)| (*n, *o))
            .collect();
        for (n, oid) in below {
            inbox.remove(&n);
            if !found.contains_key(&n) && !full_scan {
                found = history.find_inbox_commits(None)?;
                full_scan = true;
            }
            match found.get(&n) {
                Some(c) if c.patch == oid => {
                    move_refs(repo, &volume, n, oid)?;
                    already.push(n);
                }
                _ => ignored.push(Ignored {
                    ref_name: inbox_ref(&volume, n),
                    inbox: Some(n),
                    patch: Some(oid.to_string()),
                    why: "stale".into(),
                }),
            }
        }

        for i in &ignored {
            self.cx.recorder.events().emit(Event::PatchIgnored {
                ref_name: i.ref_name.clone(),
                inbox: i.inbox,
                patch: i.patch.as_deref().and_then(|p| Oid::from_str(p).ok()),
                why: i.why.clone(),
            });
        }

        // A write a crash interrupted, of a patch that has changed since.
        let in_flight = state.in_flight.take();
        let in_flight_patch = in_flight
            .as_ref()
            .and_then(|f| Oid::from_str(&f.patch).ok());
        if let Some(f) = &in_flight {
            self.recover_in_flight(f, &found, &inbox)?;
        }

        // The rest, in order. A patch commit already applied under an
        // earlier number is consumed, not applied again (design §12): the
        // whole history is scanned once to know them all.
        let mut applied = Vec::new();
        let mut duplicates = Vec::new();
        let mut blocked = None;
        let mut aside: BTreeMap<u64, Aside> = BTreeMap::new();
        let queued: Vec<(u64, Oid)> = inbox.iter().map(|(n, o)| (*n, *o)).collect();
        if !queued.is_empty() && !full_scan {
            found = history.find_inbox_commits(None)?;
        }
        let mut first_applied: std::collections::HashMap<Oid, u64> =
            std::collections::HashMap::new();
        for (n, c) in &found {
            first_applied.entry(c.patch).or_insert(*n);
        }
        // The highest number judged in this run, landed or set aside.
        let mut judged = 0;
        for (i, (n, patch)) in queued.iter().copied().enumerate() {
            let waiting = queued.len() - i;
            if n > last_applied {
                let next = last_applied.max(judged) + 1;
                if n != next {
                    let b = Blocked {
                        inbox: next,
                        patch: None,
                        reason: BlockReason::Gap,
                        message: format!("patch {next} is not queued, and {n} comes after it"),
                        paths: Vec::new(),
                    };
                    self.blocked_event(&b, waiting);
                    blocked = Some(b);
                    break;
                }
            }
            judged = judged.max(n);
            let landed = if let Some(&m) = first_applied.get(&patch)
                && m < n
            {
                let commit = self.consume_duplicate(n, patch, m)?;
                duplicates.push((n, m));
                self.cx.recorder.events().emit(Event::PatchDuplicate {
                    inbox: n,
                    patch,
                    applied_as: m,
                });
                commit
            } else {
                match self.judge_one(n, patch, &aside) {
                    Judged::Landed(commit) => {
                        first_applied.entry(patch).or_insert(n);
                        crate::failpoints::hit("patch.after_commit")?;
                        applied.push((n, commit));
                        self.cx.recorder.events().emit(Event::PatchApplied {
                            inbox: n,
                            patch,
                            commit,
                        });
                        commit
                    }
                    Judged::Aside(a) => {
                        // Marked before the next number is judged, so a
                        // crash never leaves it to read as stale.
                        repo.reference(
                            &held_ref(&volume, n),
                            patch,
                            true,
                            "agentfs: patch set aside",
                        )?;
                        self.aside_events(n, patch, &a.verdict);
                        aside.insert(n, a);
                        continue;
                    }
                    Judged::Stop(b) => {
                        self.blocked_event(&b, waiting);
                        blocked = Some(b);
                        break;
                    }
                }
            };
            move_refs(repo, &volume, n, patch)?;
            drop_held(repo, &volume, n)?;
            inbox.remove(&n);
            if n > last_applied {
                last_applied = n;
                last = Some(InboxCommit {
                    commit: landed,
                    patch,
                });
                state.last_applied = n;
                state.last_applied_patch = Some(patch.to_string());
                state.last_applied_commit = Some(landed.to_string());
            }
            state.scanned_head = Some(landed.to_string());
            // A cache: a lost update costs a rescan, nothing more.
            let _ = state.save(&state_path);
        }

        // Status.
        let head = history.head()?.map(|c| c.id());
        let doc = StatusDoc {
            version: STATUS_VERSION,
            volume: volume.clone(),
            branch: history.settings().branch_ref.clone(),
            head: head.map(|h| h.to_string()),
            updated: crate::push::utc_basic(now_ms() / 1000),
            last_applied: last.filter(|_| last_applied > 0).map(|c| AppliedEntry {
                inbox: last_applied,
                patch: c.patch.to_string(),
                commit: c.commit.to_string(),
            }),
            applied_now: applied.iter().map(|(n, _)| *n).collect(),
            duplicate_now: duplicates
                .iter()
                .map(|(n, m)| DuplicateEntry {
                    inbox: *n,
                    patch: queued
                        .iter()
                        .find(|(q, _)| q == n)
                        .map(|(_, o)| o.to_string())
                        .unwrap_or_default(),
                    applied_as: *m,
                })
                .collect(),
            queue: inbox
                .iter()
                .map(|(n, o)| QueueEntry {
                    inbox: *n,
                    patch: o.to_string(),
                    verdict: aside.get(n).map(|a| a.verdict.clone()),
                })
                .collect(),
            blocked: blocked.clone(),
            ignored: ignored.clone(),
            fetch: fetch.map(|f| FetchStatus {
                remote: f.remote.clone(),
                ok: f.ok,
                error: f.message.clone(),
            }),
        };
        let signature = committer.committer_signature()?;
        let status = write_status_ref(repo, &volume, &doc, &signature)?;
        crate::failpoints::hit("queue.after_status")?;
        state.version = STATUS_VERSION;
        state.last_applied = last_applied;
        state.last_applied_patch = last.map(|c| c.patch.to_string());
        state.last_applied_commit = last.map(|c| c.commit.to_string());
        state.scanned_head = head.map(|h| h.to_string());
        state.queue_depth = inbox.len() as u64;
        state.held = aside.len() as u64;
        state.blocked = blocked.clone();
        state.status_oid = Some(status.to_string());
        state.updated_ms = now_ms();
        // An interrupted write of a patch still queued and not landed in
        // this run stays noted, so a later run can still put it back.
        state.in_flight = in_flight.filter(|f| {
            in_flight_patch.is_some() && inbox.get(&f.inbox).copied() == in_flight_patch
        });
        state.save(&state_path)?;
        Ok(QueueReport {
            applied,
            already,
            duplicates,
            blocked,
            queue_depth: inbox.len(),
            ignored,
            last_applied,
            status,
            set_aside: aside.into_iter().map(|(n, a)| (n, a.verdict)).collect(),
            withdrawn,
        })
    }

    /// The alarms of a patch set aside: `PatchHeld` for held and not
    /// judged, and `PatchRuleFailed` for not judged. Waiting and awaiting
    /// raise none (the status carries them).
    fn aside_events(&self, n: u64, patch: Oid, verdict: &Verdict) {
        let events = self.cx.recorder.events();
        match verdict {
            Verdict::Held {
                reason, message, ..
            } => events.emit(Event::PatchHeld {
                inbox: n,
                patch: Some(patch),
                verdict: verdict.kind(),
                reason: reason.as_str(),
                message: message.clone(),
            }),
            Verdict::NotJudged { message, .. } => {
                let rule = self.cx.rule.name();
                events.emit(Event::PatchHeld {
                    inbox: n,
                    patch: Some(patch),
                    verdict: verdict.kind(),
                    reason: rule,
                    message: message.clone(),
                });
                events.emit(Event::PatchRuleFailed {
                    inbox: n,
                    rule,
                    message: message.clone(),
                });
            }
            Verdict::Waiting { .. } | Verdict::Awaiting { .. } => {}
        }
    }

    /// Consumes patch `n`, the same patch commit that `m` applied first
    /// (design §12): a marker commit with the head's tree carrying
    /// `Agentfs-Inbox: <n>`, so the branch's trailers record it as done and
    /// every crash point converges. Nothing is merged or written.
    fn consume_duplicate(&self, n: u64, patch_oid: Oid, m: u64) -> Result<Oid, PatchError> {
        let repo = self.cx.committer.history().repo();
        let summary = match repo.find_commit(patch_oid) {
            Ok(patch) => summary_of(&patch, n, &[], repo),
            Err(_) => format!("patch {n}"),
        };
        let op = Operation {
            id: 0,
            kind: OperationKind::Patch,
            source: patch_oid.to_string(),
            summary: format!("duplicate of patch {m}: {summary}"),
            caller: self.cx.caller,
            inbox: Some(n),
            author: repo.find_commit(patch_oid).ok().map(|p| author_of(&p)),
        };
        Ok(self.cx.committer.commit_patch_marker(&op)?)
    }

    fn blocked_event(&self, b: &Blocked, queued: usize) {
        self.cx.recorder.events().emit(Event::PatchBlocked {
            inbox: b.inbox,
            patch: b.patch.as_deref().and_then(|p| Oid::from_str(p).ok()),
            reason: b.reason.as_str(),
            message: b.message.clone(),
            queued,
        });
    }

    /// Judges patch `n` and lands it when it may, in an `agentfs.patch`
    /// span. `aside` are the patches this run set aside so far.
    fn judge_one(&self, n: u64, patch: Oid, aside: &BTreeMap<u64, Aside>) -> Judged {
        let span = self.cx.telemetry.span(spans::PATCH, || {
            vec![
                KeyValue::new("agentfs.patch.inbox", n as i64),
                KeyValue::new("agentfs.patch.oid", patch.to_string()),
            ]
        });
        let judged = match self.judge_inner(n, patch, aside, &span) {
            Ok(judged) => judged,
            Err(e) => Judged::Stop(blocked(
                n,
                patch,
                BlockReason::Failed,
                e.to_string(),
                Vec::new(),
            )),
        };
        match &judged {
            Judged::Landed(commit) => {
                span.set(KeyValue::new("agentfs.patch.outcome", "applied"));
                span.set(KeyValue::new("agentfs.patch.commit", commit.to_string()));
            }
            Judged::Aside(a) => {
                span.set(KeyValue::new("agentfs.patch.outcome", a.verdict.kind()));
                if let Verdict::Held { reason, .. } = &a.verdict {
                    span.set(KeyValue::new("agentfs.patch.reason", reason.as_str()));
                }
            }
            Judged::Stop(b) => {
                span.set(KeyValue::new("agentfs.patch.outcome", "blocked"));
                span.set(KeyValue::new("agentfs.patch.reason", b.reason.as_str()));
                span.error(&b.message);
            }
        }
        span.end();
        judged
    }

    /// In order (bead agentfs-mia.2): read the branch; wait behind an
    /// earlier set-aside barrier, or an earlier set-aside patch changing
    /// the same paths; merge; ask the acceptance rule; land.
    fn judge_inner(
        &self,
        n: u64,
        patch_oid: Oid,
        aside: &BTreeMap<u64, Aside>,
        span: &ActiveSpan,
    ) -> Result<Judged, PatchError> {
        let history = self.cx.committer.history();
        let repo = history.repo();
        let tip_barrier = || {
            repo.find_commit(patch_oid)
                .is_ok_and(|c| super::integrate::is_barrier(&c))
        };
        let Some(head) = history.head()? else {
            let refusal = Refusal {
                reason: BlockReason::ParentNotInHistory,
                message: "the volume's branch does not exist yet".into(),
                paths: Vec::new(),
            };
            return Ok(refused(n, patch_oid, refusal, None, tip_barrier()));
        };
        let branch = match PatchBranch::read(repo, head.id(), patch_oid) {
            Ok(branch) => branch,
            Err(refusal) => return Ok(refused(n, patch_oid, refusal, None, tip_barrier())),
        };
        let earlier = || aside.iter().filter(|(k, _)| **k < n);
        let wait = |why, behind: Vec<u64>| {
            Judged::Aside(Aside {
                verdict: Verdict::Waiting { why, behind },
                paths: Some(branch.paths.clone()),
                barrier: branch.barrier,
            })
        };
        let barriers: Vec<u64> = earlier()
            .filter(|(_, a)| a.barrier)
            .map(|(k, _)| *k)
            .collect();
        if !barriers.is_empty() {
            return Ok(wait(WaitReason::Barrier, barriers));
        }
        let same: Vec<u64> = earlier()
            .filter(|(_, a)| {
                a.paths
                    .as_ref()
                    .is_some_and(|paths| paths.iter().any(|p| branch.paths.contains(p)))
            })
            .map(|(k, _)| *k)
            .collect();
        if !same.is_empty() {
            return Ok(wait(WaitReason::SameFiles, same));
        }
        let merged = match self.integrate(head.id(), &branch)? {
            Ok(merged) => merged,
            Err(refusal) => {
                return Ok(refused(
                    n,
                    patch_oid,
                    refusal,
                    Some(branch.paths.clone()),
                    branch.barrier,
                ));
            }
        };
        let candidate = Candidate {
            volume: &history.settings().volume,
            inbox: n,
            patch: &branch,
            head: head.id(),
            merged,
        };
        let set_aside = |verdict| {
            Ok(Judged::Aside(Aside {
                verdict,
                paths: Some(branch.paths.clone()),
                barrier: branch.barrier,
            }))
        };
        match self.cx.rule.judge(repo, &candidate) {
            Ok(Acceptance::Accept) => {}
            Ok(Acceptance::Refuse { message }) => {
                return set_aside(Verdict::Held {
                    reason: BlockReason::Refused,
                    message,
                    paths: Vec::new(),
                });
            }
            Ok(Acceptance::Await { what }) => return set_aside(Verdict::Awaiting { what }),
            Err(e) => {
                return set_aside(Verdict::NotJudged {
                    rule: self.cx.rule.name().to_owned(),
                    message: e.to_string(),
                });
            }
        }
        Ok(
            match self.land(n, patch_oid, head.id(), &branch, merged, span)? {
                Landing::Landed(commit) => Judged::Landed(commit),
                // Refused before anything was written (the check): held whole
                // when the reason says so.
                Landing::Refused(b) => refused(
                    n,
                    patch_oid,
                    Refusal {
                        reason: b.reason,
                        message: b.message,
                        paths: b.paths,
                    },
                    Some(branch.paths.clone()),
                    branch.barrier,
                ),
            },
        )
    }

    /// Writes the merged tree of patch `n` into the backing tree and
    /// records it as one atomic operation (see the module docs).
    fn land(
        &self,
        n: u64,
        patch_oid: Oid,
        head: Oid,
        branch: &PatchBranch,
        merged: Oid,
        span: &ActiveSpan,
    ) -> Result<Landing, PatchError> {
        let QueueContext {
            recorder,
            committer,
            tree,
            objects,
            caller,
            ..
        } = &self.cx;
        let history = committer.history();
        let repo = history.repo();
        let block = |reason, message: String, paths: Vec<String>| {
            Ok(Landing::Refused(blocked(
                n, patch_oid, reason, message, paths,
            )))
        };
        span.set(KeyValue::new(
            "agentfs.patch.commits",
            branch.commits.len() as i64,
        ));
        let patch = repo.find_commit(patch_oid)?;
        let spec = OperationSpec {
            kind: OperationKind::Patch,
            source: patch_oid.to_string(),
            summary: summary_of(&patch, n, &branch.commits, repo),
            caller: *caller,
            inbox: Some(n),
            author: Some(author_of(&patch)),
        };
        let plan = restore::plan_tree(repo, head, patch_oid, merged, spec.summary.clone())?;
        if plan.steps.is_empty() {
            // Nothing the tree shows changes: a marker commit (design §3.4).
            let op = Operation {
                id: 0,
                kind: spec.kind,
                source: spec.source,
                summary: spec.summary,
                caller: spec.caller,
                inbox: spec.inbox,
                author: spec.author,
            };
            return Ok(Landing::Landed(committer.commit_patch_marker(&op)?));
        }
        let plan = match restore::check(plan, repo, *recorder, *objects, tree, &NotLive, false) {
            Ok(plan) => plan,
            Err(e) => {
                let reason = match &e {
                    RestoreError::Unrecorded { .. }
                    | RestoreError::InTheWay { .. }
                    | RestoreError::HistoryBehind => BlockReason::Unrecorded,
                    RestoreError::Unrecordable { .. } | RestoreError::InvalidPath { .. } => {
                        BlockReason::Unsupported
                    }
                    _ => BlockReason::Failed,
                };
                return block(reason, e.to_string(), error_paths(&e));
            }
        };
        // Noted before the write, for a crash before the record (see the
        // module docs); the run's own save clears it.
        let state_path = self.cx.state.patch_state();
        let mut noted = PatchState::load(&state_path);
        noted.in_flight = Some(InFlight {
            inbox: n,
            patch: patch_oid.to_string(),
            head: head.to_string(),
            paths: plan.steps.iter().map(|s| s.path.to_string()).collect(),
        });
        noted.save(&state_path)?;
        let guard = recorder.begin_operation_spec(spec.clone(), &[caller.pid])?;
        let result = restore::apply(&plan, repo, *objects, tree, &NotLive, false);
        match result {
            Ok(applied) => {
                crate::failpoints::hit("patch.after_tree_write")?;
                let recorded =
                    restore::record_applied(recorder, tree, &applied, *caller, RecordMode::Atomic);
                guard.finish();
                crate::failpoints::hit("patch.after_record")?;
                let committed = committer.commit_pending();
                let head_now = history.head()?;
                let done = head_now.as_ref().is_some_and(|c| {
                    c.id() != head
                        && inbox_from_message(c.message_raw().unwrap_or_default())
                            == Some((n, patch_oid))
                });
                if let (true, Some(c)) = (done, head_now) {
                    return Ok(Landing::Landed(c.id()));
                }
                committed?;
                // Refused at record or commit time: nothing of it is in
                // history, so it goes back out of the tree.
                let why = match recorded {
                    Err(problems) => problems,
                    Ok(()) => "the commit dropped the patch's atomic record (a PathRefused \
                               alarm at stage commit says why)"
                        .to_owned(),
                };
                let back = self.write_back(&plan, &applied.written, &applied.removed);
                let message = match back {
                    Ok(()) => format!("{why}; the tree was put back as it was"),
                    Err(e) => format!(
                        "{why}; putting the tree back failed too ({e}): it holds part of the \
                         patch that history does not"
                    ),
                };
                block(BlockReason::Failed, message, Vec::new())
            }
            Err(e) => {
                // Record what the failure left, as it stands, so the tree
                // matches history; the next merge picks up from there.
                let mut left = e.applied.clone();
                if let Some(path) = e.left_changed(&plan, repo, tree) {
                    left.written.push(path);
                }
                guard.finish();
                let mut message = format!("writing the patch failed part way: {e}");
                if !(left.written.is_empty() && left.removed.is_empty()) {
                    // Its own operation, without an inbox number: the
                    // commit is not the patch's.
                    let partial = OperationSpec {
                        summary: format!("part of patch {n} (failed part way)"),
                        inbox: None,
                        ..spec
                    };
                    let guard = recorder.begin_operation_spec(partial, &[caller.pid])?;
                    let recorded = restore::record_applied(
                        recorder,
                        tree,
                        &left,
                        *caller,
                        RecordMode::Chunked(PARTIAL_RECORD_CHUNK),
                    );
                    guard.finish();
                    if let Err(problems) = recorded {
                        message.push_str(&format!("; recording what was written: {problems}"));
                    }
                    committer.commit_pending()?;
                }
                block(BlockReason::Failed, message, vec![e.path.clone()])
            }
        }
    }

    /// After a crash between writing patch `f.inbox` into the tree and
    /// recording it: if the branch has not applied that number and the
    /// inbox no longer holds that patch (replaced, skipped, withdrawn), the
    /// paths it wrote go back to the branch head's state, or, when that
    /// cannot be done, are recorded as they stand without an inbox number.
    /// When the same patch is still queued, the run applies it as usual.
    fn recover_in_flight(
        &self,
        f: &InFlight,
        found: &BTreeMap<u64, InboxCommit>,
        inbox: &BTreeMap<u64, Oid>,
    ) -> Result<(), PatchError> {
        let patch = Oid::from_str(&f.patch).ok();
        // Applied is what the branch's trailers say, never the high-water
        // mark: a set-aside number below it is not applied (bead
        // agentfs-mia.2).
        if found.contains_key(&f.inbox)
            || (patch.is_some() && inbox.get(&f.inbox) == patch.as_ref())
        {
            return Ok(());
        }
        if self.put_back(f, patch).is_ok() {
            return Ok(());
        }
        let QueueContext {
            recorder,
            committer,
            tree,
            caller,
            ..
        } = &self.cx;
        let spec = OperationSpec::new(
            OperationKind::Patch,
            f.patch.clone(),
            format!(
                "part of patch {} (interrupted, then replaced or withdrawn)",
                f.inbox
            ),
            *caller,
        );
        let written: Vec<RepoPath> = f
            .paths
            .iter()
            .filter_map(|p| RepoPath::new(p.clone()).ok())
            .collect();
        let guard = recorder.begin_operation_spec(spec, &[caller.pid])?;
        // What cannot be recorded is left for the check, which blocks on
        // it as `unrecorded`.
        let _ = restore::record_applied(
            recorder,
            tree,
            &Applied {
                written,
                removed: Vec::new(),
            },
            *caller,
            RecordMode::Chunked(PARTIAL_RECORD_CHUNK),
        );
        guard.finish();
        committer.commit_pending()?;
        Ok(())
    }

    /// Writes the paths `f`'s patch changes back to the branch head's
    /// state, if the head is still the one it was merged onto.
    fn put_back(&self, f: &InFlight, patch: Option<Oid>) -> Result<(), String> {
        let history = self.cx.committer.history();
        let repo = history.repo();
        let head = history
            .head()
            .map_err(|e| e.to_string())?
            .ok_or("no branch head")?;
        if head.id().to_string() != f.head {
            return Err("the branch head moved".into());
        }
        let patch = patch.ok_or("not a commit id")?;
        let merged = match self.merge(head.id(), patch).map_err(|e| e.to_string())? {
            Ok((_, merged)) => merged,
            Err(refusal) => return Err(refusal.message),
        };
        let plan = restore::plan_tree(repo, head.id(), patch, merged, String::new())
            .map_err(|e| e.to_string())?;
        let paths: Vec<RepoPath> = plan.steps.iter().map(|s| s.path.clone()).collect();
        self.write_back(&plan, &paths, &paths)
    }

    /// Reads patch `tip` as a branch against `head` and merges it there:
    /// the branch and the merged tree, or why it does not land.
    fn merge(
        &self,
        head: Oid,
        tip: Oid,
    ) -> Result<Result<(PatchBranch, Oid), Refusal>, PatchError> {
        let repo = self.cx.committer.history().repo();
        let branch = match PatchBranch::read(repo, head, tip) {
            Ok(branch) => branch,
            Err(refusal) => return Ok(Err(refusal)),
        };
        Ok(self.integrate(head, &branch)?.map(|tree| (branch, tree)))
    }

    /// Merges `branch` onto `head`: the merged tree, whose objects this
    /// repository then sees, or why it does not land (a conflict, a merge
    /// that could not run).
    fn integrate(
        &self,
        head: Oid,
        branch: &PatchBranch,
    ) -> Result<Result<Oid, Refusal>, PatchError> {
        let repo = self.cx.committer.history().repo();
        let tree = match self.cx.integrator.integrate(head, branch) {
            Ok(Integrated::Clean { tree }) => tree,
            Ok(Integrated::Conflict { paths }) => {
                return Ok(Err(Refusal {
                    reason: BlockReason::Conflict,
                    message: format!(
                        "merging onto the branch head {head} conflicts in {}",
                        paths.join(", ")
                    ),
                    paths,
                }));
            }
            Err(e) => {
                return Ok(Err(Refusal {
                    reason: BlockReason::Failed,
                    message: e.to_string(),
                    paths: Vec::new(),
                }));
            }
        };
        // The objects git wrote, for this repository's object database.
        repo.odb()?.refresh()?;
        Ok(Ok(tree))
    }

    /// Puts the paths of `plan` that were written or removed back to the
    /// branch head's state.
    fn write_back(
        &self,
        plan: &Plan,
        written: &[RepoPath],
        removed: &[RepoPath],
    ) -> Result<(), String> {
        let steps: Vec<Step> = plan
            .steps
            .iter()
            .filter(|s| written.contains(&s.path) || removed.contains(&s.path))
            .map(|s| Step {
                path: s.path.clone(),
                current: s.target,
                target: s.current,
            })
            .collect();
        let back = Plan {
            steps,
            changed_since: Vec::new(),
            prune_floor: String::new(),
            target_tree: None,
            ..plan.clone()
        };
        restore::apply(
            &back,
            self.cx.committer.history().repo(),
            self.cx.objects,
            self.cx.tree,
            &NotLive,
            true,
        )
        .map(|_| ())
        .map_err(|e| e.to_string())
    }
}

/// A refusal of patch `n`: set aside, held whole, when its reason says so
/// ([`BlockReason::holds_whole`]), else where the run stops. `paths` are
/// the paths it changes, when its branch could be read.
fn refused(
    n: u64,
    patch: Oid,
    refusal: Refusal,
    paths: Option<Vec<RepoPath>>,
    barrier: bool,
) -> Judged {
    if refusal.reason.holds_whole() {
        Judged::Aside(Aside {
            verdict: Verdict::Held {
                reason: refusal.reason,
                message: refusal.message,
                paths: refusal.paths,
            },
            paths,
            barrier,
        })
    } else {
        Judged::Stop(blocked(
            n,
            patch,
            refusal.reason,
            refusal.message,
            refusal.paths,
        ))
    }
}

/// The `held/<n>` marks of `volume` (malformed names are not marks).
fn held_marks(repo: &Repository, volume: &str) -> Result<BTreeMap<u64, Oid>, git2::Error> {
    let prefix = held_prefix(volume);
    let mut marks = BTreeMap::new();
    for reference in repo.references_glob(&format!("{prefix}*"))? {
        let reference = reference?;
        if let (Some(n), Some(oid)) = (
            reference
                .name()
                .ok()
                .and_then(|name| seq_of_ref(&prefix, name)),
            reference.target(),
        ) {
            marks.insert(n, oid);
        }
    }
    Ok(marks)
}

/// Deletes `held/<n>`, if there is one.
fn drop_held(repo: &Repository, volume: &str, n: u64) -> Result<(), git2::Error> {
    match repo.find_reference(&held_ref(volume, n)) {
        Ok(mut mark) => mark.delete(),
        Err(e) if e.code() == git2::ErrorCode::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

fn blocked(
    n: u64,
    patch: Oid,
    reason: BlockReason,
    message: String,
    paths: Vec<String>,
) -> Blocked {
    Blocked {
        inbox: n,
        patch: Some(patch.to_string()),
        reason,
        message,
        paths,
    }
}

/// The paths a refusal names.
fn error_paths(e: &RestoreError) -> Vec<String> {
    match e {
        RestoreError::Unrecordable { path, .. }
        | RestoreError::Unrecorded { path }
        | RestoreError::InTheWay { path, .. }
        | RestoreError::LfsObject { path, .. }
        | RestoreError::InvalidPath { path, .. } => vec![path.clone()],
        RestoreError::ChangedSince { paths } | RestoreError::OpenForWriting { paths } => {
            paths.clone()
        }
        _ => Vec::new(),
    }
}

/// The commit message of a patch's commit: the tip's own, without a final
/// trailer paragraph (agentfs adds its own trailers); for a patch of more
/// than one commit (`commits`, oldest first), followed by a blank line and
/// one line per commit, `<12-hex short oid> <subject>`.
pub fn summary_of(patch: &git2::Commit<'_>, n: u64, commits: &[Oid], repo: &Repository) -> String {
    let raw = String::from_utf8_lossy(patch.message_raw_bytes()).into_owned();
    let mut paragraphs: Vec<&str> = raw
        .trim_end()
        .split("\n\n")
        .filter(|p| !p.trim().is_empty())
        .collect();
    if paragraphs.len() > 1
        && let Some(last) = paragraphs.last()
        && last.lines().all(is_trailer_line)
    {
        paragraphs.pop();
    }
    let message = paragraphs.join("\n\n").trim().to_owned();
    let mut message = if message.is_empty() {
        format!("patch {n}")
    } else {
        message
    };
    if commits.len() > 1 {
        message.push('\n');
        for oid in commits {
            let subject = repo
                .find_commit(*oid)
                .ok()
                .and_then(|c| {
                    c.summary_bytes()
                        .map(|s| String::from_utf8_lossy(s).into_owned())
                })
                .unwrap_or_default();
            let short = oid.to_string();
            message.push_str(&format!("\n{} {subject}", &short[..12]));
        }
    }
    message
}

/// `Token: value`, as git reads trailers.
fn is_trailer_line(line: &str) -> bool {
    match line.split_once(": ") {
        Some((key, _)) => {
            !key.is_empty() && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        }
        None => false,
    }
}

/// The patch commit's author.
pub fn author_of(patch: &git2::Commit<'_>) -> PatchAuthor {
    let author = patch.author();
    PatchAuthor {
        name: String::from_utf8_lossy(author.name_bytes()).into_owned(),
        email: String::from_utf8_lossy(author.email_bytes()).into_owned(),
        time_s: author.when().seconds(),
        offset_min: author.when().offset_minutes(),
    }
}

/// `applied/<n>` = `patch`, then `inbox/<n>` deleted if it still names
/// `patch`.
fn move_refs(repo: &Repository, volume: &str, n: u64, patch: Oid) -> Result<(), git2::Error> {
    repo.reference(
        &applied_ref(volume, n),
        patch,
        true,
        "agentfs: patch applied",
    )?;
    if let Ok(mut inbox) = repo.find_reference(&inbox_ref(volume, n))
        && inbox.target() == Some(patch)
    {
        inbox.delete()?;
    }
    Ok(())
}

/// The last applied patch as `meta/patch.json` caches it.
fn cached_last(state: &PatchState) -> Option<InboxCommit> {
    Some(InboxCommit {
        commit: Oid::from_str(state.last_applied_commit.as_deref()?).ok()?,
        patch: Oid::from_str(state.last_applied_patch.as_deref()?).ok()?,
    })
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// Records that the patch queue could not run at all (design §12): blocked
/// with reason `failed` at inbox 0 in `meta/patch.json`, so the
/// `agentfs.patch.blocked` gauge and `agentfs status` show it, and, when
/// `committer` is given and the repository can be written, in a new status
/// commit that the queue push takes to the remote. Best effort: it runs
/// on a failure path, and the alarm is the caller's.
pub fn record_failure(state: &StateDir, committer: Option<&Committer>, message: &str) {
    let blocked = Blocked {
        inbox: 0,
        patch: None,
        reason: BlockReason::Failed,
        message: message.to_owned(),
        paths: Vec::new(),
    };
    let path = state.patch_state();
    let mut cache = PatchState::load(&path);
    cache.version = STATUS_VERSION;
    cache.blocked = Some(blocked.clone());
    cache.updated_ms = now_ms();
    if let Some(committer) = committer {
        let history = committer.history();
        let repo = history.repo();
        let volume = history.settings().volume.clone();
        let previous = repo
            .refname_to_id(&super::status_ref(&volume))
            .ok()
            .and_then(|oid| super::status::read_status(repo, oid).ok());
        let head = history.head().ok().flatten().map(|c| c.id().to_string());
        let doc = StatusDoc {
            version: STATUS_VERSION,
            volume: volume.clone(),
            branch: history.settings().branch_ref.clone(),
            head,
            updated: crate::push::utc_basic(now_ms() / 1000),
            last_applied: previous.as_ref().and_then(|p| p.last_applied.clone()),
            applied_now: Vec::new(),
            duplicate_now: Vec::new(),
            queue: previous.map(|p| p.queue).unwrap_or_default(),
            blocked: Some(blocked),
            ignored: Vec::new(),
            fetch: None,
        };
        if let Ok(signature) = committer.committer_signature()
            && let Ok(oid) = write_status_ref(repo, &volume, &doc, &signature)
        {
            cache.status_oid = Some(oid.to_string());
        }
    }
    let _ = cache.save(&path);
}
