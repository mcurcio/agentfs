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
//!    entries at or below the last applied number as ignored;
//! 4. applies the rest in order, the next number being the last applied
//!    plus one (a missing number blocks with `gap`). Each patch must have
//!    one parent on the branch, change at most [`MAX_PATCH_PATHS`] paths
//!    and no gitlinks; it is merged onto the head with libgit2's
//!    cherry-pick (a three-way merge against its parent), written into the
//!    backing tree with the restore engine ([`crate::restore`]), and
//!    recorded as one atomic record of a patch operation, so it becomes one
//!    commit carrying `Agentfs-Inbox: <n>`. A patch that changes nothing
//!    gets a marker commit with the head's tree;
//! 5. moves the refs (`applied/<n>` = the patch, `inbox/<n>` deleted) and
//!    stops at the first patch it cannot apply;
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

use git2::{FileMode, Oid, Repository};
use opentelemetry::KeyValue;

use super::fetch::FetchOutcome;
use super::status::{
    AppliedEntry, FetchStatus, InFlight, PatchState, QueueEntry, STATUS_VERSION, StatusDoc,
    write_status_ref,
};
use super::{
    BlockReason, Blocked, Ignored, MAX_PATCH_PATHS, PatchError, applied_ref, inbox_from_message,
    inbox_prefix, inbox_ref, seq_of_ref,
};
use crate::confined::Dir;
use crate::events::Event;
use crate::history::InboxCommit;
use crate::lfs::fetch::LfsObjects;
use crate::paths::RepoPath;
use crate::record::{Caller, Operation, OperationKind, PatchAuthor};
use crate::restore::{self, Applied, NotLive, Plan, RecordMode, RestoreError, Step};
use crate::telemetry::{Telemetry, spans};
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
}

/// What one [`Queue::process`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueReport {
    /// Applied in this run: inbox number and branch commit, in order.
    pub applied: Vec<(u64, Oid)>,
    /// Entries the branch already applied, whose refs this run moved.
    pub already: Vec<u64>,
    pub blocked: Option<Blocked>,
    /// Patches still queued, the blocked one included.
    pub queue_depth: usize,
    pub ignored: Vec<Ignored>,
    /// The highest inbox number applied on the branch.
    pub last_applied: u64,
    /// The status commit written.
    pub status: Oid,
}

/// How one patch went.
enum Step1 {
    Applied(Oid),
    Blocked(Blocked),
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
    /// applied blocks the queue instead.
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

