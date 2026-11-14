//! The lag gate's arithmetic, pure: each tree's lag (the builder's estimated
//! work for its steps past the built point), the builder's measured rates and
//! health, and whether a tree's new work may start (docs/trees.md, "History
//! trails the files"). History holds it under its lock and waits on its
//! events; nothing here waits or reads a clock but the caller's `now`.

use std::collections::{BTreeMap, VecDeque};
use std::time::Duration;

use super::super::TreeId;
use super::super::journal::durable::RecordNo;
use super::built::{Health, TreeLag};

/// Over this, `tree HistoryLagging` is logged once (a placeholder, one home).
pub(in crate::tree_actor) const LAG_ALARM: Duration = Duration::from_secs(30);
/// Over this, a tree's calls that start new work wait for the builder.
pub(in crate::tree_actor) const LAG_LIMIT: Duration = Duration::from_secs(120);
/// The builder's rates before it has measured a batch.
const INITIAL_NS_PER_STEP: u64 = 5_000_000;
const INITIAL_NS_PER_BYTE: u64 = 10;

/// Each tree's unbuilt steps and the bytes each adds, the builder's last
/// measured rates, and its health.
pub(in crate::tree_actor) struct LagGate {
    unbuilt: BTreeMap<TreeId, VecDeque<(RecordNo, u64)>>,
    ns_per_step: u64,
    ns_per_byte: u64,
    limit: Duration,
    alarm: Duration,
    failed: bool,
    /// The journal's compaction is overdue: every tree's new work waits
    /// until no step is unbuilt.
    overdue: bool,
    /// The last health logged, so each entry into a state logs once.
    logged: Health,
}

impl Default for LagGate {
    fn default() -> Self {
        Self::with_limits(LAG_ALARM, LAG_LIMIT)
    }
}

impl LagGate {
    /// A gate with these levels: the alarm and the limit.
    pub(in crate::tree_actor) fn with_limits(alarm: Duration, limit: Duration) -> Self {
        Self {
            unbuilt: BTreeMap::new(),
            ns_per_step: INITIAL_NS_PER_STEP,
            ns_per_byte: INITIAL_NS_PER_BYTE,
            limit,
            alarm,
            failed: false,
            overdue: false,
            logged: Health::Current,
        }
    }

    /// Sets whether the journal's compaction is overdue; true when that
    /// changed.
    pub(in crate::tree_actor) fn compaction_overdue(&mut self, overdue: bool) -> bool {
        std::mem::replace(&mut self.overdue, overdue) != overdue
    }

    /// The actor appended `step` in `tree`, which adds about `bytes` for the
    /// builder to write.
    pub(in crate::tree_actor) fn appended(&mut self, tree: TreeId, step: RecordNo, bytes: u64) {
        self.unbuilt
            .entry(tree)
            .or_default()
            .push_back((step, bytes));
        self.log();
    }

