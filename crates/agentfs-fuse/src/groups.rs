//! fsync-class groups in the mount (design §8): which group a write
//! belongs to, holding writes while a group is captured, and when each
//! group is due.
//!
//! A **group** is a database and its `-wal` and `-journal` files
//! ([`agentfs_core::fsync_class`]). Every operation that changes a member's
//! content or name takes a [`Hold`] on the group's [`Gate`] around its
//! system call and marks the group changed inside the hold. A capture
//! ([`crate::ops::Ops`]) *freezes* the gate: it waits for the holds in
//! progress, blocks new ones, and copies every member while nothing can
//! change any of them, so the copies are one point in time, as a crash
//! would leave them, which SQLite recovers from. Renames are kept out by the
//! mount's `names` lock instead: a capture holds it for reading.
//!
//! Lock order: `names` before a group's capture lock before its gate. A
//! thread holding a [`Hold`] never waits for `names` or freezes a gate.
//!
//! **When** a group is captured, from [`GroupState`]:
//!
//! - at `fsync` of a member, if the group changed and no capture was
//!   attempted in the last `agentfs.coalesce` ([`GroupState::fsync_due`]);
//! - by the trailing timer, `agentfs.coalesce` after the first change since
//!   the last capture, and never sooner than `agentfs.coalesce` after the
//!   last attempt ([`GroupState::due_at`]), so a change is recorded at most
//!   one interval late (plus the capture itself);
//! - at the close of the group's last open handle, its release, and
//!   unmount, whenever it changed. Closing one member while another stays
//!   open (SQLite's `-journal` in DELETE mode, every transaction) leaves
//!   the change to the two rules above, so the rate stays bounded.
//!
//! **One entry point at a time** (design §8, §9): a group's holders at any
//! instant share one entry point, and [`Group::hold`] refuses a writer
//! through another entry point while the group holds unrecorded changes
//! ([`Foreign`]): the operation captures the group first, as the pending
//! writer, with trigger `cut`, then starts over. No capture ever holds two
//! entry points.
//!
//! **Operator operations** (a restore or revert through the mount, design
//! §15): a change made by an operation's actor marks the group
//! ([`GroupState::actor`]), as agentfs's own change
//! ([`Writer::agentfs`]). While the operation runs, no trigger but the
//! operation's own capture ([`Groups::in_operation`]) records the group, so
//! history never holds a group half restored.
//!
//! A capture that fails leaves the group changed: the timer retries it one
//! interval later. A capture the recorder refuses (a member over a limit)
//! counts as done, as a refused close does for an ordinary file.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use agentfs_core::fsync_class::{GroupKey, GroupMembers};
use agentfs_core::record::{Caller, EntryPoint, Writer};

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Whether a process id is an actor of an operator operation now
/// ([`agentfs_core::volume::Recorder::in_operation`]).
pub type IsActor = Arc<dyn Fn(u32) -> bool + Send + Sync>;

/// Holds writes to a group's members out while it is captured. Writers
/// ([`Gate::hold`]) share it; a capture ([`Gate::freeze`]) has it alone,
/// and a waiting capture goes before new writers, so a busy database is
/// still captured.
#[derive(Default)]
pub struct Gate {
    state: Mutex<GateState>,
    cv: Condvar,
}

#[derive(Default)]
struct GateState {
    holders: usize,
    /// The entry point every current holder writes through.
    entry: Option<EntryPoint>,
    freezers_waiting: usize,
    frozen: bool,
}

impl Gate {
    /// Waits while the gate is frozen, a freezer waits, or holders through
    /// another entry point than `entry` hold it.
    fn hold(&self, entry: &EntryPoint) -> HoldGuard<'_> {
        let mut s = lock(&self.state);
        while s.frozen
            || s.freezers_waiting > 0
            || (s.holders > 0 && s.entry.as_ref().is_some_and(|e| e != entry))
        {
            s = self.cv.wait(s).unwrap_or_else(PoisonError::into_inner);
        }
        s.holders += 1;
        s.entry = Some(entry.clone());
        HoldGuard { gate: self }
    }

    /// Waits for every hold to end, then keeps new ones out until the
    /// guard drops.
    pub fn freeze(&self) -> Frozen<'_> {
        let mut s = lock(&self.state);
        s.freezers_waiting += 1;
        while s.holders > 0 || s.frozen {
            s = self.cv.wait(s).unwrap_or_else(PoisonError::into_inner);
        }
        s.freezers_waiting -= 1;
        s.frozen = true;
        Frozen { gate: self }
    }
}

