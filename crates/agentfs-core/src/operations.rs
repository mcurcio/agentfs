//! Operator operations in progress: restores and reverts (design §15).
//!
//! A restore writes into the tree like any workload, so its writes reach
//! the recorder as ordinary change boundaries: from the CLI's own process
//! in direct mode, or through the FUSE layer from the daemon's restore
//! thread in mount mode. The process doing the writing registers itself
//! here as an **actor** (FUSE reports the calling thread's id as the pid)
//! for as long as the operation runs, and [`crate::volume::Recorder`]
//! turns every record from an actor into part of the operation: trigger
//! `restore`, the operator as caller, and the [`Operation`] in the
//! journal record, so the committer gives the operation its own commit.
//!
//! [`OperationGuard`] deregisters the actors when it is finished or
//! dropped; until then the committer holds the operation's trailing group
//! open, so a slow restore is still one commit.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use crate::record::Operation;

/// The registry of running operations, shared by a volume's recorder and
/// committer.
#[derive(Debug, Default)]
pub struct Operations {
    state: RwLock<State>,
    /// How many operations are running: lets the recorder skip the lock
    /// when none is (the common case).
    running: AtomicUsize,
    /// The last operation id given out.
    last_id: AtomicU64,
}

#[derive(Debug, Default)]
struct State {
    /// Actor (a pid or thread id) to its operation.
    actors: HashMap<u32, Arc<Active>>,
    /// Ids of the running operations.
    ids: HashSet<u64>,
}

#[derive(Debug)]
struct Active {
    op: Operation,
    /// The last journal sequence number recorded for the operation.
    last_seq: Mutex<Option<u64>>,
}

/// An actor is already part of a running operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("process or thread {0} is already part of a running operation")]
pub struct ActorBusy(pub u32);

impl Operations {
    /// A fresh operation id: the time in milliseconds, or one more than
    /// the last id if the clock has not moved on, so ids grow within a
    /// process and rarely repeat across processes.
    pub fn next_id(&self, now_ms: u64) -> u64 {
        let mut last = self.last_id.load(Ordering::SeqCst);
        loop {
            let id = now_ms.max(last + 1);
            match self
                .last_id
                .compare_exchange(last, id, Ordering::SeqCst, Ordering::SeqCst)
            {
                Ok(_) => return id,
                Err(seen) => last = seen,
            }
        }
    }

    /// Registers `op` for `actors` until the returned guard is finished or
    /// dropped. Fails, registering nothing, if an actor is already part of
    /// another operation.
    pub fn begin(
        self: &Arc<Self>,
        op: Operation,
        actors: &[u32],
    ) -> Result<OperationGuard, ActorBusy> {
        let mut state = self.write();
        if let Some(busy) = actors.iter().find(|a| state.actors.contains_key(a)) {
            return Err(ActorBusy(*busy));
        }
        let active = Arc::new(Active {
            op,
            last_seq: Mutex::new(None),
        });
        for actor in actors {
            state.actors.insert(*actor, active.clone());
        }
        state.ids.insert(active.op.id);
        self.running.fetch_add(1, Ordering::SeqCst);
        Ok(OperationGuard {
            operations: self.clone(),
            active,
            actors: actors.to_vec(),
            done: false,
        })
    }

    /// The operation `actor` is part of, if any.
    pub fn of_actor(&self, actor: u32) -> Option<Operation> {
        if self.running.load(Ordering::SeqCst) == 0 {
            return None;
        }
        self.read().actors.get(&actor).map(|a| a.op.clone())
    }

    /// Notes that `seq` was journaled for `actor`.s operation.
    pub fn recorded(&self, actor: u32, seq: u64) {
        if self.running.load(Ordering::SeqCst) == 0 {
            return;
        }
        if let Some(active) = self.read().actors.get(&actor) {
            let mut last = active
                .last_seq
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *last = Some(last.map_or(seq, |l| l.max(seq)));
        }
    }

    /// Whether the operation `id` is still running.
    pub fn is_active(&self, id: u64) -> bool {
        self.running.load(Ordering::SeqCst) > 0 && self.read().ids.contains(&id)
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, State> {
        self.state
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, State> {
        self.state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// A running operation. Its actors' records belong to it until
/// [`OperationGuard::finish`] or drop.
#[derive(Debug)]
pub struct OperationGuard {
    operations: Arc<Operations>,
    active: Arc<Active>,
    actors: Vec<u32>,
    done: bool,
}

impl OperationGuard {
    pub fn operation(&self) -> &Operation {
        &self.active.op
    }

    /// The last sequence number recorded for the operation so far.
    pub fn last_seq(&self) -> Option<u64> {
        *self
            .active
            .last_seq
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Ends the operation and returns the last sequence number recorded
    /// for it, if anything was.
    pub fn finish(mut self) -> Option<u64> {
        self.end();
        self.last_seq()
    }

    fn end(&mut self) {
        if self.done {
            return;
        }
        self.done = true;
        let mut state = self.operations.write();
        for actor in &self.actors {
            if state
                .actors
                .get(actor)
                .is_some_and(|a| Arc::ptr_eq(a, &self.active))
            {
                state.actors.remove(actor);
            }
        }
        state.ids.remove(&self.active.op.id);
        self.operations.running.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Drop for OperationGuard {
    fn drop(&mut self) {
        self.end();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::{Caller, OperationKind};

    fn op(id: u64) -> Operation {
        Operation {
            id,
            kind: OperationKind::Restore,
            source: "abc".into(),
            summary: "restore x from abc".into(),
            caller: Caller {
                uid: 1,
                gid: 1,
                pid: 99,
            },
        }
    }

    #[test]
    fn actors_belong_to_the_operation_until_it_ends() {
        let ops = Arc::new(Operations::default());
        assert_eq!(ops.of_actor(7), None);
        let guard = ops.begin(op(1), &[7, 8]).unwrap();
        assert_eq!(ops.of_actor(7).map(|o| o.id), Some(1));
        assert_eq!(ops.of_actor(8).map(|o| o.id), Some(1));
        assert_eq!(ops.of_actor(9), None);
        assert!(ops.is_active(1));
        assert_eq!(ops.begin(op(2), &[9, 8]).unwrap_err(), ActorBusy(8));
        assert_eq!(ops.of_actor(9), None, "nothing registered on failure");
        assert!(!ops.is_active(2));

        assert_eq!(guard.last_seq(), None);
        ops.recorded(7, 4);
        ops.recorded(8, 6);
        ops.recorded(7, 5);
        ops.recorded(9, 100);
        assert_eq!(guard.finish(), Some(6));
        assert_eq!(ops.of_actor(7), None);
        assert!(!ops.is_active(1));

        let guard = ops.begin(op(3), &[7]).unwrap();
        drop(guard);
        assert_eq!(ops.of_actor(7), None, "drop ends it too");
        assert!(!ops.is_active(3));
    }

    #[test]
    fn ids_grow() {
        let ops = Operations::default();
        let a = ops.next_id(1000);
        let b = ops.next_id(1000);
        let c = ops.next_id(500);
        let d = ops.next_id(5000);
        assert!(a < b && b < c && c < d, "{a} {b} {c} {d}");
        assert_eq!(d, 5000);
    }
}
