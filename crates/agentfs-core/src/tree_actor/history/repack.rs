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
//! A stop kills a repack in progress, with every process it started (its
//! own process group): git writes new packs before it deletes old ones, so
//! nothing is lost, and the next open removes its temporary files
//! ([`super::clean_packs`]).
//!
//! The child is not counted with the actor's Git children
//! ([`crate::git_cli::counted`]): it is not on the actor's path, and a count
//! a background task adds to would not tell what an operation started.

use std::io::{self, Read};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use super::built::History;
use crate::repack::{REPACK_ARGS, RepackError};
use crate::tree_actor::stages::{Timer, Which};

/// The task's child and whether a stop was asked, behind one lock: a stop
/// and a start never cross, so no repack starts after a stop.
#[derive(Default)]
struct Slot {
    child: Option<Child>,
    stopped: bool,
}

/// The running repack task: its thread, which ends when History stops.
pub(in crate::tree_actor) struct Repacker {
    thread: std::thread::JoinHandle<()>,
    child: Arc<Mutex<Slot>>,
}

/// How one run ended, when git could be run.
#[derive(Debug, PartialEq, Eq)]
enum Ran {
    Done,
    /// A stop came first, or killed it.
    Stopped,
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
        let child = Arc::new(Mutex::new(Slot::default()));
        let slot = Arc::clone(&child);
        let thread = std::thread::Builder::new()
            .name("tree-repack".into())
            .spawn(move || run(&history, &git_dir, &slot))?;
        Ok(Self { thread, child })
    }

    /// Waits for the task to end: History has stopped, and a repack in
    /// progress was killed. Called after History stops (`TreeActor::shut_down`).
    pub(in crate::tree_actor) fn join(self) {
        {
            let mut slot = lock(&self.child);
            slot.stopped = true;
            if let Some(child) = &slot.child {
                kill_group(child);
            }
        }
        let _ = self.thread.join();
    }
}

fn lock(slot: &Mutex<Slot>) -> MutexGuard<'_, Slot> {
    // Every holder leaves the slot whole: a poisoned lock is still sound.
    slot.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The task: one repack per built event seen, until History stops. A failed
/// repack is logged when it differs from the last and tried again at the
/// next event (keep operating).
fn run(history: &History, git_dir: &Path, slot: &Mutex<Slot>) {
    let mut seen = 0;
    let mut last: Option<String> = None;
    loop {
        seen = history.wait_epoch(seen);
        if history.stopping() {
            return;
        }
        match repack_once(&program(git_dir), git_dir, slot) {
            Ok(Ran::Done) => last = None,
            Ok(Ran::Stopped) => return,
            Err(error) => {
                let text = error.to_string();
                if last.as_ref() != Some(&text) {
                    eprintln!("tree RepackFailed: {text}");
                }
                last = Some(text);
            }
        }
    }
}

/// The `git` program, as `PATH` ([`crate::git_cli::GIT_PATH`]) finds it. A
/// test stands a wrapper in the git directory to hold or skip a repack.
fn program(git_dir: &Path) -> PathBuf {
    #[cfg(test)]
    {
        let wrapper = git_dir.join(tests::WRAPPER);
        if wrapper.exists() {
            return wrapper;
        }
    }
    let _ = git_dir;
    PathBuf::from("git")
}

/// One `git <REPACK_ARGS>` in `git_dir`, in a process group of its own so a
/// stop kills every process it started. The child is reaped only after the
/// slot lets go of it, so a stop never signals a process id reused since.
fn repack_once(git: &Path, git_dir: &Path, slot: &Mutex<Slot>) -> Result<Ran, RepackError> {
    let _timer = Timer::start(Which::Repack);
    let mut held = lock(slot);
    if held.stopped {
        return Ok(Ran::Stopped);
    }
    let mut cmd = crate::git_cli::command(git, git_dir);
    cmd.args(REPACK_ARGS)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut child = cmd.spawn().map_err(RepackError::Spawn)?;
    let pipe = child.stderr.take();
    let pid = child.id();
    held.child = Some(child);
    drop(held);
    // Until every process of the group has let go of it: they have exited
    // or were killed.
    let mut stderr = Vec::new();
    if let Some(mut pipe) = pipe {
        let _ = pipe.read_to_end(&mut stderr);
    }
    // Exited, not yet reaped. Should this fail, the reap below waits instead.
    let _ = exited(pid);
    let mut held = lock(slot);
    let status = match held.child.take() {
        Some(mut child) => child.wait().map_err(RepackError::Spawn)?,
        None => return Ok(Ran::Stopped),
    };
    if held.stopped {
        return Ok(Ran::Stopped);
    }
    drop(held);
    if !status.success() {
        return Err(RepackError::Failed {
            status: status.to_string(),
            stderr: String::from_utf8_lossy(&stderr)
                .lines()
                .next()
                .unwrap_or_default()
                .to_owned(),
        });
    }
    Ok(Ran::Done)
}