    /// A batch through `through` is built in `elapsed`. Each rate is measured
    /// as if it bore the whole batch: its time over its steps, and over the
    /// bytes they added. One batch cannot split its time between the two, and
    /// [`LagGate::lag`] takes the larger estimate, so a cost that grows with
    /// the tree (its listings) is never read as a fixed cost per step.
    pub(in crate::tree_actor) fn built(&mut self, through: RecordNo, elapsed: Duration) {
        let (mut steps, mut bytes) = (0_u64, 0_u64);
        for queue in self.unbuilt.values_mut() {
            while let Some((_, added)) = queue.front().copied().filter(|(step, _)| *step <= through)
            {
                queue.pop_front();
                steps += 1;
                bytes = bytes.saturating_add(added);
            }
        }
        self.unbuilt.retain(|_, queue| !queue.is_empty());
        let elapsed = u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX);
        if let Some(per_step) = elapsed.checked_div(steps) {
            self.ns_per_step = per_step.max(1);
        }
        if let Some(per_byte) = elapsed.checked_div(bytes) {
            self.ns_per_byte = per_byte.max(1);
        }
        self.failed = false;
        self.log();
    }

    /// A batch failed: the gate holds nothing while the builder has failed.
    pub(in crate::tree_actor) fn failed(&mut self) {
        self.failed = true;
        self.logged = Health::Current;
    }

    /// `tree`'s lag: its unbuilt steps' estimated build time, the larger of
    /// their count at the step rate and their bytes at the byte rate.
    pub(in crate::tree_actor) fn lag(&self, tree: TreeId) -> Duration {
        let Some(queue) = self.unbuilt.get(&tree) else {
            return Duration::ZERO;
        };
        let bytes: u64 = queue.iter().map(|(_, bytes)| *bytes).sum();
        let nanos = (queue.len() as u64)
            .saturating_mul(self.ns_per_step)
            .max(bytes.saturating_mul(self.ns_per_byte));
        Duration::from_nanos(nanos)
    }

    /// Whether `tree`'s new work may start now: its lag is under the limit
    /// and no overdue compaction waits for the builder, or the builder has
    /// failed (a failed builder holds nothing back).
    pub(in crate::tree_actor) fn admits(&self, tree: TreeId) -> bool {
        self.failed || (self.lag(tree) <= self.limit && !(self.overdue && !self.unbuilt.is_empty()))
    }

    /// Every tree's lag, worst first.
    pub(in crate::tree_actor) fn trees(&self) -> Vec<TreeLag> {
        let mut trees: Vec<TreeLag> = self
            .unbuilt
            .iter()
            .map(|(tree, queue)| TreeLag {
                tree: *tree,
                seconds: self.lag(*tree).as_secs_f64(),
                steps: queue.len() as u64,
            })
            .collect();
        trees.sort_by(|a, b| b.seconds.total_cmp(&a.seconds));
        trees
    }

    /// The health the lags give, failure aside: stalled over the limit,
    /// lagging over the alarm level.
    pub(in crate::tree_actor) fn health(&self) -> Health {
        let worst = self
            .unbuilt
            .keys()
            .map(|tree| self.lag(*tree))
            .max()
            .unwrap_or_default();
        if worst > self.limit {
            Health::Stalled
        } else if worst > self.alarm {
            Health::Lagging
        } else {
            Health::Current
        }
    }

    fn log(&mut self) {
        let health = self.health();
        if health != self.logged {
            match health {
                Health::Lagging => eprintln!("tree HistoryLagging"),
                Health::Stalled => eprintln!("tree HistoryStalled"),
                Health::Current | Health::Failed(_) => {}
            }
            self.logged = health;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: TreeId = TreeId::Root;
    const B: TreeId = TreeId::Fork(1);

    fn step(n: u64) -> RecordNo {
        RecordNo::new(n)
    }

    #[test]
    fn the_gate_holds_only_its_tree() {
        let mut gate = LagGate::with_limits(Duration::from_millis(5), Duration::from_millis(10));
        for n in 1..=3 {
            gate.appended(A, step(n), 0);
        }
        assert_eq!(gate.lag(A), Duration::from_millis(15));
        assert!(!gate.admits(A), "A is over its limit");
        assert!(gate.admits(B), "B has no unbuilt step");
        assert_eq!(gate.health(), Health::Stalled);
        gate.built(step(2), Duration::from_millis(2));
        assert_eq!(
            gate.lag(A),
            Duration::from_millis(1),
            "one step at the measured rate"
        );
        assert!(gate.admits(A));
        assert_eq!(gate.health(), Health::Current);
    }

    #[test]
    fn a_failed_builder_holds_nothing_back() {
        let mut gate = LagGate::with_limits(Duration::from_millis(5), Duration::from_millis(10));
        for n in 1..=5 {
            gate.appended(A, step(n), 0);
        }
        assert!(!gate.admits(A));
        gate.failed();
        assert!(gate.admits(A));
        gate.built(step(5), Duration::from_millis(5));
        assert!(gate.admits(A));
        assert!(gate.trees().is_empty());
    }

    #[test]
    fn an_overdue_compaction_holds_every_tree_until_steps_are_built() {
        let mut gate = LagGate::default();
        gate.appended(A, step(1), 0);
        assert!(gate.admits(A) && gate.admits(B));
        assert!(gate.compaction_overdue(true));
        assert!(!gate.admits(A) && !gate.admits(B), "the journal is shared");
        gate.built(step(1), Duration::from_millis(1));
        assert!(gate.admits(A) && gate.admits(B), "every step is built");
        gate.appended(B, step(2), 0);
        assert!(!gate.admits(A));
        assert!(gate.compaction_overdue(false));
        assert!(gate.admits(A));
    }

    #[test]
    fn bytes_count_at_the_byte_rate() {
        let mut gate = LagGate::default();
        gate.appended(A, step(1), 1_000_000);
        assert_eq!(
            gate.lag(A),
            Duration::from_nanos(INITIAL_NS_PER_STEP.max(1_000_000 * INITIAL_NS_PER_BYTE))
        );
        assert_eq!(gate.trees()[0].steps, 1);
    }

    /// The builder's cost per step grows with the tree's listings until the
    /// commit layout costs only its change. A batch of small-tree steps sets
    /// the byte rate; a step in a tree ten times as large is estimated at ten
    /// times the cost, not at the small tree's cost per step.
    #[test]
    fn a_cost_that_grows_with_the_tree_is_estimated_by_its_bytes() {
        let mut gate = LagGate::with_limits(Duration::from_secs(30), Duration::from_secs(120));
        for n in 1..=100 {
            gate.appended(A, step(n), 1_000);
        }
        gate.built(step(100), Duration::from_millis(100));
        for n in 101..=200 {
            gate.appended(A, step(n), 10_000);
        }
        assert_eq!(gate.lag(A), Duration::from_millis(1_000));
    }
}
