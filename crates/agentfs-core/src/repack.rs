//! Periodic repack of the history (design §8).
//!
//! Every commit writes its new blobs as loose objects, and a database in
//! the fsync class is captured whole every `agentfs.coalesce`: without
//! repacking, each version costs its full size on disk. Every
//! `agentfs.repackInterval` the commit loop ([`crate::service`]) runs
//!
//! ```text
//! git -c repack.writeBitmaps=false -c pack.threads=1 \
//!     -c pack.windowMemory=64m -c pack.deltaCacheSize=16m \
//!     -c core.bigFileThreshold=64m -c core.packedGitLimit=128m \
//!     repack -d -q --geometric=2
//! ```
//!
//! with the git CLI on the state area's git directory: loose objects and
//! small packs are rolled into geometrically growing packs, with deltas
//! between versions, and the redundant ones deleted. It never prunes
//! unreachable objects, so nothing a ref or the journal still needs can
//! go. A round with no loose objects does nothing.
//!
//! **Memory.** git's defaults (one thread per CPU, an unbounded delta
//! window, deltas for blobs up to 512 MiB) can need gigabytes for a large
//! database, inside the sidecar's memory limit. The settings above bound
//! it: one thread, at most 64 MiB of delta window, and blobs over 64 MiB
//! (a large database) stored whole, compressed, never deltified. A repack
//! then needs up to about 256 MiB besides the daemon.
//!
//! **Environment.** The child starts from an empty environment: a fixed
//! `PATH`, `LC_ALL=C`, `HOME` set to the git directory, and the system and
//! global git config switched off, so the CLI reads only the repository's
//! own config, as libgit2 does (design §4.4), and nothing in the daemon's
//! environment (`GIT_CONFIG_*`, `GIT_EXEC_PATH`) reaches it.
//!
//! A repack runs on the commit thread, between commits, so it never races
//! the committer; afterwards the committer's object database is refreshed
//! so it sees the new packs. It runs only while no push is in progress (it
//! takes every remote's push lock, without waiting), so a push never reads
//! a pack being deleted; when a push holds a lock, the repack waits for
//! the next round. A stop (shutdown) kills a repack in progress: git
//! writes new packs before it deletes old ones, so a killed repack loses
//! nothing (it may leave a temporary file, which the next one ignores).

use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::service::Stop;

/// The arguments after `git`.
pub const REPACK_ARGS: [&str; 16] = [
    "-c",
    "repack.writeBitmaps=false",
    "-c",
    "pack.threads=1",
    "-c",
    "pack.windowMemory=64m",
    "-c",
    "pack.deltaCacheSize=16m",
    "-c",
    "core.bigFileThreshold=64m",
    "-c",
    "core.packedGitLimit=128m",
    "repack",
    "-d",
    "-q",
    "--geometric=2",
];

/// The `PATH` git is looked up in and runs with.
pub const REPACK_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// How often a running repack checks for a stop.
const POLL: Duration = Duration::from_millis(100);

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
    /// No loose objects: nothing to pack.
    NothingToDo,
    /// A stop came first: the repack was killed.
    Stopped,
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
/// Kills git if `stop` is set while it runs.
pub fn repack(
    git: &Path,
    git_dir: &Path,
    push_locks: &[PathBuf],
    stop: &Stop,
) -> Result<RepackOutcome, RepackError> {
    if loose_objects(git_dir) == 0 {
        return Ok(RepackOutcome::NothingToDo);
    }
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
    let mut child = command(git, git_dir).spawn().map_err(RepackError::Spawn)?;
    if let Err(e) = crate::failpoints::hit("repack.running") {
        let _ = child.kill();
        let _ = child.wait();
        return Err(RepackError::Spawn(e));
    }
    // Read stderr on the side, so a chatty git never blocks on the pipe.
    let stderr = child.stderr.take().map(|mut pipe| {
        std::thread::spawn(move || {
            let mut out = Vec::new();
            let _ = pipe.read_to_end(&mut out);
            out
        })
    });
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(RepackError::Spawn(e));
            }
        }
        if stop.wait(POLL) {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(RepackOutcome::Stopped);
        }
    };
    drop(held);
    let stderr = stderr
        .and_then(|t| t.join().ok())
        .map(|b| String::from_utf8_lossy(&b).trim().to_owned())
        .unwrap_or_default();
    if !status.success() {
        return Err(RepackError::Failed {
            status: status.to_string(),
            stderr,
        });
    }
    Ok(RepackOutcome::Done(start.elapsed()))
}

