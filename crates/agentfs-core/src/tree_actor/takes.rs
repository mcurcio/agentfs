//! Queue takes off the actor (target git-model §6; docs/trees.md, "Takes").
//!
//! A take runs in three parts, one at a time per volume: the actor reads its
//! inputs under its lock ([`TreeActor::take_inputs`]); [`decide`] plans and
//! judges it on a [`BuiltSnapshot`], with the actor unlocked; the actor lands
//! the decision under its lock ([`TreeActor::land`]), or refuses it when a
//! step after the decision changed what it read. A landed decision read
//! nothing that changed and writes nothing an open file holds, so it is the
//! take an atomic decision at the land would have made.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Instant;

use super::history::builder::take_diff;
use super::history::built::{BuiltSnapshot, History};
use super::history::take::{self as plan, Decided};
use super::history::{self, refs};
use super::journal::durable::RecordNo;
use super::journal::{Change, TakeHead, TakeRecord, Target};
use super::queue::{self, Key};
use super::stages::{Timer, Which};
use super::tree::EventNumber;
use super::{Error, Result, Took, TreeActor, TreeId, referee};

/// What a decision needs from the actor, read under its lock.
#[derive(Clone, Debug)]
pub(in crate::tree_actor) struct TakeInputs {
    pub tree: TreeId,
    /// The keys the tree's write-opens hold (R9).
    pub held: BTreeSet<Key>,
    /// The last step that moved the tree's head when the inputs were read.
    pub head_step: RecordNo,
    /// A fork's upstream and the last step that moved its head: a sync reads it.
    pub upstream: Option<(TreeId, RecordNo)>,
    /// Every record appended when the inputs were read: the decision waits
    /// for history built through it, so it sees every step its inputs saw.
    pub through: RecordNo,
}

/// A take decided on a built snapshot, not yet landed.
#[derive(Debug)]
pub(in crate::tree_actor) struct Decision {
    pub tree: TreeId,
    pub entry: String,
    /// The built point the decision read.
    pub decided_at: RecordNo,
    /// The tree's head step the decision saw ([`TakeInputs::head_step`]).
    pub head_at: RecordNo,
    /// The upstream's head step a sync read.
    pub upstream_at: Option<(TreeId, RecordNo)>,
    /// Touched keys, plus held keys that cut its sync or blocked an earlier entry.
    pub reads: BTreeSet<Key>,
    /// Its plan's keys (a losing change's files included, R9.1).
    pub writes: BTreeSet<Key>,
    pub take: TakeRecord,
    started: Instant,
}

/// Why [`TreeActor::land`] did not land a decision. The take stays queued and
/// is decided again at the next built event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::tree_actor) enum Refused {
    /// A step after the decision touched a key it read.
    FilesChanged,
    /// A write-open holds a key it writes.
    Held,
    /// A fast-forward's head, or a sync's upstream head, moved after the decision.
    HeadMoved,
    /// Its queue entry is no longer queued.
    EntryGone,
}

