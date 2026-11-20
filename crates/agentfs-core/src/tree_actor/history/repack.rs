//! The tree history's repack, a task of its own (docs/trees.md, "History
//! trails the files"). Each batch the builder installs is one pack, and a
//! read's cost grows with the pack count, so at each built event the task
//! runs `git repack --geometric=2 -d` in the state area's git directory, one
//! at a time, with the memory bounds of the recording engine's repack
//! ([`crate::repack::REPACK_ARGS`]). It runs beside `git fast-import`, as a
//! git server repacks beside receive-pack, never under the builder's lease
//! and never stopping it. Geometric repack packs whole packs, keeping every
//! object, reachable or not; nothing prunes and no ref is touched. Events
//! that come while a repack runs are one more run after it, not one each.
//! A stop kills a repack in progress: git writes new packs before it deletes
//! old ones, so nothing is lost.

#![expect(
    dead_code,
    reason = "frozen interface (agentfs-ln6.5): the repack task"
)]

use std::io;
use std::path::PathBuf;

use super::built::History;

/// The running repack task: its thread, which ends when History stops.
pub(in crate::tree_actor) struct Repacker {
    thread: std::thread::JoinHandle<()>,
}

impl Repacker {
    /// Starts the task on its own thread (`tree-repack`): it waits on
    /// History's built events ([`History::wait_epoch`]) and repacks
    /// `git_dir` after each, until History stops.
    ///
    /// # Errors
    ///
    /// The thread could not be started.
    pub(in crate::tree_actor) fn start(history: History, git_dir: PathBuf) -> io::Result<Self> {
        let _ = (history, git_dir);
        todo!("agentfs-ln6.5")
    }

    /// Waits for the task to end: History has stopped, and a repack in
    /// progress was killed.
    pub(in crate::tree_actor) fn join(self) {
        todo!("agentfs-ln6.5")
    }
}