struct HoldGuard<'a> {
    gate: &'a Gate,
}

impl Drop for HoldGuard<'_> {
    fn drop(&mut self) {
        let mut s = lock(&self.gate.state);
        s.holders -= 1;
        if s.holders == 0 {
            s.entry = None;
            self.gate.cv.notify_all();
        }
    }
}

/// A frozen gate; dropping it lets writers in again.
pub struct Frozen<'a> {
    gate: &'a Gate,
}

impl Drop for Frozen<'_> {
    fn drop(&mut self) {
        lock(&self.gate.state).frozen = false;
        self.gate.cv.notify_all();
    }
}

/// A write to a group in progress: the group cannot be captured until it
/// drops. Mark the group changed with [`Hold::changed`] before dropping it.
pub struct Hold<'a> {
    group: &'a Group,
    /// Who the hold's changes are recorded as.
    writer: Writer,
    /// The writer is an operator operation's actor.
    actor: Option<Caller>,
    _guard: HoldGuard<'a>,
}

impl Hold<'_> {
    /// Marks the group changed by the hold's writer, inside the hold, so a
    /// capture that starts after the hold ends sees both the change and
    /// the mark.
    pub fn changed(&self) {
        self.group.changed(&self.writer, self.actor);
    }
}

/// A hold or change refused because the group holds unrecorded changes
/// from another entry point than the writer's (design §8, §9; provenance
/// slice 1): the operation captures the group first, as `pending`, with
/// trigger `cut`, then asks again. No record ever holds two entry points.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Foreign {
    /// The writer whose changes the group holds.
    pub pending: agentfs_core::record::Writer,
}

/// When a group changed and was captured.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GroupState {
    /// The first change since the last capture; `None` when clean.
    pub dirty_since: Option<Instant>,
    /// Counts changes, so a capture only cleans the group if nothing
    /// changed since it froze the gate.
    pub generation: u64,
    /// When the last capture was attempted.
    pub last_attempt: Option<Instant>,
    /// Who changed the group last: every change since the last capture
    /// came through its entry point ([`Group::hold`]).
    pub writer: Option<Writer>,
    /// Handles open on members.
    pub handles: usize,
    /// The operator operation's actor that changed the group (design §15):
    /// while it is one, only the operation's own capture records the group.
    pub actor: Option<Caller>,
}

impl GroupState {
    pub fn is_dirty(&self) -> bool {
        self.dirty_since.is_some()
    }

    /// Whether an `fsync` at `now` captures the group: it changed, and the
    /// last attempt is at least `coalesce` ago.
    pub fn fsync_due(&self, now: Instant, coalesce: Duration) -> bool {
        self.is_dirty() && self.last_attempt.is_none_or(|last| now >= last + coalesce)
    }

    /// When the trailing timer captures the group: `coalesce` after its
    /// first change since the last capture, or after the last attempt if
    /// that is later. `None` when clean.
    pub fn due_at(&self, coalesce: Duration) -> Option<Instant> {
        let since = self.dirty_since?;
        let from = match self.last_attempt {
            Some(last) if last > since => last,
            _ => since,
        };
        Some(from + coalesce)
    }

    fn changed(&mut self, now: Instant, writer: &Writer) {
        self.dirty_since.get_or_insert(now);
        self.generation += 1;
        self.writer = Some(writer.clone());
    }

    /// A capture that froze the group at `generation` ended at `now`;
    /// `done` if it was recorded (or refused). The group is clean unless it
    /// changed since.
    pub fn captured(&mut self, generation: u64, now: Instant, done: bool) {
        self.last_attempt = Some(now);
        if done && self.generation == generation {
            self.dirty_since = None;
            self.actor = None;
        }
    }
}

/// What the capture thread waits on: a group became dirty, or stop.
#[derive(Default)]
struct Kick {
    state: Mutex<KickState>,
    cv: Condvar,
}

#[derive(Default)]
struct KickState {
    kicked: bool,
    stopped: bool,
}

