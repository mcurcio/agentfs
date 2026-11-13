//! Queue takes: a landing judged into its upstream, and a fork synced from its upstream
//! (target git-model §6).

use std::collections::{BTreeMap, BTreeSet};

use git2::Oid;

use super::message::{self, Message};
use super::refs;
use crate::tree_actor::stages::{Timer, Which};
use crate::tree_actor::tree::{EventNumber, Lines, MergeFileIdentity};
use crate::tree_actor::{Draft, Error, Result, TreeId, Volume, queue, referee};
use crate::tree_git::{RefUpdate, TreeGit};

/// The actor's [`referee::Landed`]: whether a save's commit is in `upstream`'s history. That
/// never changes, and every line one save wrote has that save's commit: ask Git once per commit,
/// not once per line (an 8 MiB file is 8,000 lines).
pub(in crate::tree_actor) struct LandedInto<'a> {
    pub(in crate::tree_actor) repo: &'a TreeGit,
    pub(in crate::tree_actor) owners: &'a BTreeMap<EventNumber, Oid>,
    pub(in crate::tree_actor) upstream: Oid,
    pub(in crate::tree_actor) reached: BTreeMap<Oid, bool>,
}

impl referee::Landed for LandedInto<'_> {
    fn landed(&mut self, save: EventNumber) -> Result<bool> {
        let owner = *self.owners.get(&save).ok_or(Error::Format)?;
        if let Some(known) = self.reached.get(&owner) {
            return Ok(*known);
        }
        let answer = self.repo.is_ancestor(owner, self.upstream)?;
        self.reached.insert(owner, answer);
        Ok(answer)
    }
}

pub(in crate::tree_actor) struct SyncPlan {
    pub(in crate::tree_actor) target: Option<Oid>,
    pub(in crate::tree_actor) snapshot: referee::Snapshot,
    pub(in crate::tree_actor) remaining: bool,
    pub(in crate::tree_actor) keys: BTreeSet<queue::Key>,
}

impl Volume {
    /// Whether two snapshots show the same: the same files, each with the same line
    /// versions in the same order, and the same name versions. The versions each line
    /// version was written over may differ.
    pub(in crate::tree_actor) fn shows_the_same(
        a: &referee::Snapshot,
        b: &referee::Snapshot,
    ) -> bool {
        a.files.len() == b.files.len()
            && a.names.len() == b.names.len()
            && a.files.iter().all(|(id, file)| {
                b.files
                    .get(id)
                    .is_some_and(|other| other.lines.lines == file.lines.lines)
            })
            && a.names.iter().all(|(id, history)| {
                b.names
                    .get(id)
                    .is_some_and(|other| other.head == history.head)
            })
    }

    pub(in crate::tree_actor) fn prior_decisions(
        &self,
        mut head: Oid,
    ) -> Result<referee::PriorDecisions> {
        let _timer = Timer::start(Which::PriorDecisions);
        let mut result = BTreeMap::new();
        loop {
            let commit = self.repo.read_commit(head)?;
            for collision in message::collisions(&commit.message)? {
                result.entry(collision.lost).or_insert(collision.winner);
            }
            match commit.parents.first() {
                Some(parent) => head = *parent,
                None => break,
            }
        }
        Ok(result)
    }

    pub(in crate::tree_actor) fn build_landing(
        &self,
        tree: TreeId,
        reference: &str,
        incoming: Oid,
        event: EventNumber,
    ) -> Result<Draft> {
        let current = self.head(tree)?;
        let t = self.tree(tree)?;
        let incoming_commit = self.repo.read_commit(incoming)?;
        let base_commit = *incoming_commit.parents.first().ok_or(Error::Format)?;
        let (mine, base, theirs) = (
            Self::snapshot_of(t),
            self.snapshot_at(base_commit)?,
            self.snapshot_at(incoming)?,
        );
        let prior = self.prior_decisions(current)?;
        let decision = {
            let _timer = Timer::start(Which::Judge);
            referee::land(&mine, &base, &theirs, &prior)?
        };
        let head = if base_commit == current {
            // Made on the parent's current version: every line replaces the
            // parent's, so nothing collides, and the save itself is the head.
            // The journal's record of this step moves the pointer (finding 45).
            if !decision.collisions.is_empty()
                || !referee::touches(&decision.snapshot, &theirs).is_empty()
            {
                return Err(Error::Format);
            }
            incoming
        } else {
            let content = self.snapshot_tree(&decision.snapshot)?;
            let message = Message::Landing {
                tree,
                event,
                collisions: &decision.collisions,
            }
            .render()?;
            self.repo
                .write_commit(content, &[current, incoming], &message)?
        };
        let mut changes = vec![
            RefUpdate {
                refname: refs::branch(tree),
                new: Some(head),
                expected_old: Some(current),
            },
            RefUpdate {
                refname: reference.into(),
                new: None,
                expected_old: Some(incoming),
            },
        ];
        if !decision.collisions.is_empty() {
            let name = refs::collision(tree, incoming);
            if self.pointers.get(&name).is_none() {
                changes.push(RefUpdate {
                    refname: name,
                    new: Some(incoming),
                    expected_old: None,
                });
            }
        }
        self.object_table_update(tree, &decision.snapshot, &mut changes)?;
        let enqueued = self.enqueue_moves(tree, head, event, &mut changes)?;
        let mut built = t.clone();
        built.install(decision.snapshot)?;
        let mut draft = Draft::new("agentfs", tree, built);
        draft.changes = changes;
        draft.enqueued = enqueued;
        draft.taken = 1;
        Ok(draft)
    }