/// The git command, in an environment of its own (see the module docs).
fn command(git: &Path, git_dir: &Path) -> Command {
    let mut cmd = Command::new(git);
    cmd.args(REPACK_ARGS)
        .env_clear()
        .env("PATH", REPACK_PATH)
        .env("LC_ALL", "C")
        .env("HOME", git_dir)
        .env("GIT_DIR", git_dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .current_dir(git_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    cmd
}

/// How many loose objects `git_dir` holds (unreadable directories count
/// as none).
fn loose_objects(git_dir: &Path) -> usize {
    let Ok(dirs) = std::fs::read_dir(git_dir.join("objects")) else {
        return 0;
    };
    dirs.filter_map(Result::ok)
        .filter(|e| {
            let name = e.file_name();
            let name = name.as_encoded_bytes();
            name.len() == 2 && name.iter().all(u8::is_ascii_hexdigit)
        })
        .filter_map(|e| std::fs::read_dir(e.path()).ok())
        .map(Iterator::count)
        .sum()
}

/// The push lock at `path`, unless another holder (in this process too)
/// has it.
fn try_lock(path: &Path) -> io::Result<Option<crate::lockfile::HeldLock>> {
    crate::lockfile::HeldLock::try_exclusive(path, 0o600)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Arc;

    use super::*;
    use crate::volume::{self, StateDir};

    fn git_available() -> bool {
        Command::new("git").arg("--version").output().is_ok()
    }

    /// A git directory with one (fake) loose object.
    fn with_a_loose_object() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("objects/ab")).unwrap();
        std::fs::write(dir.path().join("objects/ab/cdef"), b"x").unwrap();
        dir
    }

    /// An executable script standing in for git.
    fn fake_git(dir: &Path, body: &str) -> PathBuf {
        let path = dir.join("fake-git");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// Runs `f`, again while a just-written script is busy (`ETXTBSY`: a
    /// process forked by another test still holds it open for writing).
    fn retry_busy(
        f: impl Fn() -> Result<RepackOutcome, RepackError>,
    ) -> Result<RepackOutcome, RepackError> {
        for _ in 0..50 {
            match f() {
                Err(RepackError::Spawn(e)) if e.raw_os_error() == Some(26) => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                other => return other,
            }
        }
        f()
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
        let stop = Stop::default();
        let lock = dir.path().join("push.lock");
        let run = || {
            repack(
                Path::new("git"),
                &state.git(),
                std::slice::from_ref(&lock),
                &stop,
            )
        };

        // A push in progress: nothing is done.
        let held = try_lock(&lock).unwrap().unwrap();
        assert_eq!(run().unwrap(), RepackOutcome::PushInProgress);
        drop(held);

        let outcome = run().unwrap();
        assert!(matches!(outcome, RepackOutcome::Done(_)), "{outcome:?}");
        assert_eq!(loose_objects(&state.git()), 0);
        // The same handle finds the objects once its odb is refreshed.
        repo.odb().unwrap().refresh().unwrap();
        assert_eq!(repo.find_commit(commit).unwrap().tree_id(), tree.id());
        // Nothing loose left: the next round does nothing.
        assert_eq!(run().unwrap(), RepackOutcome::NothingToDo);
    }

    /// git killed at every stage of a repack (as a crash or a stop would)
    /// loses nothing: the history stays whole and the next repack runs.
    #[cfg(feature = "failpoints")]
    #[test]
    fn a_killed_repack_loses_nothing() {
        if !git_available() {
            eprintln!("git is not installed: skipped");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(dir.path().join("state"));
        volume::init(&state, "vol", None).unwrap();
        let repo = git2::Repository::open_bare(state.git()).unwrap();
        let sig = git2::Signature::now("t", "t@example.com").unwrap();
        let mut parent: Option<git2::Oid> = None;
        for i in 0..20 {
            let blob = repo
                .blob(format!("version {i}\n").repeat(500).as_bytes())
                .unwrap();
            let mut tree = repo.treebuilder(None).unwrap();
            tree.insert("a", blob, 0o100644).unwrap();
            let tree = repo.find_tree(tree.write().unwrap()).unwrap();
            let parents: Vec<git2::Commit<'_>> = parent
                .map(|p| repo.find_commit(p).unwrap())
                .into_iter()
                .collect();
            let refs: Vec<&git2::Commit<'_>> = parents.iter().collect();
            parent = Some(
                repo.commit(
                    Some("refs/heads/agentfs/vol"),
                    &sig,
                    &sig,
                    "c",
                    &tree,
                    &refs,
                )
                .unwrap(),
            );
        }
        let run = |stop: &Stop| repack(Path::new("git"), &state.git(), &[], stop);
        let fsck = || {
            let out = Command::new("git")
                .args(["fsck", "--full", "--strict", "--no-progress"])
                .env("GIT_DIR", state.git())
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        // Killed at once (a crash right after the spawn).
        {
            let _guard = crate::failpoints::configure("repack.running=error").unwrap();
            let failed = run(&Stop::default());
            assert!(matches!(failed, Err(RepackError::Spawn(_))), "{failed:?}");
            fsck();
        }
        // Killed by a stop after 1, 5, 20 and 50 ms of work.
        for ms in [1, 5, 20, 50] {
            let stop = Stop::default();
            let outcome = std::thread::scope(|s| {
                s.spawn(|| {
                    std::thread::sleep(Duration::from_millis(ms));
                    stop.stop();
                });
                run(&stop)
            });
            assert!(outcome.is_ok(), "{ms} ms: {outcome:?}");
            fsck();
        }
        // A later round packs everything (unless a stopped one finished).
        let last = run(&Stop::default()).unwrap();
        assert!(
            matches!(last, RepackOutcome::Done(_) | RepackOutcome::NothingToDo),
            "{last:?}"
        );
        assert_eq!(loose_objects(&state.git()), 0);
        fsck();
        let head = repo.refname_to_id("refs/heads/agentfs/vol").unwrap();
        assert_eq!(Some(head), parent);
    }

    #[test]
    fn git_runs_bounded_in_an_environment_of_its_own() {
        let dir = with_a_loose_object();
        let git = fake_git(
            dir.path(),
            &format!(
                "echo \"$@\" > {0}/args; env > {0}/env",
                dir.path().display()
            ),
        );
        let outcome = retry_busy(|| repack(&git, dir.path(), &[], &Stop::default())).unwrap();
        assert!(matches!(outcome, RepackOutcome::Done(_)), "{outcome:?}");
        let args = std::fs::read_to_string(dir.path().join("args")).unwrap();
        for bound in [
            "pack.threads=1",
            "pack.windowMemory=64m",
            "core.bigFileThreshold=64m",
        ] {
            assert!(args.contains(bound), "{args}");
        }
        let env = std::fs::read_to_string(dir.path().join("env")).unwrap();
        let mut keys: Vec<&str> = env
            .lines()
            .filter_map(|l| l.split_once('=').map(|(k, _)| k))
            // The shell's own.
            .filter(|k| !matches!(*k, "PWD" | "SHLVL" | "_" | "OLDPWD"))
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "GIT_CONFIG_GLOBAL",
                "GIT_CONFIG_NOSYSTEM",
                "GIT_DIR",
                "GIT_TERMINAL_PROMPT",
                "HOME",
                "LC_ALL",
                "PATH"
            ],
            "{env}"
        );
        assert!(env.contains(&format!("PATH={REPACK_PATH}\n")), "{env}");
        assert!(env.contains("GIT_CONFIG_GLOBAL=/dev/null\n"), "{env}");
    }

    #[test]
    fn a_stop_kills_a_running_repack() {
        let dir = with_a_loose_object();
        let git = fake_git(dir.path(), "exec sleep 60");
        let stop = Arc::new(Stop::default());
        let stopper = {
            let stop = stop.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(300));
                stop.stop();
            })
        };
        let start = Instant::now();
        let outcome = retry_busy(|| repack(&git, dir.path(), &[], &stop)).unwrap();
        stopper.join().unwrap();
        assert_eq!(outcome, RepackOutcome::Stopped);
        assert!(start.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn nothing_loose_is_nothing_to_do() {
        let dir = tempfile::tempdir().unwrap();
        let outcome = repack(
            Path::new("/nonexistent/git"),
            dir.path(),
            &[],
            &Stop::default(),
        )
        .unwrap();
        assert_eq!(outcome, RepackOutcome::NothingToDo);
    }

    #[test]
    fn a_missing_git_is_an_error() {
        let dir = with_a_loose_object();
        let err = repack(
            Path::new("/nonexistent/git"),
            dir.path(),
            &[],
            &Stop::default(),
        )
        .unwrap_err();
        assert!(matches!(err, RepackError::Spawn(_)), "{err}");
    }

    #[test]
    fn a_failing_repack_reports_stderr() {
        let dir = with_a_loose_object();
        let git = fake_git(dir.path(), "echo broken >&2; exit 3");
        let err = retry_busy(|| repack(&git, dir.path(), &[], &Stop::default())).unwrap_err();
        match err {
            RepackError::Failed { stderr, .. } => assert_eq!(stderr, "broken"),
            other => panic!("{other}"),
        }
    }
}