/// The tree's first entry P1 lets go, decided on `snapshot`; none when every
/// entry waits, the queue as built is empty, or the snapshot is older than
/// the inputs. Pure over the snapshot's Git reads and the inputs.
///
/// # Errors
///
/// A failed read of history, or a judgement that fails.
pub(in crate::tree_actor) fn decide(
    snapshot: &BuiltSnapshot,
    inputs: &TakeInputs,
) -> Result<Option<Decision>> {
    let started = Instant::now();
    let tree = inputs.tree;
    let entries = snapshot.queue(tree);
    if snapshot.at() < inputs.through || entries.is_empty() {
        return Ok(None);
    }
    let open = &inputs.held;
    let mine = history::tree_snapshot(snapshot.repo(), snapshot.pointers(), tree)?;
    let mut plans = Vec::new();
    let mut cuts = Vec::new();
    let planning = Timer::start(Which::QueuePlan);
    for (_, oid) in &entries {
        match inputs.upstream {
            None => {
                plans.push(plan::plan_landing(snapshot, *oid)?);
                cuts.push(None);
            }
            Some((upstream, _)) => {
                let cut = plan::plan_sync(snapshot, tree, upstream, &mine, open)?;
                plans.push(queue::Plan {
                    source: upstream,
                    files: cut.keys().clone(),
                });
                cuts.push(Some(cut));
            }
        }
    }
    drop(planning);
    let Some(index) = queue::eligible(&plans, open) else {
        return Ok(None);
    };
    // The held keys that blocked an earlier entry: were one let go, an atomic
    // take would take that entry first.
    let mut reads: BTreeSet<Key> = plans[..index]
        .iter()
        .flat_map(|p| p.files.intersection(open).cloned())
        .collect();
    let (entry, incoming) = entries[index].clone();
    let decided = match (cuts.swap_remove(index), inputs.upstream) {
        (Some(cut), Some((upstream, _))) => {
            reads.extend(cut.held().iter().cloned());
            plan::decide_sync(snapshot, tree, upstream, cut)?
        }
        _ => plan::decide_landing(snapshot, tree, &mine, incoming)?,
    };
    let Decided {
        head,
        snapshot: after,
        collisions,
        entry_done,
    } = decided;
    let mut writes = plans[index].files.clone();
    let (files, names) = match &after {
        Some(after) => {
            writes.extend(referee::touches(&mine, after));
            take_diff(&mine, after)
        }
        None => Default::default(),
    };
    reads.extend(writes.iter().cloned());
    Ok(Some(Decision {
        tree,
        entry: entry.clone(),
        decided_at: snapshot.at(),
        head_at: inputs.head_step,
        upstream_at: inputs.upstream,
        reads,
        writes,
        take: TakeRecord {
            entry,
            whole: entry_done,
            decided_at: snapshot.at(),
            head,
            files,
            names,
            collisions,
        },
        started,
    }))
}

/// The actor as a take reaches it: locked only to read inputs and to land.
pub(in crate::tree_actor) trait Lands {
    /// Runs `f` on the actor, locked.
    ///
    /// # Errors
    ///
    /// [`Error::Stopped`] when the actor stopped; `f`'s.
    fn with<R>(&mut self, f: impl FnOnce(&mut TreeActor) -> Result<R>) -> Result<R>;
}

impl Lands for &mut TreeActor {
    fn with<R>(&mut self, f: impl FnOnce(&mut TreeActor) -> Result<R>) -> Result<R> {
        f(self)
    }
}

/// One take in `tree`, as the view's worker, `take_next` and the CLI run it:
/// inputs under the lock, the decision on history built through them with the
/// actor unlocked, the land under the lock. A refused decision is decided
/// again on the history that refused it.
///
/// # Errors
///
/// [`Error::Stopped`], a failed build, a failed decision or land.
pub(in crate::tree_actor) fn take_one(
    actor: &mut impl Lands,
    history: &History,
    tree: TreeId,
) -> Result<Took> {
    loop {
        let inputs = actor.with(|a| {
            a.tree(tree)?;
            if !a.queued(tree) {
                return Ok(None);
            }
            // A take is a step: pending closes save first, and its plan reads
            // the tree they leave, as built.
            a.run_pending_closes(tree, None)?;
            a.take_inputs(tree).map(Some)
        })?;
        let Some(inputs) = inputs else {
            return Ok(Took::Empty);
        };
        history.wait_built(inputs.through)?;
        let snapshot = history.snapshot();
        let decision = decide(&snapshot, &inputs)?;
        // The entries before the one decided, or all of them, wait.
        let waiting: Vec<String> = snapshot
            .queue(tree)
            .into_iter()
            .map(|(name, _)| name)
            .filter(|name| decision.as_ref().is_none_or(|d| *name < d.entry))
            .collect();
        actor.with(|a| {
            a.note_waiting(&waiting);
            Ok(())
        })?;
        let Some(decision) = decision else {
            return Ok(Took::Waiting);
        };
        match actor.with(|a| a.land(decision))? {
            Ok(_) => return Ok(Took::Taken),
            Err(refused) => {
                eprintln!("tree TakeRefused: tree={tree} {refused:?}");
            }
        }
    }
}

