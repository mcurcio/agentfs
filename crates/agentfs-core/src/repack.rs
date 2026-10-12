//! Periodic repack of the history (design §8).
//!
//! Every commit writes its new blobs as loose objects, and a database in
//! the fsync class is captured whole every `agentfs.coalesce`: without
//! repacking, each version costs its full size on disk. Every
//! `agentfs.repackInterval` the commit loop ([`crate::service`]) runs
//!
//! ```text
//! git -c repack.writeBitmaps=false repack -d -q --geometric=2
//! ```
//!
//! with the git CLI on the state area's git directory: loose objects and
//! small packs are rolled into geometrically growing packs, with deltas
//! between versions, and the redundant ones deleted. It never prunes
//! unreachable objects, so nothing a ref or the journal still needs can
//! go. The CLI reads only the repository's own config, as libgit2 does
//! (design §4.4): system and global config are switched off.
//!
//! A repack runs on the commit thread, between commits, so it never races
//! the committer; afterwards the committer's object database is refreshed
//! so it sees the new packs. It runs only while no push is in progress (it
//! takes every remote's push lock, without waiting), so a push never reads
//! a pack being deleted; when a push holds a lock, the repack waits for
//! the next round.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// The arguments after `git`.
pub const REPACK_ARGS: [&str; 6] = [
    "-c",
    "repack.writeBitmaps=false",
    "repack",
    "-d",
    "-q",
    "--geometric=2",
];

#[derive(Debug, thiserror::Error)]
pub enum RepackError {
    #[error("running git: {0}")]
    Spawn(#[source] io::Error),
    #[error("git repack exited with {status}: {stderr}")]
    Failed { status: String, stderr: String },
    #[error("locking {path}: {source}")]
    Lock {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

/// What one [`repack`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepackOutcome {
    /// The repack ran, taking this long.
    Done(Duration),
    /// A push held its lock: nothing was done.
    PushInProgress,
}

/// When the commit loop repacks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RepackSchedule {
    /// From the start of the loop to the first repack.
    pub first: Duration,
    /// Between repacks.
    pub interval: Duration,
}

impl RepackSchedule {
    /// The first repack one `interval` after start, then every `interval`.
    pub fn every(interval: Duration) -> Self {
        Self {
            first: interval,
            interval,
        }
    }
}

/// Repacks the repository at `git_dir` (a bare repository) with the git
/// CLI `git`, holding each of `push_locks` (without waiting) meanwhile.
pub fn repack(
    git: &Path,
    git_dir: &Path,
    push_locks: &[PathBuf],
) -> Result<RepackOutcome, RepackError> {
    let mut held = Vec::with_capacity(push_locks.len());
    for path in push_locks {
        match try_lock(path) {
            Ok(Some(file)) => held.push(file),
            Ok(None) => return Ok(RepackOutcome::PushInProgress),
            Err(source) => {
                return Err(RepackError::Lock {
                    path: path.clone(),
                    source,
                });
            }
        }
    }
    let start = Instant::now();
    let out = Command::new(git)
        .args(REPACK_ARGS)
        .env("GIT_DIR", git_dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .current_dir(git_dir)
        .output()
        .map_err(RepackError::Spawn)?;
    drop(held);
    if !out.status.success() {
        return Err(RepackError::Failed {
            status: out.status.to_string(),
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_owned(),
        });
    }
    Ok(RepackOutcome::Done(start.elapsed()))
}

/// The push lock at `path`, unless another holder has it.
fn try_lock(path: &Path) -> io::Result<Option<File>> {
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)?;
    match rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Ok(Some(file)),
        Err(rustix::io::Errno::WOULDBLOCK) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::volume::{self, StateDir};

    fn git_available() -> bool {
        Command::new("git").arg("--version").output().is_ok()
    }

    fn loose_objects(git_dir: &Path) -> usize {
        std::fs::read_dir(git_dir.join("objects"))
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().len() == 2)
            .map(|e| std::fs::read_dir(e.path()).unwrap().count())
            .sum()
    }

    #[test]
    fn repack_packs_loose_objects_and_keeps_history() {
        if !git_available() {
            eprintln!("git is not installed: skipped");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(dir.path().join("state"));
        volume::init(&state, "vol", None).unwrap();
        let repo = git2::Repository::open_bare(state.git()).unwrap();
        let blob = repo.blob(b"hello").unwrap();
        let mut tree = repo.treebuilder(None).unwrap();
        tree.insert("a", blob, 0o100644).unwrap();
        let tree = repo.find_tree(tree.write().unwrap()).unwrap();
        let sig = git2::Signature::now("t", "t@example.com").unwrap();
        let commit = repo
            .commit(Some("refs/heads/agentfs/vol"), &sig, &sig, "c", &tree, &[])
            .unwrap();
        assert!(loose_objects(&state.git()) >= 3);

        let lock = dir.path().join("push.lock");
        let outcome = repack(Path::new("git"), &state.git(), std::slice::from_ref(&lock)).unwrap();
        assert!(matches!(outcome, RepackOutcome::Done(_)), "{outcome:?}");
        assert_eq!(loose_objects(&state.git()), 0);
        // The same handle finds the objects once its odb is refreshed.
        repo.odb().unwrap().refresh().unwrap();
        assert_eq!(repo.find_commit(commit).unwrap().tree_id(), tree.id());

        // A push in progress: nothing is done.
        let held = try_lock(&lock).unwrap().unwrap();
        let outcome = repack(Path::new("git"), &state.git(), std::slice::from_ref(&lock)).unwrap();
        assert_eq!(outcome, RepackOutcome::PushInProgress);
        drop(held);
    }

    #[test]
    fn a_missing_git_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let err = repack(Path::new("/nonexistent/git"), dir.path(), &[]).unwrap_err();
        assert!(matches!(err, RepackError::Spawn(_)), "{err}");
    }

    #[test]
    fn a_failing_repack_reports_stderr() {
        if !git_available() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let err = repack(Path::new("git"), &dir.path().join("nope"), &[]).unwrap_err();
        assert!(
            matches!(err, RepackError::Failed { .. } | RepackError::Spawn(_)),
            "{err}"
        );
    }
}