        // What the branch applies already.
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
            });
        let mut found = history.find_inbox_commits(cached)?;
        let mut full_scan = cached.is_none();
        let mut last_applied = found.keys().copied().max().unwrap_or(0);
        let mut last = found.get(&last_applied).copied();
        if cached.is_some() && state.last_applied > last_applied {
            last_applied = state.last_applied;
            last = cached_last(&state);
        }

        // Entries at or below it: finish their bookkeeping, or ignore them.
        let mut already = Vec::new();
        let below: Vec<(u64, Oid)> = inbox
            .range(..=last_applied)
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
        if let Some(f) = state.in_flight.take() {
            self.recover_in_flight(&f, last_applied, &inbox)?;
        }

        // The rest, in order.
        let mut applied = Vec::new();
        let mut blocked = None;
        let queued: Vec<(u64, Oid)> = inbox.iter().map(|(n, o)| (*n, *o)).collect();
        for (i, (n, patch)) in queued.iter().copied().enumerate() {
            let waiting = queued.len() - i;
            let next = last_applied + 1;
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
            match self.apply_one(n, patch) {
                Step1::Applied(commit) => {
                    crate::failpoints::hit("patch.after_commit")?;
                    move_refs(repo, &volume, n, patch)?;
                    inbox.remove(&n);
                    last_applied = n;
                    last = Some(InboxCommit { commit, patch });
                    applied.push((n, commit));
                    state.last_applied = n;
                    state.last_applied_patch = Some(patch.to_string());
                    state.last_applied_commit = Some(commit.to_string());
                    state.scanned_head = Some(commit.to_string());
                    // A cache: a lost update costs a rescan, nothing more.
                    let _ = state.save(&state_path);
                    self.cx.recorder.events().emit(Event::PatchApplied {
                        inbox: n,
                        patch,
                        commit,
                    });
                }
                Step1::Blocked(b) => {
                    self.blocked_event(&b, waiting);
                    blocked = Some(b);
                    break;
                }
            }
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
            queue: inbox
                .iter()
                .map(|(n, o)| QueueEntry {
                    inbox: *n,
                    patch: o.to_string(),
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
        state.blocked = blocked.clone();
        state.status_oid = Some(status.to_string());
        state.updated_ms = now_ms();
        state.save(&state_path)?;
        Ok(QueueReport {
            applied,
            already,
            blocked,
            queue_depth: inbox.len(),
            ignored,
            last_applied,
            status,
        })
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

    /// Applies patch `n` in an `agentfs.patch` span.
    fn apply_one(&self, n: u64, patch: Oid) -> Step1 {
        let span = self.cx.telemetry.span(spans::PATCH, || {
            vec![
                KeyValue::new("agentfs.patch.inbox", n as i64),
                KeyValue::new("agentfs.patch.oid", patch.to_string()),
            ]
        });
        let step = match self.apply_inner(n, patch) {
            Ok(step) => step,
            Err(e) => Step1::Blocked(blocked(
                n,
                patch,
                BlockReason::Failed,
                e.to_string(),
                Vec::new(),
            )),
        };
        match &step {
            Step1::Applied(commit) => {
                span.set(KeyValue::new("agentfs.patch.outcome", "applied"));
                span.set(KeyValue::new("agentfs.patch.commit", commit.to_string()));
            }
            Step1::Blocked(b) => {
                span.set(KeyValue::new("agentfs.patch.outcome", "blocked"));
                span.set(KeyValue::new("agentfs.patch.reason", b.reason.as_str()));
                span.error(&b.message);
            }
        }
        span.end();
        step
    }

    fn apply_inner(&self, n: u64, patch_oid: Oid) -> Result<Step1, PatchError> {
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
            Ok(Step1::Blocked(blocked(
                n, patch_oid, reason, message, paths,
            )))
        };
        let Ok(patch) = repo.find_commit(patch_oid) else {
            return block(
                BlockReason::Unsupported,
                format!("{patch_oid} is not a commit"),
                Vec::new(),
            );
        };
        match patch.parent_count() {
            1 => {}
            0 => {
                return block(
                    BlockReason::Unsupported,
                    "the patch is a root commit: it needs the branch commit it was written \
                     against as its parent"
                        .into(),
                    Vec::new(),
                );
            }
            parents => {
                return block(
                    BlockReason::MergeCommit,
                    format!("the patch has {parents} parents; it must have one"),
                    Vec::new(),
                );
            }
        }
        let parent = patch.parent_id(0)?;
        let Some(head) = history.head()? else {
            return block(
                BlockReason::ParentNotInHistory,
                "the volume's branch does not exist yet".into(),
                Vec::new(),
            );
        };
        let on_branch = repo.odb()?.exists(parent)
            && (parent == head.id() || repo.graph_descendant_of(head.id(), parent)?);
        if !on_branch {
            return block(
                BlockReason::ParentNotInHistory,
                format!(
                    "the patch's parent {parent} is not the branch head {} or one of its \
                     ancestors",
                    head.id()
                ),
                Vec::new(),
            );
        }
        if let Some(step) = validate_diff(repo, &patch, parent)? {
            return Ok(Step1::Blocked(blocked(
                n, patch_oid, step.0, step.1, step.2,
            )));
        }
        let mut index = repo.cherrypick_commit(&patch, &head, 0, None)?;
        if index.has_conflicts() {
            let mut paths = Vec::new();
            for conflict in index.conflicts()? {
                let conflict = conflict?;
                let entry = conflict.our.or(conflict.their).or(conflict.ancestor);
                if let Some(entry) = entry {
                    let path = String::from_utf8_lossy(&entry.path).into_owned();
                    if !paths.contains(&path) {
                        paths.push(path);
                    }
                }
            }
            return block(
                BlockReason::Conflict,
                format!(
                    "merging onto the branch head {} conflicts in {}",
                    head.id(),
                    paths.join(", ")
                ),
                paths,
            );
        }
        let merged = index.write_tree_to(repo)?;
        let spec = OperationSpec {
            kind: OperationKind::Patch,
            source: patch_oid.to_string(),
            summary: summary_of(&patch, n),
            caller: *caller,
            inbox: Some(n),
            author: Some(author_of(&patch)),
        };
        let plan = restore::plan_tree(repo, head.id(), patch_oid, merged, spec.summary.clone())?;
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
            return Ok(Step1::Applied(committer.commit_patch_marker(&op)?));
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
            head: head.id().to_string(),
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
                    c.id() != head.id()
                        && inbox_from_message(c.message_raw().unwrap_or_default())
                            == Some((n, patch_oid))
                });
                if let (true, Some(c)) = (done, head_now) {
                    return Ok(Step1::Applied(c.id()));
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
        last_applied: u64,
        inbox: &BTreeMap<u64, Oid>,
    ) -> Result<(), PatchError> {
        let patch = Oid::from_str(&f.patch).ok();
        if f.inbox <= last_applied || (patch.is_some() && inbox.get(&f.inbox) == patch.as_ref()) {
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
        let patch = repo
            .find_commit(patch.ok_or("not a commit id")?)
            .map_err(|e| e.to_string())?;
        let mut index = repo
            .cherrypick_commit(&patch, &head, 0, None)
            .map_err(|e| e.to_string())?;
        if index.has_conflicts() {
            return Err("the merge conflicts".into());
        }
        let merged = index.write_tree_to(repo).map_err(|e| e.to_string())?;
        let plan = restore::plan_tree(repo, head.id(), patch.id(), merged, String::new())
            .map_err(|e| e.to_string())?;
        let paths: Vec<RepoPath> = plan.steps.iter().map(|s| s.path.clone()).collect();
        self.write_back(&plan, &paths, &paths)
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

/// Why the diff from `parent` to `patch` is not one the queue applies:
/// too many paths, a gitlink, or a path agentfs cannot write.
fn validate_diff(
    repo: &Repository,
    patch: &git2::Commit<'_>,
    parent: Oid,
) -> Result<Option<(BlockReason, String, Vec<String>)>, PatchError> {
    let before = repo.find_commit(parent)?.tree()?;
    let after = patch.tree()?;
    let diff = repo.diff_tree_to_tree(Some(&before), Some(&after), None)?;
    let count = diff.deltas().len();
    if count > MAX_PATCH_PATHS {
        return Ok(Some((
            BlockReason::TooLarge,
            format!("the patch changes {count} paths; at most {MAX_PATCH_PATHS} are applied"),
            Vec::new(),
        )));
    }
    for delta in diff.deltas() {
        for file in [delta.old_file(), delta.new_file()] {
            let Some(path) = file.path_bytes() else {
                continue;
            };
            let path = String::from_utf8_lossy(path).into_owned();
            if file.mode() == FileMode::Commit {
                return Ok(Some((
                    BlockReason::Unsupported,
                    format!("{path} is a gitlink (submodule); agentfs never records those"),
                    vec![path],
                )));
            }
            if let Err(e) = RepoPath::new(path.clone()) {
                return Ok(Some((
                    BlockReason::Unsupported,
                    format!("{path:?} is not a path agentfs records: {e}"),
                    vec![path],
                )));
            }
        }
    }
    Ok(None)
}

/// The commit message of a patch's commit: the patch's own, without a
/// final trailer paragraph (agentfs adds its own trailers).
pub fn summary_of(patch: &git2::Commit<'_>, n: u64) -> String {
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
    if message.is_empty() {
        format!("patch {n}")
    } else {
        message
    }
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
