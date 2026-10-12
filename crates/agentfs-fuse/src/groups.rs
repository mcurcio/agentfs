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
//! - at the close of a member, the release of the group's last handle, and
//!   unmount, whenever it changed.
//!
//! A capture that fails leaves the group changed: the timer retries it one
//! interval later. A capture the recorder refuses (a member over a limit)
//! counts as done, as a refused close does for an ordinary file.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use agentfs_core::fsync_class::{GroupKey, GroupMembers};
use agentfs_core::record::Caller;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

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
    freezers_waiting: usize,
    frozen: bool,
}

impl Gate {
    fn hold(&self) -> HoldGuard<'_> {
        let mut s = lock(&self.state);
        while s.frozen || s.freezers_waiting > 0 {
            s = self.cv.wait(s).unwrap_or_else(PoisonError::into_inner);
        }
        s.holders += 1;
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
    _guard: HoldGuard<'a>,
}

impl Hold<'_> {
    /// Marks the group changed by `caller`, inside the hold, so a capture
    /// that starts after the hold ends sees both the change and the mark.
    pub fn changed(&self, caller: Caller) {
        self.group.changed(caller);
    }
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
    /// Who changed the group last.
    pub caller: Option<Caller>,
    /// Handles open on members.
    pub handles: usize,
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

    fn changed(&mut self, now: Instant, caller: Caller) {
        self.dirty_since.get_or_insert(now);
        self.generation += 1;
        self.caller = Some(caller);
    }

    /// A capture that froze the group at `generation` ended at `now`;
    /// `done` if it was recorded (or refused). The group is clean unless it
    /// changed since.
    pub fn captured(&mut self, generation: u64, now: Instant, done: bool) {
        self.last_attempt = Some(now);
        if done && self.generation == generation {
            self.dirty_since = None;
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
    /// One capture at a time.
    capturing: Mutex<()>,
    kick: Arc<Kick>,
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

    /// Starts a write to a member: waits while the group is being
    /// captured.
    pub fn hold(&self) -> Hold<'_> {
        Hold {
            group: self,
            _guard: self.gate.hold(),
        }
    }

    /// Marks the group changed by `caller`. Prefer [`Hold::changed`]; this
    /// is for changes made with no hold (a rename, which `names` orders
    /// against captures).
    pub fn changed(&self, caller: Caller) {
        let was_clean = {
            let mut s = self.state();
            let was_clean = !s.is_dirty();
            s.changed(Instant::now(), caller);
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
#[derive(Default)]
pub struct Groups {
    map: Mutex<HashMap<GroupKey, Arc<Group>>>,
    kick: Arc<Kick>,
}

impl Groups {
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
                    capturing: Mutex::new(()),
                    kick: self.kick.clone(),
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

    /// The groups the timer captures at `now`.
    pub fn due(&self, now: Instant) -> Vec<Arc<Group>> {
        lock(&self.map)
            .values()
            .filter(|g| g.state().due_at(g.coalesce).is_some_and(|at| at <= now))
            .cloned()
            .collect()
    }

    /// When the next group is due, if any is dirty.
    pub fn next_due(&self) -> Option<Instant> {
        lock(&self.map)
            .values()
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
        s.changed(t0, C);
        // Never attempted: an fsync captures at once, the timer later.
        assert!(s.fsync_due(t0, 5 * S));
        assert_eq!(s.due_at(5 * S), Some(t0 + 5 * S));
        // Later changes do not push the timer back.
        s.changed(t0 + 3 * S, C);
        assert_eq!(s.due_at(5 * S), Some(t0 + 5 * S));
        let generation = s.generation;
        s.captured(generation, t0 + 3 * S, true);
        assert!(!s.is_dirty());
        // A change right after a capture: fsync waits out the interval,
        // and so does the timer.
        s.changed(t0 + 4 * S, C);
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
        s.changed(t0 + 11 * S, C);
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
        b.changed(C);
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
        let hold = g.hold();
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
        hold.changed(C);
        drop(hold);
        // Wait for the freeze to begin; a new hold then waits for its end.
        while !frozen.load(Ordering::SeqCst) {
            thread::yield_now();
        }
        let hold = g.hold();
        assert!(
            !frozen.load(Ordering::SeqCst),
            "the freeze kept the hold out"
        );
        drop(hold);
        freezer.join().unwrap();
        assert!(g.state().is_dirty());
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
        g.changed(C);
        assert!(waiter.join().unwrap());
        // Already dirty: no kick, so the deadline ends the wait.
        g.changed(C);
        assert!(groups.wait(Some(Instant::now() + Duration::from_millis(10))));
        groups.stop();
        assert!(!groups.wait(None));
    }
}