/// One group in the mount.
pub struct Group {
    key: GroupKey,
    /// Member paths from the mount root, sorted.
    members: Vec<Vec<u8>>,
    coalesce: Duration,
    gate: Gate,
    state: Mutex<GroupState>,
    /// Signalled when a capture ends an operator operation's deferral
    /// ([`Group::wait_undeferred`]).
    undeferred: Condvar,
    /// One capture at a time.
    capturing: Mutex<()>,
    kick: Arc<Kick>,
    is_actor: IsActor,
}

impl fmt::Debug for Group {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Group")
            .field("key", &self.key)
            .field("coalesce", &self.coalesce)
            .finish_non_exhaustive()
    }
}

impl Group {
    pub fn key(&self) -> &GroupKey {
        &self.key
    }

    pub fn members(&self) -> &[Vec<u8>] {
        &self.members
    }

    pub fn coalesce(&self) -> Duration {
        self.coalesce
    }

    pub fn gate(&self) -> &Gate {
        &self.gate
    }

    /// Starts a change to a member by `writer`: waits while the group is
    /// being captured or held through another entry point. Then refuses
    /// with [`Foreign`] if the group holds unrecorded changes through
    /// another entry point: capture it as [`Foreign::pending`] first. A
    /// change by an operator operation's actor ([`IsActor`]) is held as
    /// [`Writer::agentfs`] of its caller, the rule
    /// [`agentfs_core::volume::Recorder::record`] applies to its records.
    pub fn hold(&self, writer: &Writer) -> Result<Hold<'_>, Foreign> {
        let actor = (self.is_actor)(writer.caller.pid).then_some(writer.caller);
        let writer = match actor {
            Some(caller) => Writer::agentfs(caller),
            None => writer.clone(),
        };
        let guard = self.gate.hold(&writer.entry);
        // Holding: no capture can clean the group, and every other holder
        // shares the entry point, so this answer stands until it drops.
        {
            let s = self.state();
            if s.is_dirty()
                && let Some(pending) = &s.writer
                && pending.entry != writer.entry
            {
                return Err(Foreign {
                    pending: pending.clone(),
                });
            }
        }
        Ok(Hold {
            group: self,
            writer,
            actor,
            _guard: guard,
        })
    }

    fn changed(&self, writer: &Writer, actor: Option<Caller>) {
        let was_clean = {
            let mut s = self.state();
            let was_clean = !s.is_dirty();
            s.changed(Instant::now(), writer);
            if actor.is_some() {
                s.actor = actor;
            }
            was_clean
        };
        if was_clean {
            let mut k = lock(&self.kick.state);
            k.kicked = true;
            self.kick.cv.notify_all();
        }
    }

    pub fn state(&self) -> MutexGuard<'_, GroupState> {
        lock(&self.state)
    }

    /// The actor of the operator operation whose changes the group holds,
    /// while that operation runs: no other capture may record it.
    pub fn deferred(&self) -> Option<Caller> {
        let actor = self.state().actor?;
        (self.is_actor)(actor.pid).then_some(actor)
    }

    /// Blocks until no running operator operation defers the group
    /// ([`Group::deferred`]): woken by the operation's end capture
    /// ([`Group::captured`]), an event, never a poll. Call it holding
    /// neither `names` nor any hold.
    pub fn wait_undeferred(&self) {
        let mut s = self.state();
        while s.actor.is_some_and(|actor| (self.is_actor)(actor.pid)) {
            s = self
                .undeferred
                .wait(s)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// A capture that froze the group at `generation` ended; `done` if it
    /// was recorded (or refused) ([`GroupState::captured`]). The capture at
    /// an operator operation's end (`operation_end`) ends the operation's
    /// claim on the group whatever its outcome: a group it left changed is
    /// the timer's, as once the operation ends. Wakes
    /// [`Group::wait_undeferred`].
    pub fn captured(&self, generation: u64, done: bool, operation_end: bool) {
        {
            let mut s = self.state();
            s.captured(generation, Instant::now(), done);
            if operation_end {
                s.actor = None;
            }
        }
        self.undeferred.notify_all();
    }

    /// Serializes captures of this group.
    pub fn capturing(&self) -> MutexGuard<'_, ()> {
        lock(&self.capturing)
    }

    pub fn opened(&self) {
        self.state().handles += 1;
    }

    /// A handle on a member was released; returns how many are left.
    pub fn released(&self) -> usize {
        let mut s = self.state();
        s.handles = s.handles.saturating_sub(1);
        s.handles
    }
}