    pub(in crate::tree_actor) fn plan_sync(
        &self,
        tree: TreeId,
        open: &BTreeSet<queue::Key>,
    ) -> Result<SyncPlan> {
        let fork = self.tree(tree)?;
        let fork_head = self.head(tree)?;
        let fork_snapshot = Self::snapshot_of(fork);
        let mut todo = Vec::new();
        let upstream = self.upstream_of(tree).ok_or(Error::Format)?;
        let mut cursor = self.head(upstream)?;
        while !self.repo.is_ancestor(cursor, fork_head)? {
            todo.push(cursor);
            let commit = self.repo.read_commit(cursor)?;
            let Some(parent) = commit.parents.first() else {
                break;
            };
            cursor = *parent;
        }
        todo.reverse();
        let mut current = fork_snapshot.clone();
        let mut target = None;
        let mut keys = BTreeSet::new();
        let mut remaining = false;
        for oid in todo {
            let parent = self.snapshot_at(oid)?;
            let undecided = referee::undecided(
                &fork_snapshot,
                &mut LandedInto {
                    repo: &self.repo,
                    owners: &self.owners,
                    upstream: oid,
                    reached: BTreeMap::new(),
                },
            )?;
            let last_lines = self.last_lines(&parent, oid, &undecided)?;
            let next = {
                let _timer = Timer::start(Which::SyncMerge);
                referee::sync(&parent, &fork_snapshot, &undecided, &last_lines)?
            };
            let changes = referee::touches(&current, &next);
            if !changes.is_disjoint(open) {
                remaining = true;
                if target.is_none() {
                    keys = changes;
                }
                break;
            }
            keys.extend(changes);
            current = next;
            target = Some(oid);
        }
        Ok(SyncPlan {
            target,
            snapshot: current,
            remaining,
            keys,
        })
    }

    /// The lines each file held in the parent's last version of it, for the files
    /// the parent at `at` has ended and a fork keeps under an undecided name. The
    /// parent's snapshot has no file to read them from (finding 44).
    pub(in crate::tree_actor) fn last_lines(
        &self,
        parent: &referee::Snapshot,
        at: Oid,
        undecided: &referee::Undecided,
    ) -> Result<BTreeMap<MergeFileIdentity, Lines>> {
        let mut wanted = referee::ended_files(parent, undecided)?;
        let mut lines = BTreeMap::new();
        let mut cursor = at;
        while !wanted.is_empty() {
            let Some(previous) = self.repo.read_commit(cursor)?.parents.first().copied() else {
                break;
            };
            let snapshot = self.snapshot_at(previous)?;
            wanted.retain(|id| match snapshot.files.get(id) {
                Some(file) => {
                    lines.insert(*id, file.lines.clone());
                    false
                }
                None => true,
            });
            cursor = previous;
        }
        Ok(lines)
    }

    /// Whether a landing of `tree` waits in its upstream's queue.
    pub(in crate::tree_actor) fn pending_from(&self, tree: TreeId) -> Result<bool> {
        let Some(upstream) = self.upstream_of(tree) else {
            return Ok(false);
        };
        for (_, oid) in self.pointers.under(&refs::queue(upstream)) {
            let commit = self.repo.read_commit(oid)?;
            if message::tree_of(&commit.message)? == Some(tree) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub(in crate::tree_actor) fn build_sync(
        &self,
        tree: TreeId,
        reference: &str,
        value: Oid,
        plan: SyncPlan,
        event: EventNumber,
    ) -> Result<Draft> {
        let current = self.head(tree)?;
        let mut built = self.tree(tree)?.clone();
        let mut changes = Vec::new();
        if let Some(target) = plan.target {
            let parent = self.snapshot_at(target)?;
            let mut snapshot = plan.snapshot;
            // R6.1: a sync that can fast-forward does. It can when nothing of the fork waits
            // for the upstream and the sync shows what the upstream's commit shows (the
            // model's `World::sync`). The fork then takes that commit's line maps and names,
            // so a version the upstream decided against stays in history only (finding 51).
            let head = if Self::shows_the_same(&parent, &snapshot) && !self.pending_from(tree)? {
                for (id, file) in &mut snapshot.files {
                    if let Some(theirs) = parent.files.get(id) {
                        file.lines = theirs.lines.clone();
                    }
                }
                snapshot.names = parent.names.clone();
                target
            } else {
                let content = self.snapshot_tree(&snapshot)?;
                let message = Message::Sync { tree, event }.render()?;
                self.repo
                    .write_commit(content, &[current, target], &message)?
            };
            changes.push(RefUpdate {
                refname: refs::branch(tree),
                new: Some(head),
                expected_old: Some(current),
            });
            self.object_table_update(tree, &snapshot, &mut changes)?;
            built.install(snapshot)?;
        }
        let taken = !plan.remaining;
        if taken {
            changes.push(RefUpdate {
                refname: reference.into(),
                new: None,
                expected_old: Some(value),
            });
        }
        let mut draft = Draft::new("agentfs", tree, built);
        draft.changes = changes;
        draft.taken = u64::from(taken);
        Ok(draft)
    }
}
