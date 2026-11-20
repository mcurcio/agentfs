//! The tree history's repack, a task of its own (docs/trees.md, "History
//! trails the files"). Each batch the builder installs is one pack, and a
//! read's cost grows with the pack count, so at each built event the task
//! runs `git repack --geometric=2 -d`, one at a time, with the memory bounds
//! of the recording engine's repack ([`crate::repack::REPACK_ARGS`]). It runs
//! beside `git fast-import`, as a git server repacks beside receive-pack,
//! never under the builder's lease and never stopping it. Geometric repack
//! packs whole packs, keeping every object, reachable or not; nothing prunes
//! and no ref is touched. Events that come while a repack runs are one more
//! run after it, not one each.
//!
//! **A reader never loses a pack it has listed to a repack.** git and libgit2
//! find a pack by listing `objects/pack` and then opening the index of a pack
//! listed; on a miss they list again once and give up. `git repack -d`
//! deletes the packs it rolled up as soon as it installs the new one, so a
//! reader that listed before the install and opens after the delete reports
//! an object it holds as missing. The repack therefore never runs in the
//! store's own `objects/pack`, the way git's incremental repack defers its
//! deletes (`git multi-pack-index expire` after `repack`):
//! - each run is in a scratch object directory of its own under
//!   `repack/` in the git directory, on hard links to the store's packs, with
//!   the store's objects as an alternate; git rolls up and deletes there only;
//! - the task links each new pack into `objects/pack`, its index last, and
//!   fsyncs the directory;
//! - the store's packs that run rolled up are set aside: later runs leave
//!   them out, and they are deleted only at a run that starts on a built
//!   event that came after that run ended. A reader fails only if one
//!   listing of its spans a whole built batch.
//!
//! A stop kills a repack in progress, with every process it started (its
//! own process group). A process outliving its parent (a daemon killed) only
//! ever writes in its scratch directory, which the next run removes. git
//! fsyncs the pack it writes and the task fsyncs `objects/pack` after linking
//! it, so a batch whose pack is rolled up is as durable as one that is not
//! (history/feed.rs).
//!
//! The child is not counted with the actor's Git children
//! ([`crate::git_cli::counted`]): it is not on the actor's path, and a count
//! a background task adds to would not tell what an operation started.

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{self, Read};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, SystemTime};

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

/// The fsync settings a repack runs with, whatever the repository's config
/// says: git fsyncs the pack it writes before it renames it in, which
/// history/feed.rs relies on when a batch's pack is rolled up before its own
/// fsync.
const FSYNC: [&str; 4] = [
    "-c",
    "core.fsync=pack,pack-metadata",
    "-c",
    "core.fsyncMethod=fsync",
];

/// The git directory's folder of the runs' scratch object directories.
const SCRATCH: &str = "repack";

/// A pack's files the scratch directory links, its index last: a reader
/// finds a pack by its index. A `.keep` keeps a pack out of the roll-up
/// there as here.
const LINKED: [&str; 4] = ["keep", "pack", "rev", "idx"];

/// A new pack's files the task links into the store, its index last.
const INSTALLED: [&str; 3] = ["pack", "rev", "idx"];

/// A pack's files the task deletes once it is set aside long enough, its
/// index first: no listing after that finds it.
const DELETED: [&str; 4] = ["idx", "pack", "rev", "keep"];

/// Packs by name (`pack-<hash>`).
type Packs = BTreeSet<String>;