impl TreeActor {
    /// Whether `tree`'s queue holds an entry, as the journal's records name it.
    pub(in crate::tree_actor) fn queued(&self, tree: TreeId) -> bool {
        !self.pointers.under(&refs::queue(tree)).is_empty()
    }

    /// The last step that moved `tree`'s head since this actor opened.
    fn head_step(&self, tree: TreeId) -> RecordNo {
        self.heads.get(&tree).copied().unwrap_or(RecordNo::ZERO)
    }

    /// What a decision on `tree` reads from the actor: the keys its
    /// write-opens hold, its head step and its upstream's, and the last record.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] for no such tree; [`Error::Stopped`].
    pub(in crate::tree_actor) fn take_inputs(&self, tree: TreeId) -> Result<TakeInputs> {
        let t = self.tree(tree)?;
        let held = queue::held(
            t,
            self.handles
                .values()
                .filter(|h| h.tree == tree)
                .map(|h| h.object),
        );
        Ok(TakeInputs {
            tree,
            held,
            head_step: self.head_step(tree),
            upstream: self.upstream_of(tree).map(|u| (u, self.head_step(u))),
            through: self.durability().appended(),
        })
    }

    /// Notes when each waiting entry started waiting (the wait metrics).
    pub(in crate::tree_actor) fn note_waiting(&mut self, waiting: &[String]) {
        for reference in waiting {
            if !self.waiting_since.contains_key(reference) {
                self.waiting_since.insert(reference.clone(), Instant::now());
                self.metrics.waits = self.metrics.waits.saturating_add(1);
            }
        }
    }

    /// Records the keys a step of `tree` touched, while its queue holds an
    /// entry a decision may read: a decision reads a queue as built, so a
    /// step before the entry was queued is before every decision on it.
    pub(in crate::tree_actor) fn touch(
        &mut self,
        tree: TreeId,
        seq: u64,
        keys: impl IntoIterator<Item = Key>,
    ) {
        if !self.queued(tree) {
            return;
        }
        let touched = self.touched.entry(tree).or_default();
        for key in keys {
            touched.insert(key, RecordNo::new(seq));
        }
    }

    /// Appends the take if no step after `decided_at` touched a key of
    /// `reads`, no write-open holds a key of `writes`, the entry is still
    /// queued, and (a fast-forward) the head or (a sync) the upstream's head
    /// has not moved; enqueues its moves under this record's number.
    ///
    /// # Errors
    ///
    /// A failed record ([`Error::Stopped`], an I/O error) or a take whose
    /// shape does not apply.
    pub(in crate::tree_actor) fn land(
        &mut self,
        decision: Decision,
    ) -> Result<std::result::Result<EventNumber, Refused>> {
        let tree = decision.tree;
        self.tree(tree)?;
        self.run_pending_closes(tree, None)?;
        if let Some(refused) = self.refusal(&decision) {
            return Ok(Err(refused));
        }
        let event = self.record_take(decision)?;
        self.prune_touched(tree);
        Ok(Ok(event))
    }

    fn refusal(&self, decision: &Decision) -> Option<Refused> {
        let tree = decision.tree;
        if self.pointers.get(&decision.entry).is_none() {
            return Some(Refused::EntryGone);
        }
        let changed = self.touched.get(&tree).is_some_and(|touched| {
            decision
                .reads
                .iter()
                .any(|key| touched.get(key).is_some_and(|at| *at > decision.decided_at))
        });
        if changed {
            return Some(Refused::FilesChanged);
        }
        let held = self.take_inputs(tree).ok()?.held;
        if !decision.writes.is_disjoint(&held) {
            return Some(Refused::Held);
        }
        let fast_forward = matches!(decision.take.head, TakeHead::FastForward { .. });
        if fast_forward && self.head_step(tree) != decision.head_at {
            return Some(Refused::HeadMoved);
        }
        if let Some((upstream, at)) = decision.upstream_at
            && self.head_step(upstream) != at
        {
            return Some(Refused::HeadMoved);
        }
        None
    }