/// Every group the mount knows. A group is kept while it is dirty or
/// anything holds it; [`Groups::prune`] drops the rest.
pub struct Groups {
    map: Mutex<HashMap<GroupKey, Arc<Group>>>,
    kick: Arc<Kick>,
    is_actor: IsActor,
}

impl Default for Groups {
    /// Groups no operator operation ever changes.
    fn default() -> Self {
        Self::new(Arc::new(|_| false))
    }
}

impl Groups {
    /// No groups yet; `is_actor` tells which processes are actors of a
    /// running operator operation.
    pub fn new(is_actor: IsActor) -> Self {
        Self {
            map: Mutex::new(HashMap::new()),
            kick: Arc::new(Kick::default()),
            is_actor,
        }
    }

    /// The group `members` describes, made on first use.
    pub fn get(&self, members: GroupMembers) -> Arc<Group> {
        let mut map = lock(&self.map);
        map.entry(members.key.clone())
            .or_insert_with(|| {
                Arc::new(Group {
                    key: members.key,
                    members: members
                        .members
                        .iter()
                        .map(|p| p.as_str().as_bytes().to_vec())
                        .collect(),
                    coalesce: members.coalesce,
                    gate: Gate::default(),
                    state: Mutex::new(GroupState::default()),
                    undeferred: Condvar::new(),
                    capturing: Mutex::new(()),
                    kick: self.kick.clone(),
                    is_actor: self.is_actor.clone(),
                })
            })
            .clone()
    }

    /// Every dirty group.
    pub fn dirty(&self) -> Vec<Arc<Group>> {
        lock(&self.map)
            .values()
            .filter(|g| g.state().is_dirty())
            .cloned()
            .collect()
    }

    /// The groups the timer captures at `now`: never one a running
    /// operator operation changed.
    pub fn due(&self, now: Instant) -> Vec<Arc<Group>> {
        lock(&self.map)
            .values()
            .filter(|g| g.state().due_at(g.coalesce).is_some_and(|at| at <= now))
            .filter(|g| g.deferred().is_none())
            .cloned()
            .collect()
    }

    /// Every group a running operator operation changed, with the actor
    /// that changed it.
    pub fn in_operation(&self) -> Vec<(Arc<Group>, Caller)> {
        lock(&self.map)
            .values()
            .filter_map(|g| g.deferred().map(|actor| (g.clone(), actor)))
            .collect()
    }

    /// Wakes [`Groups::wait`], so it works out the next deadline again.
    pub fn wake(&self) {
        lock(&self.kick.state).kicked = true;
        self.kick.cv.notify_all();
    }

    /// When the next group is due, if any is dirty and not held back by a
    /// running operator operation.
    pub fn next_due(&self) -> Option<Instant> {
        lock(&self.map)
            .values()
            .filter(|g| g.deferred().is_none())
            .filter_map(|g| g.state().due_at(g.coalesce))
            .min()
    }

    /// Drops clean groups nothing else holds.
    pub fn prune(&self) {
        lock(&self.map).retain(|_, g| Arc::strong_count(g) > 1 || g.state().is_dirty());
    }