/// Why one repack failed: the task logs it and tries again at the next event.
#[derive(Debug, thiserror::Error)]
enum TaskError {
    #[error(transparent)]
    Repack(#[from] RepackError),
    #[error("{what} {path}: {source}")]
    Io {
        what: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

fn io_error(what: &'static str, path: &Path) -> impl FnOnce(io::Error) -> TaskError {
    let path = path.to_owned();
    move |source| TaskError::Io { what, path, source }
}

/// How one run ended, when git could be run.
#[derive(Debug, PartialEq, Eq)]
enum Ran {
    /// The store's packs it rolled up, to set aside.
    Done(Packs),
    /// A stop came first, or killed it.
    Stopped,
}

/// The store's packs a run rolled up, kept until a built event after `after`.
#[derive(Default)]
struct Aside {
    packs: Packs,
    after: u64,
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
/// next event (keep operating). Packs set aside at a stop stay: a later
/// run rolls them up again, with the objects they share with the new pack
/// written once.
fn run(history: &History, git_dir: &Path, slot: &Mutex<Slot>) {
    let mut seen = 0;
    let mut last: Option<String> = None;
    let mut aside = Aside::default();
    loop {
        seen = history.wait_epoch(seen);
        if history.stopping() {
            return;
        }
        let result = if !aside.packs.is_empty() && seen > aside.after {
            delete(git_dir, &aside.packs).map(|()| aside.packs.clear())
        } else {
            Ok(())
        };
        let result = result.and_then(|()| {
            repack_once(&program(git_dir), git_dir, slot, &aside.packs).map(|ran| match ran {
                Ran::Done(rolled) => {
                    if !rolled.is_empty() {
                        aside.packs.extend(rolled);
                        aside.after = history.wait_epoch_for(u64::MAX, Duration::ZERO);
                    }
                    true
                }
                Ran::Stopped => false,
            })
        });
        match result {
            Ok(true) => last = None,
            Ok(false) => return,
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

/// A run's scratch object directory, removed when dropped.
struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    /// A new scratch object directory whose `pack/` links every pack of
    /// `git_dir`'s store but those in `aside`: the store's packs it linked.
    /// What another process's runs left (a killed daemon's) is removed
    /// first, as far as it can be; this process's own go when dropped.
    fn new(git_dir: &Path, aside: &Packs) -> Result<(Self, Packs), TaskError> {
        static RUNS: AtomicU64 = AtomicU64::new(0);
        let ours = format!("{}-", process_token());
        let root = git_dir.join(SCRATCH);
        if let Ok(entries) = fs::read_dir(&root) {
            for entry in entries.flatten() {
                if !entry.file_name().to_string_lossy().starts_with(&ours) {
                    let _ = fs::remove_dir_all(entry.path());
                }
            }
        }
        let run = RUNS.fetch_add(1, Ordering::Relaxed);
        let scratch = Self {
            dir: root.join(format!("{ours}{run}")),
        };
        let pack_dir = scratch.dir.join("pack");
        fs::create_dir_all(&pack_dir).map_err(io_error("creating", &pack_dir))?;
        let store = git_dir.join("objects").join("pack");
        let mut linked = Packs::new();
        for name in packs(&store)? {
            if aside.contains(&name) {
                continue;
            }
            // A pack whose index is gone was set aside and deleted by a
            // run before a restart; one being installed is not yet listed.
            if link_pack(&store, &pack_dir, &name, &LINKED)? {
                linked.insert(name);
            }
        }
        Ok((scratch, linked))
    }

    fn pack_dir(&self) -> PathBuf {
        self.dir.join("pack")
    }
}

/// This process among every one that has repacked the store: its id and
/// the time it first asked, since a restarted daemon may get its id again.
fn process_token() -> &'static str {
    static TOKEN: OnceLock<String> = OnceLock::new();
    TOKEN.get_or_init(|| {
        let at = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        format!("{}.{at}", std::process::id())
    })
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// The packs listed in `dir`: each name with an index (`pack-<hash>.idx`).
fn packs(dir: &Path) -> Result<Packs, TaskError> {
    let mut names = Packs::new();
    for entry in fs::read_dir(dir).map_err(io_error("listing", dir))? {
        let name = entry.map_err(io_error("listing", dir))?.file_name();
        let name = name.to_string_lossy();
        if let Some(pack) = name.strip_suffix(".idx")
            && pack.starts_with("pack-")
        {
            names.insert(pack.to_owned());
        }
    }
    Ok(names)
}

/// Links pack `name`'s files `exts` from `from` into `to`, in that order,
/// leaving any `to` already has. False when its pack or index is not in
/// `from`: it is not whole there.
fn link_pack(from: &Path, to: &Path, name: &str, exts: &[&str]) -> Result<bool, TaskError> {
    for ext in exts {
        let file = format!("{name}.{ext}");
        let source = from.join(&file);
        match fs::hard_link(&source, to.join(&file)) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound && !matches!(*ext, "pack" | "idx") => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(io_error("linking", &source)(e)),
        }
    }
    Ok(true)
}

/// fsyncs directory `dir`: the names linked into it, or deleted, are durable.
fn sync_dir(dir: &Path) -> Result<(), TaskError> {
    File::open(dir)
        .and_then(|d| d.sync_all())
        .map_err(io_error("syncing", dir))
}

/// Deletes the store's packs `aside`, each index first, then fsyncs
/// `objects/pack`.
fn delete(git_dir: &Path, aside: &Packs) -> Result<(), TaskError> {
    let store = git_dir.join("objects").join("pack");
    for name in aside {
        for ext in DELETED {
            let path = store.join(format!("{name}.{ext}"));
            match fs::remove_file(&path) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => {
                    return Err(io_error("deleting", &path)(e));
                }
                _ => {}
            }
        }
    }
    sync_dir(&store)
}

/// One `git <REPACK_ARGS>` in a scratch object directory of `git_dir`'s
/// store, leaving out the packs `aside`; links the packs it wrote into the
/// store and returns the store's packs it rolled up. git runs in a process
/// group of its own so a stop kills every process it started. The child is
/// reaped only after the slot lets go of it, so a stop never signals a
/// process id reused since.
fn repack_once(
    git: &Path,
    git_dir: &Path,
    slot: &Mutex<Slot>,
    aside: &Packs,
) -> Result<Ran, TaskError> {
    let _timer = Timer::start(Which::Repack);
    if lock(slot).stopped {
        return Ok(Ran::Stopped);
    }
    let (scratch, linked) = Scratch::new(git_dir, aside)?;
    let mut held = lock(slot);
    if held.stopped {
        return Ok(Ran::Stopped);
    }
    let mut cmd = crate::git_cli::command(git, git_dir);
    cmd.env("GIT_OBJECT_DIRECTORY", &scratch.dir)
        .env("GIT_ALTERNATE_OBJECT_DIRECTORIES", git_dir.join("objects"))
        .args(FSYNC)
        .args(REPACK_ARGS)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .process_group(0);
    // git dies with this process; what it started finishes in its scratch
    // directory.
    let parent = std::process::id();
    // SAFETY: pre_exec runs after fork, before exec; prctl and getppid are
    // async-signal-safe.
    unsafe {
        cmd.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) == -1 {
                return Err(io::Error::last_os_error());
            }
            if u32::try_from(libc::getppid()).ok() != Some(parent) {
                return Err(io::Error::from_raw_os_error(libc::ECHILD));
            }
            Ok(())
        });
    }
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
        }
        .into());
    }
    // git fsynced the new packs' files; linking them in, then this fsync,
    // makes their names durable before any pack they roll up is deleted.
    let store = git_dir.join("objects").join("pack");
    let after = packs(&scratch.pack_dir())?;
    for name in after.difference(&linked) {
        link_pack(&scratch.pack_dir(), &store, name, &INSTALLED)?;
    }
    sync_dir(&store)?;
    Ok(Ran::Done(linked.difference(&after).cloned().collect()))
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

    fn done(ran: Ran) -> Packs {
        match ran {
            Ran::Done(rolled) => rolled,
            Ran::Stopped => panic!("the run stopped"),
        }
    }

    /// The store's packs, as a reader lists them.
    fn listed(git_dir: &Path) -> Packs {
        super::packs(&git_dir.join("objects/pack")).unwrap()
    }

    fn entries(git_dir: &Path) -> BTreeSet<std::ffi::OsString> {
        std::fs::read_dir(git_dir.join("objects/pack"))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect()
    }

    /// E1: 64 batches are 64 packs; one repack rolls them up, and once the
    /// rolled-up packs are deleted a handful are left. Every object of every
    /// built commit reads throughout.
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
        let none = Packs::new();
        let rolled = done(repack_once(Path::new("git"), &git_dir, &slot, &none).unwrap());
        assert!(rolled.len() >= 56, "{} rolled up", rolled.len());
        assert_eq!(read_reachable(&git_dir), reachable);
        // A run leaves out the packs set aside.
        let again = done(repack_once(Path::new("git"), &git_dir, &slot, &rolled).unwrap());
        assert!(again.is_disjoint(&rolled));
        delete(&git_dir, &rolled.union(&again).cloned().collect()).unwrap();
        let after = packs(&git_dir);
        assert!(after <= 8, "{before} packs became {after}");
        assert_eq!(read_reachable(&git_dir), reachable);
        assert!(all_objects(&git_dir).is_superset(&objects));
        drop(v);
    }

    /// A pack a reader listed is still there after the run that rolls it
    /// up: git and libgit2 list `objects/pack` again only once on a miss, so
    /// a pack deleted between a reader's listing and its open is an object
    /// reported missing that the store holds (`git cat-file` said `<oid>
    /// missing` in the phase 1 replay when `git repack -d` ran in the store).
    #[test]
    fn a_pack_a_reader_listed_outlives_the_run_that_rolls_it_up() {
        let tmp = tempfile::tempdir().unwrap();
        let git_dir = tmp.path().join("history.git");
        let mut v = new_actor(tmp.path());
        script(&git_dir.join(WRAPPER), SKIP);
        batches(&mut v, 0, 8);
        let seen = listed(&git_dir);
        let slot = Mutex::new(Slot::default());
        let rolled = done(repack_once(Path::new("git"), &git_dir, &slot, &Packs::new()).unwrap());
        for pack in &seen {
            for ext in ["idx", "pack"] {
                let path = git_dir.join(format!("objects/pack/{pack}.{ext}"));
                assert!(File::open(&path).is_ok(), "{path:?}, listed before the run");
            }
        }
        assert!(!rolled.is_empty() && rolled.is_subset(&seen));
        drop(v);
    }

    /// E2: while git repacks, the store's `objects/pack` is as it was (no
    /// file in flight, none deleted), and a snapshot taken before reads every
    /// object then, after the run, and after the rolled-up packs go.
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
        let before = entries(&git_dir);

        let held = tmp.path().join("hold-git");
        hold(&held);
        let run = {
            let (held, git_dir) = (held.clone(), git_dir.clone());
            std::thread::spawn(move || {
                let slot = Mutex::new(Slot::default());
                repack_once(&held, &git_dir, &slot, &Packs::new()).unwrap()
            })
        };
        wait_written(&held);
        assert_eq!(entries(&git_dir), before, "the store's packs mid-repack");
        let reached = read_snapshot(&snapshot);
        release(&held);
        let rolled = done(guarded("the repack", move || run.join().unwrap()));
        assert!(listed(&git_dir).is_superset(&rolled));
        assert_eq!(read_snapshot(&snapshot), reached);
        delete(&git_dir, &rolled).unwrap();
        assert!(packs(&git_dir) <= 8, "{} packs", packs(&git_dir));
        assert_eq!(read_snapshot(&snapshot), reached);
        assert_eq!(read_snapshot(&v.history().snapshot()), reached);
        drop(v);
    }

    /// E3: an unreachable blob written before a repack is in the store after
    /// it, and after the rolled-up packs go.
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
        let rolled = done(repack_once(Path::new("git"), &git_dir, &slot, &Packs::new()).unwrap());
        delete(&git_dir, &rolled).unwrap();
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
    /// held returns, leaving nothing of it, and a reopen reads every object.
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
        let scratch = std::fs::read_dir(git_dir.join(SCRATCH)).unwrap().count();
        assert_eq!(scratch, 0, "a scratch directory is left");

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

    /// Batches built while the task repacks beside them: none fails, though
    /// a repack may roll a batch's pack up, and delete it, before the batch
    /// fsyncs it (history/feed.rs).
    #[test]
    fn batches_beside_the_task_never_fail() {
        let tmp = tempfile::tempdir().unwrap();
        let git_dir = tmp.path().join("history.git");
        let mut v = new_actor(tmp.path());
        batches(&mut v, 0, 100);
        let reachable = read_reachable(&git_dir);
        drop(v);
        let v = reopen(tmp.path());
        assert_eq!(read_reachable(&git_dir), reachable);
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
        std::fs::create_dir_all(tmp.path().join("objects/pack")).unwrap();
        let slot = Mutex::new(Slot::default());
        let none = Packs::new();
        let error = repack_once(&fails, tmp.path(), &slot, &none).unwrap_err();
        assert!(error.to_string().contains("broken"), "{error}");
        // A stop asked first: nothing starts.
        lock(&slot).stopped = true;
        assert_eq!(
            repack_once(&fails, tmp.path(), &slot, &none).unwrap(),
            Ran::Stopped
        );
    }
}