    /// Drops touched keys no later decision reads: those at or before the
    /// built point (every later decision is at or after it), and every key
    /// of a tree whose queue is empty.
    fn prune_touched(&mut self, tree: TreeId) {
        if !self.queued(tree) {
            self.touched.remove(&tree);
            return;
        }
        let built = self.history.built_through();
        for touched in self.touched.values_mut() {
            touched.retain(|_, at| *at > built);
        }
    }

    /// Records a decided take and applies it: its shape's transition and the
    /// bytes of the files it changed.
    fn record_take(&mut self, decision: Decision) -> Result<EventNumber> {
        let Decision {
            tree,
            entry,
            take,
            started,
            ..
        } = decision;
        let seq = self.next;
        let event = EventNumber::new(seq);
        let landing = tree.is_root();
        let mut pointers = Vec::new();
        let moved = !matches!(take.head, TakeHead::Stay);
        match &take.head {
            TakeHead::FastForward { to } => {
                pointers.push(self.change(refs::branch(tree), Some(Target::Oid(to.clone()))));
            }
            TakeHead::Landing { .. } | TakeHead::Sync { .. } => {
                pointers
                    .push(self.change(refs::branch(tree), Some(Target::Step(RecordNo::new(seq)))));
            }
            TakeHead::Stay => {}
        }
        if take.whole {
            pointers.push(self.change(entry.clone(), None));
        }
        if let TakeHead::Landing { incoming } = &take.head
            && !take.collisions.is_empty()
        {
            let incoming: git2::Oid = incoming.parse().map_err(|_| Error::Format)?;
            let name = refs::collision(tree, incoming);
            if self.pointers.get(&name).is_none() {
                pointers.push(self.change(name, Some(Target::Oid(incoming.to_string()))));
            }
        }
        if moved {
            pointers.push(self.change(
                refs::object_table(tree),
                Some(Target::Step(RecordNo::new(seq))),
            ));
        }
        let mut enqueued = 0;
        if landing {
            enqueued = self.enqueue_syncs(tree, event, &mut pointers);
        }
        let whole = take.whole;
        let change = Change::Take {
            tree: tree.to_string(),
            take,
        };
        // The shape's transition, tried before the record: a take that would
        // put two files on one object fails here (finding 39).
        let mut after = self.tree(tree)?.clone();
        let keys = after.shape.apply(&change, event)?;
        let by = self.agentfs();
        self.record_step(by, change.clone(), pointers)?;
        self.touch(tree, seq, keys);
        if let Change::Take { take, .. } = &change {
            for (_, file) in &take.files {
                let Some(file) = file else { continue };
                let bytes = file.lines.bytes();
                let changed = after
                    .bytes
                    .get(&file.object)
                    .is_some_and(|old| old[..] != bytes[..]);
                if changed {
                    self.changed.push((tree, file.object));
                }
                after.bytes.insert(file.object, Arc::new(bytes.into()));
            }
        }
        self.trees.insert(tree, after);
        if let Some(since) = self.waiting_since.remove(&entry) {
            let nanos = u64::try_from(since.elapsed().as_nanos()).unwrap_or(u64::MAX);
            self.metrics.wait_nanos = self.metrics.wait_nanos.saturating_add(nanos);
            self.metrics.max_wait_nanos = self.metrics.max_wait_nanos.max(nanos);
        }
        self.metrics.queued = self
            .metrics
            .queued
            .saturating_sub(u64::from(whole))
            .saturating_add(enqueued);
        let nanos = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
        self.metrics.takes = self.metrics.takes.saturating_add(1);
        self.metrics.take_nanos = self.metrics.take_nanos.saturating_add(nanos);
        self.metrics.max_take_nanos = self.metrics.max_take_nanos.max(nanos);
        if landing {
            self.metrics.landings = self.metrics.landings.saturating_add(1);
            self.metrics.landing_nanos = self.metrics.landing_nanos.saturating_add(nanos);
        } else {
            self.metrics.syncs = self.metrics.syncs.saturating_add(1);
            self.metrics.sync_nanos = self.metrics.sync_nanos.saturating_add(nanos);
        }
        self.after_step();
        Ok(event)
    }
}

/// The touched keys of each tree, each key's last touching step.
pub(in crate::tree_actor) type Touched = BTreeMap<TreeId, BTreeMap<Key, RecordNo>>;