    pub fn len(&self) -> usize {
        lock(&self.map).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Waits until `deadline` (forever if `None`), a group becomes dirty,
    /// or [`Groups::stop`]. Returns false once stopped.
    pub fn wait(&self, deadline: Option<Instant>) -> bool {
        let mut k = lock(&self.kick.state);
        loop {
            if k.stopped {
                return false;
            }
            if k.kicked {
                k.kicked = false;
                return true;
            }
            match deadline {
                None => k = self.kick.cv.wait(k).unwrap_or_else(PoisonError::into_inner),
                Some(at) => {
                    let now = Instant::now();
                    if now >= at {
                        return true;
                    }
                    k = self
                        .kick
                        .cv
                        .wait_timeout(k, at - now)
                        .unwrap_or_else(PoisonError::into_inner)
                        .0;
                }
            }
        }
    }

    /// Ends [`Groups::wait`] for good.
    pub fn stop(&self) {
        lock(&self.kick.state).stopped = true;
        self.kick.cv.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;

    use agentfs_core::paths::RepoPath;

    use super::*;

    const C: Caller = Caller {
        uid: 1,
        gid: 1,
        pid: 1,
    };
    const W: Writer = Writer {
        caller: C,
        entry: EntryPoint::Main,
    };
    const S: Duration = Duration::from_secs(1);

    fn members(stem: &str) -> GroupMembers {
        GroupMembers {
            key: GroupKey {
                dir: String::new(),
                group: Some("sqlite".into()),
                stem: stem.into(),
            },
            members: vec![
                RepoPath::new(stem).unwrap(),
                RepoPath::new(format!("{stem}-wal")).unwrap(),
            ],
            coalesce: 5 * S,
        }
    }

    #[test]
    fn the_schedule() {
        let t0 = Instant::now();
        let mut s = GroupState::default();
        assert!(!s.fsync_due(t0, 5 * S));
        assert_eq!(s.due_at(5 * S), None);
        s.changed(t0, &W);
        // Never attempted: an fsync captures at once, the timer later.
        assert!(s.fsync_due(t0, 5 * S));
        assert_eq!(s.due_at(5 * S), Some(t0 + 5 * S));
        // Later changes do not push the timer back.
        s.changed(t0 + 3 * S, &W);
        assert_eq!(s.due_at(5 * S), Some(t0 + 5 * S));
        let generation = s.generation;
        s.captured(generation, t0 + 3 * S, true);
        assert!(!s.is_dirty());
        // A change right after a capture: fsync waits out the interval,
        // and so does the timer.
        s.changed(t0 + 4 * S, &W);
        assert!(!s.fsync_due(t0 + 7 * S, 5 * S));
        assert!(s.fsync_due(t0 + 8 * S, 5 * S));
        assert_eq!(s.due_at(5 * S), Some(t0 + 9 * S));
        // A failed attempt keeps it dirty, retried one interval later.
        let generation = s.generation;
        s.captured(generation, t0 + 10 * S, false);
        assert!(s.is_dirty());
        assert_eq!(s.due_at(5 * S), Some(t0 + 15 * S));
        // A change during a capture keeps it dirty.
        let generation = s.generation;
        s.changed(t0 + 11 * S, &W);
        s.captured(generation, t0 + 12 * S, true);
        assert!(s.is_dirty());
    }

    #[test]
    fn groups_are_shared_by_key_and_pruned_when_clean_and_unused() {
        let groups = Groups::default();
        let a = groups.get(members("a.db"));
        assert!(Arc::ptr_eq(&a, &groups.get(members("a.db"))));
        assert_eq!(a.members(), [b"a.db".to_vec(), b"a.db-wal".to_vec()]);
        let b = groups.get(members("b.db"));
        assert_eq!(groups.len(), 2);
        b.hold(&W).unwrap().changed();
        drop((a, b));
        groups.prune();
        // b is dirty, so it stays.
        assert_eq!(groups.len(), 1);
        assert_eq!(groups.dirty().len(), 1);
        let due = groups.next_due().unwrap();
        assert!(groups.due(due - S).is_empty());
        assert_eq!(groups.due(due).len(), 1);
    }

    #[test]
    fn a_freeze_waits_for_holds_and_keeps_new_ones_out() {
        let groups = Groups::default();
        let g = groups.get(members("a.db"));
        let hold = g.hold(&W).unwrap();
        let frozen = Arc::new(AtomicBool::new(false));
        let freezer = {
            let g = g.clone();
            let frozen = frozen.clone();
            thread::spawn(move || {
                let f = g.gate().freeze();
                frozen.store(true, Ordering::SeqCst);
                thread::sleep(Duration::from_millis(50));
                frozen.store(false, Ordering::SeqCst);
                drop(f);
            })
        };
        thread::sleep(Duration::from_millis(50));
        assert!(
            !frozen.load(Ordering::SeqCst),
            "the hold keeps the freeze out"
        );
        hold.changed();
        drop(hold);
        // Wait for the freeze to begin; a new hold then waits for its end.
        while !frozen.load(Ordering::SeqCst) {
            thread::yield_now();
        }
        let hold = g.hold(&W).unwrap();
        assert!(
            !frozen.load(Ordering::SeqCst),
            "the freeze kept the hold out"
        );
        drop(hold);
        freezer.join().unwrap();
        assert!(g.state().is_dirty());
    }

    #[test]
    fn a_dirty_group_refuses_another_entry_point_until_captured() {
        use agentfs_core::record::SandboxId;
        let groups = Groups::default();
        let g = groups.get(members("a.db"));
        let sandbox = Writer {
            caller: C,
            entry: EntryPoint::Sandbox(SandboxId::new("x").unwrap()),
        };
        g.hold(&W).unwrap().changed();
        assert_eq!(g.hold(&sandbox).err(), Some(Foreign { pending: W }));
        // The same entry point holds it as before.
        g.hold(&W).unwrap().changed();
        let generation = g.state().generation;
        g.captured(generation, true, false);
        g.hold(&sandbox).unwrap().changed();
        assert_eq!(g.state().writer, Some(sandbox.clone()));
        assert_eq!(g.hold(&W).err(), Some(Foreign { pending: sandbox }));
    }

    #[test]
    fn holders_share_one_entry_point() {
        use agentfs_core::record::SandboxId;
        let groups = Groups::default();
        let g = groups.get(members("a.db"));
        let sandbox = Writer {
            caller: C,
            entry: EntryPoint::Sandbox(SandboxId::new("x").unwrap()),
        };
        let main = g.hold(&W).unwrap();
        let entered = Arc::new(AtomicBool::new(false));
        let other = {
            let g = g.clone();
            let entered = entered.clone();
            thread::spawn(move || {
                // Clean group: no cut, only a wait for main's hold.
                let hold = g.hold(&sandbox).map(|_| ());
                entered.store(true, Ordering::SeqCst);
                hold
            })
        };
        thread::sleep(Duration::from_millis(50));
        assert!(!entered.load(Ordering::SeqCst), "main's hold keeps it out");
        drop(main);
        assert_eq!(other.join().unwrap(), Ok(()));
    }

    #[test]
    fn the_timer_wakes_on_a_first_change_and_on_stop() {
        let groups = Arc::new(Groups::default());
        let g = groups.get(members("a.db"));
        let waiter = {
            let groups = groups.clone();
            thread::spawn(move || groups.wait(None))
        };
        thread::sleep(Duration::from_millis(20));
        g.hold(&W).unwrap().changed();
        assert!(waiter.join().unwrap());
        // Already dirty: no kick, so the deadline ends the wait.
        g.hold(&W).unwrap().changed();
        assert!(groups.wait(Some(Instant::now() + Duration::from_millis(10))));
        groups.stop();
        assert!(!groups.wait(None));
    }

    #[test]
    fn a_running_operation_holds_its_groups_back_from_the_timer() {
        let running = Arc::new(AtomicBool::new(true));
        let groups = {
            let running = running.clone();
            Groups::new(Arc::new(move |pid| {
                pid == 7 && running.load(Ordering::SeqCst)
            }))
        };
        let a = groups.get(members("a.db"));
        let b = groups.get(members("b.db"));
        let actor = Caller { pid: 7, ..C };
        a.hold(&Writer { caller: actor, ..W }).unwrap().changed();
        // The actor's change is agentfs's own.
        assert_eq!(a.state().writer, Some(Writer::agentfs(actor)));
        b.hold(&W).unwrap().changed();
        assert_eq!(a.deferred(), Some(actor));
        assert_eq!(b.deferred(), None);
        let far = Instant::now() + 60 * S;
        // Only b is the timer's; a is the operation's.
        assert_eq!(groups.due(far).len(), 1);
        assert!(Arc::ptr_eq(&groups.due(far)[0], &b));
        let ops = groups.in_operation();
        assert_eq!(ops.len(), 1);
        assert!(Arc::ptr_eq(&ops[0].0, &a));
        // Once the operation ends, a left changed is the timer's again.
        running.store(false, Ordering::SeqCst);
        assert_eq!(a.deferred(), None);
        assert_eq!(groups.due(far).len(), 2);
        assert!(groups.in_operation().is_empty());
    }
}