/// Waits until child `pid` has exited, leaving it unreaped (`WNOWAIT`): its
/// process id and group stay its own until [`Child::wait`].
fn exited(pid: u32) -> io::Result<()> {
    let id = libc::id_t::from(pid);
    loop {
        // SAFETY: `info` is a valid, writable siginfo_t; waitid has no other
        // preconditions.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let r = unsafe {
            libc::waitid(
                libc::P_PID,
                id,
                &raw mut info,
                libc::WEXITED | libc::WNOWAIT,
            )
        };
        if r == 0 {
            return Ok(());
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

/// Kills `child`'s process group, which it leads: git and every process it
/// started. The child is unreaped, so the group is still its own.
fn kill_group(child: &Child) {
    if let Ok(pid) = libc::pid_t::try_from(child.id()) {
        // SAFETY: kill has no memory preconditions; a negative pid names the group.
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
}

#[cfg(test)]
pub(in crate::tree_actor) mod tests {
    use std::collections::BTreeSet;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::mpsc;
    use std::time::Duration;

    use git2::{ObjectType, Oid, Repository};

    use super::*;
    use crate::tree_actor::TreeActor;
    use crate::tree_actor::TreeId;
    use crate::tree_actor::history::built::BuiltSnapshot;
    use crate::tree_actor::test_support::{ATTRS, new_actor, reopen, writer};

    /// The program the task runs instead of git when the git directory holds it.
    pub(in crate::tree_actor) const WRAPPER: &str = "repack-git";

    /// How long a step that should not block may take before the test fails.
    const HANG_GUARD: Duration = Duration::from_secs(120);

    /// A wrapper that skips the repack: the packs stay as the builder wrote them.
    const SKIP: &str = "exit 0";

    /// A wrapper that writes the new packs, says so on `$0.written`, waits for
    /// `$0.release`, then repacks again with `-d`, which deletes the old ones:
    /// git's own write and delete, with a hold between them.
    const HOLD: &str = r#"for a do shift; [ "$a" = -d ] || set -- "$@" "$a"; done
git "$@" || exit 1
echo written > "$0.written"
read x < "$0.release"
exec git "$@" -d"#;

    fn script(path: &Path, body: &str) {
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::rename(&tmp, path).unwrap();
    }

    /// A held wrapper at `path`, with its two fifos.
    fn hold(path: &Path) {
        for end in ["written", "release"] {
            let fifo = PathBuf::from(format!("{}.{end}", path.display()));
            let c = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
            // SAFETY: a valid C string.
            assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        }
        script(path, HOLD);
    }

    /// Runs `f` on a thread of its own and fails the test when it does not
    /// return within [`HANG_GUARD`].
    fn guarded<T: Send + 'static>(what: &str, f: impl FnOnce() -> T + Send + 'static) -> T {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(f());
        });
        rx.recv_timeout(HANG_GUARD)
            .unwrap_or_else(|_| panic!("{what} did not return"))
    }

    /// Returns once the held wrapper at `path` has written its packs.
    fn wait_written(path: &Path) {
        let fifo = PathBuf::from(format!("{}.written", path.display()));
        guarded("the repack's write", move || {
            std::fs::read(fifo).unwrap();
        });
    }

    fn release(path: &Path) {
        let fifo = PathBuf::from(format!("{}.release", path.display()));
        guarded("the release", move || {
            std::fs::write(fifo, b"go\n").unwrap();
        });
    }

    fn packs(git_dir: &Path) -> usize {
        std::fs::read_dir(git_dir.join("objects/pack"))
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .path()
                    .extension()
                    .is_some_and(|x| x == "pack")
            })
            .count()
    }

    /// `n` batches, one built event each: one pack each.
    fn batches(v: &mut TreeActor, from: usize, n: usize) {
        let w = writer("w");
        for i in from..from + n {
            let h = v
                .write_open(TreeId::Root, &format!("f{i}"), &w, Some(ATTRS))
                .unwrap();
            v.write(h, 0, format!("file {i}\n").as_bytes(), false)
                .unwrap();
            v.close(h).unwrap();
            v.settle().unwrap();
        }
    }

    /// Every object in the store.
    fn all_objects(git_dir: &Path) -> BTreeSet<Oid> {
        let repo = Repository::open_bare(git_dir).unwrap();
        let mut all = BTreeSet::new();
        repo.odb()
            .unwrap()
            .foreach(|oid| {
                all.insert(*oid);
                true
            })
            .unwrap();
        all
    }

    /// Reads every commit, tree and blob reachable from every ref, through
    /// a fresh handle; returns how many objects.
    fn read_reachable(git_dir: &Path) -> usize {
        let repo = Repository::open_bare(git_dir).unwrap();
        let mut walk = repo.revwalk().unwrap();
        for r in repo.references().unwrap() {
            if let Some(oid) = r.unwrap().target()
                && repo.find_object(oid, None).unwrap().kind() == Some(ObjectType::Commit)
            {
                walk.push(oid).unwrap();
            }
        }
        let mut seen = BTreeSet::new();
        for commit in walk {
            let commit = repo.find_commit(commit.unwrap()).unwrap();
            seen.insert(commit.id());
            let mut trees = vec![commit.tree_id()];
            while let Some(tree) = trees.pop() {
                if !seen.insert(tree) {
                    continue;
                }
                for entry in repo.find_tree(tree).unwrap().iter() {
                    match entry.kind() {
                        Some(ObjectType::Tree) => trees.push(entry.id()),
                        _ => {
                            if seen.insert(entry.id()) {
                                repo.find_blob(entry.id()).unwrap();
                            }
                        }
                    }
                }
            }
        }
        seen.len()
    }

    /// Reads every commit, tree and blob a snapshot reaches, through its own
    /// handle on the store.
    fn read_snapshot(snapshot: &BuiltSnapshot) -> usize {
        let repo = snapshot.repo();
        let mut seen = BTreeSet::new();
        let mut commits: Vec<Oid> = snapshot.refs().into_iter().map(|(_, oid)| oid).collect();
        while let Some(oid) = commits.pop() {
            if !seen.insert(oid) {
                continue;
            }
            let Ok(commit) = repo.read_commit(oid) else {
                // A ref that names a blob (a sync queue entry's marker).
                repo.read_blob(oid).unwrap();
                continue;
            };
            commits.extend(commit.parents.iter().copied());
            let mut trees = vec![commit.tree];
            while let Some(tree) = trees.pop() {
                if !seen.insert(tree) {
                    continue;
                }
                for entry in repo.list_tree(tree).unwrap() {
                    if entry.mode == 0o040_000 {
                        trees.push(entry.oid);
                    } else if seen.insert(entry.oid) {
                        repo.read_blob(entry.oid).unwrap();
                    }
                }
            }
        }
        seen.len()
    }

    /// E1: 64 batches are 64 packs; one repack leaves a handful, and every
    /// object of every built commit still reads.
    #[test]
    fn a_repack_folds_the_batches_packs() {
        let tmp = tempfile::tempdir().unwrap();
        let git_dir = tmp.path().join("history.git");
        let mut v = new_actor(tmp.path());
        script(&git_dir.join(WRAPPER), SKIP);
        batches(&mut v, 0, 64);
        let before = packs(&git_dir);
        assert!(before >= 64, "{before} packs");
        let reachable = read_reachable(&git_dir);
        let objects = all_objects(&git_dir);

        let slot = Mutex::new(Slot::default());
        assert_eq!(
            repack_once(Path::new("git"), &git_dir, &slot).unwrap(),
            Ran::Done
        );
        let after = packs(&git_dir);
        assert!(after <= 8, "{before} packs became {after}");
        assert_eq!(read_reachable(&git_dir), reachable);
        assert!(all_objects(&git_dir).is_superset(&objects));
        drop(v);
    }

    /// E2: a snapshot taken before the repack reads every object while the
    /// repack holds between its write and its delete, and after the delete.
    #[test]
    fn a_snapshot_reads_through_a_repack() {
        let tmp = tempfile::tempdir().unwrap();
        let git_dir = tmp.path().join("history.git");
        let mut v = new_actor(tmp.path());
        script(&git_dir.join(WRAPPER), SKIP);
        batches(&mut v, 0, 16);
        let snapshot = v.history().snapshot();
        // The handle is open, with only the head's pack read.
        let head = snapshot.head(TreeId::Root).unwrap();
        snapshot.repo().read_commit(head).unwrap();
        let before = packs(&git_dir);

        let held = tmp.path().join("hold-git");
        hold(&held);
        let run = {
            let (held, git_dir) = (held.clone(), git_dir.clone());
            std::thread::spawn(move || {
                repack_once(&held, &git_dir, &Mutex::new(Slot::default())).unwrap()
            })
        };
        wait_written(&held);
        assert!(packs(&git_dir) > before, "the new pack is written");
        let reached = read_snapshot(&snapshot);
        release(&held);
        assert_eq!(
            guarded("the repack", move || run.join().unwrap()),
            Ran::Done
        );
        assert!(packs(&git_dir) <= 8, "{} packs", packs(&git_dir));
        assert_eq!(read_snapshot(&snapshot), reached);
        assert_eq!(read_snapshot(&v.history().snapshot()), reached);
        drop(v);
    }

    /// E3: an unreachable blob written before a repack is in the store after it.
    #[test]
    fn a_repack_keeps_unreachable_objects() {
        let tmp = tempfile::tempdir().unwrap();
        let git_dir = tmp.path().join("history.git");
        let mut v = new_actor(tmp.path());
        script(&git_dir.join(WRAPPER), SKIP);
        batches(&mut v, 0, 4);
        let blob = Repository::open_bare(&git_dir)
            .unwrap()
            .blob(b"nothing names this\n")
            .unwrap();
        let objects = all_objects(&git_dir);
        assert!(objects.contains(&blob));

        let slot = Mutex::new(Slot::default());
        assert_eq!(
            repack_once(Path::new("git"), &git_dir, &slot).unwrap(),
            Ran::Done
        );
        assert!(packs(&git_dir) < 5);
        let repo = Repository::open_bare(&git_dir).unwrap();
        assert_eq!(
            repo.find_blob(blob).unwrap().content(),
            b"nothing names this\n"
        );
        assert!(all_objects(&git_dir).is_superset(&objects));
        drop(v);
    }

    /// E4: the task repacks at a built event; a shutdown with that repack
    /// held returns, and a reopen reads every object.
    #[test]
    fn a_stop_kills_a_held_repack() {
        let tmp = tempfile::tempdir().unwrap();
        let git_dir = tmp.path().join("history.git");
        let mut v = new_actor(tmp.path());
        script(&git_dir.join(WRAPPER), SKIP);
        batches(&mut v, 0, 8);
        let objects = all_objects(&git_dir);
        let reachable = read_reachable(&git_dir);

        // In place of the skipping wrapper, at once.
        let wrapper = git_dir.join(WRAPPER);
        hold(&wrapper);
        batches(&mut v, 8, 1);
        wait_written(&wrapper);
        guarded("the shutdown", move || drop(v));

        std::fs::remove_file(&wrapper).unwrap();
        let v = reopen(tmp.path());
        assert!(all_objects(&git_dir).is_superset(&objects));
        assert!(read_reachable(&git_dir) > reachable);
        let pack_dir = git_dir.join("objects/pack");
        for entry in std::fs::read_dir(pack_dir).unwrap() {
            let name = entry.unwrap().file_name();
            assert!(
                !name.to_string_lossy().starts_with(".tmp-"),
                "{name:?} is left"
            );
        }
        drop(v);
    }

    /// A killed repack's temporary files go at the next open.
    #[test]
    fn clean_packs_removes_a_killed_repacks_files() {
        let tmp = tempfile::tempdir().unwrap();
        let pack_dir = tmp.path().join("objects/pack");
        std::fs::create_dir_all(&pack_dir).unwrap();
        let tmp_pack = pack_dir.join(".tmp-123-pack-abc.pack");
        std::fs::write(&tmp_pack, b"").unwrap();
        crate::tree_actor::history::clean_packs(tmp.path()).unwrap();
        assert!(!tmp_pack.exists());
    }

    /// A failed repack is an error the task logs, and the next event runs
    /// another.
    #[test]
    fn a_failed_repack_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let fails = tmp.path().join("fails");
        script(&fails, "echo broken >&2; exit 3");
        let slot = Mutex::new(Slot::default());
        let error = repack_once(&fails, tmp.path(), &slot).unwrap_err();
        assert!(error.to_string().contains("broken"), "{error}");
        // A stop asked first: nothing starts.
        lock(&slot).stopped = true;
        assert_eq!(
            repack_once(&fails, tmp.path(), &slot).unwrap(),
            Ran::Stopped
        );
    }
}
