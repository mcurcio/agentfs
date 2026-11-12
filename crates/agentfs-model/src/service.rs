//! The model's history service, its composition root: the trees ([`World`])
//! and one mirror ([`Mirror`]), composed, with the step list over both
//! (`docs/target-architecture/future/module-cut.md`). It computes nothing of
//! its own: the trees take their steps, the mirror its push, and the service
//! schedules the push after the trees' steps, when the mirror is due. It writes
//! the run's digest, which reads the mirror too.

use std::ops::{Deref, DerefMut};
use std::path::Path;

use crate::calls::TreeId;
use crate::mirror::Mirror;
use crate::world::{Policies, TreeNames, TreeStep, World};

/// A step the scheduler can take: the trees' ([`TreeStep`]) or the mirror's
/// push.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Step {
    /// [`TreeStep::Next`].
    Next(TreeId),
    /// [`TreeStep::Drop`].
    Drop(TreeId),
    /// The mirror's push ([`Mirror::push`]): due once the root moved or a take
    /// recorded a collision since its last push, taken in any order with the
    /// trees' steps, so the push is eventual and never part of a take.
    Push,
}

impl From<TreeStep> for Step {
    fn from(s: TreeStep) -> Self {
        match s {
            TreeStep::Next(t) => Step::Next(t),
            TreeStep::Drop(t) => Step::Drop(t),
        }
    }
}

/// The trees and their mirror. It derefs to the [`World`]; no method name is
/// on both, so code holding a `&World` cannot call the service's.
pub struct HistoryService {
    world: World,
    mirror: Mirror,
}

impl HistoryService {
    pub fn new(policies: Policies, names: TreeNames) -> Self {
        let mirror = Mirror::new(&names);
        Self {
            world: World::new(policies, names),
            mirror,
        }
    }

    pub fn mirror(&self) -> &Mirror {
        &self.mirror
    }

    /// Every step that can act now: the trees' in their order, then the
    /// mirror's push when it is due.
    pub fn pending(&self) -> Vec<Step> {
        let mut out: Vec<Step> = self
            .world
            .tree_steps()
            .into_iter()
            .map(Step::from)
            .collect();
        if self.mirror.due(self.world.moves()) {
            out.push(Step::Push);
        }
        out
    }

    /// Takes one step and returns how many queue entries it took (none for a
    /// drop or a push).
    pub fn step(&mut self, s: &Step) -> usize {
        match s {
            Step::Next(t) => self.world.take_step(&TreeStep::Next(*t)),
            Step::Drop(t) => self.world.take_step(&TreeStep::Drop(*t)),
            Step::Push => {
                self.mirror.push(self.world.repo(), self.world.moves());
                0
            }
        }
    }

    /// Takes steps until none is left.
    pub fn settle(&mut self) {
        for _ in 0..100_000 {
            let Some(s) = self.pending().into_iter().next() else {
                return;
            };
            self.world.note_step(&s);
            self.step(&s);
        }
        panic!("the queues never drained");
    }
}

impl Deref for HistoryService {
    type Target = World;
    fn deref(&self) -> &World {
        &self.world
    }
}

impl DerefMut for HistoryService {
    fn deref_mut(&mut self) -> &mut World {
        &mut self.world
    }
}

/// Writes the run's digest, before the world and the mirror drop.
impl Drop for HistoryService {
    fn drop(&mut self) {
        if std::thread::panicking() {
            return;
        }
        if let Some(dir) = std::env::var_os(crate::digest::DIGEST_DIR) {
            crate::digest::write(self, Path::new(&dir), &self.world.run_name());
        }
    }
}
