//! The tree history's repack, a task of its own (docs/trees.md, "History
//! trails the files"). Each batch the builder installs is one pack, and a
//! read's cost grows with the pack count, so at each built event the task
//! rolls the small packs up, one run at a time, with the memory bounds of
//! the recording engine's repack ([`crate::repack::PACK_CONFIG`]). It runs
//! beside `git fast-import`, as a git server repacks beside receive-pack,
//! never under the builder's lease and never stopping it. Events that come
//! while a run is in progress are one more run after it, not one each.
//!
//! **git's incremental repack.** The packs a roll-up supersedes are deleted
//! by git at a later run, as `git maintenance`'s incremental-repack task
//! defers `git multi-pack-index expire` after its roll-up:
//! 1. `git multi-pack-index expire`, once a built event came after the last
//!    roll-up ended. git deletes the packs in which the multi-pack index
//!    finds no object: the ones that roll-up superseded.
//! 2. The packs to roll up are picked as `git repack --geometric=2` picks
//!    them ([`roll_up`]), by size, and written into one new pack by `git
//!    pack-objects --stdin-packs`, the call `git repack --geometric` makes:
//!    whole packs, every object, reachable or not. Nothing is deleted.
//! 3. `git multi-pack-index write --preferred-pack=<the new pack>`: the
//!    index finds every object the new pack holds there, so the packs it
//!    rolled up hold none the index finds, and step 1 of a later run
//!    expires them. `objects/pack` is fsynced before any of them can go.
//!
//! Which pack goes is git's judgement, from the index, not a list of names
//! the task keeps. A roll-up whose pack has the name of one the store
//! already holds (a pack's name hashes its content, and rolling up the same
//! packs writes the same pack) keeps that pack, since the index finds its
//! objects there. No ref is touched and nothing prunes.
//!
//! **A reader never loses a pack it has listed to a roll-up.** git and
//! libgit2 find an object through the multi-pack index or by listing
//! `objects/pack`, and open its pack later; on a miss they look again once
//! and give up. A pack is deleted only at a run that starts on a built
//! event after the roll-up that superseded it ended, so a reader fails only
//! if one look of its spans a whole built batch. While a roll-up waits for
//! its expiry no run rolls up again, so at most one roll-up's packs wait to
//! go.
//!
//! **A stop.** A stop kills a run in progress, with every process it
//! started (its own process group). git writes a pack under a temporary
//! name and renames it in, index last, so a killed run leaves at most a
//! temporary file, which the next start removes (history/mod.rs,
//! `clean_packs`), or a whole new pack the index does not prefer, whose
//! packs a later run rolls up again. A stale `multi-pack-index.lock` goes
//! at the next start with the other locks (`remove_stale_locks`). The start
//! expires what the last roll-up superseded before the volume is served,
//! so a stop leaves no copy behind. git fsyncs what it writes
//! (`core.fsync=pack,pack-metadata`), so a batch whose pack is rolled up is
//! as durable as one that is not (history/feed.rs).
//!
//! **Costs.** The packs a roll-up superseded stay on disk until a built
//! event after it, so an idle volume keeps them until its next save or
//! start. The volume's own libgit2 handles keep every pack they opened
//! mapped, so a deleted pack's space comes back only when they let it go,
//! at the latest when the volume stops. Loose objects (the verification
//! notes `git notes` writes) are not rolled up.
//!
//! The children are not counted with the actor's Git children
//! ([`crate::git_cli::counted`]): they are not on the actor's path, and a
//! count a background task adds to would not tell what an operation started.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use super::built::History;
use crate::repack::PACK_CONFIG;
use crate::tree_actor::stages::{Timer, Which};

/// The task's child and whether a stop was asked, behind one lock: a stop
/// and a start never cross, so no git command starts after a stop.
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

/// The fsync settings a run's git commands use, whatever the repository's
/// config says: git fsyncs the pack and the multi-pack index it writes
/// before it renames them in, which history/feed.rs relies on when a
/// batch's pack is rolled up before its own fsync.
const FSYNC: [&str; 4] = [
    "-c",
    "core.fsync=pack,pack-metadata",
    "-c",
    "core.fsyncMethod=fsync",
];

/// The roll-up's pack write; the names of the packs it rolls up go on its
/// stdin, and the new pack's hash comes on its stdout.
const PACK_OBJECTS: [&str; 6] = [
    "pack-objects",
    "--stdin-packs",
    "--non-empty",
    "--delta-base-offset",
    "-q",
    "objects/pack/pack",
];

/// The packs listed in `objects/pack` a roll-up may take, by name
/// (`pack-<hash>`), with the size of each one's `.pack`.
type Packs = BTreeMap<String, u64>;

/// Why one run failed: the task logs it and tries again at the next event.
#[derive(Debug, thiserror::Error)]
enum TaskError {
    #[error("running git: {0}")]
    Spawn(#[source] io::Error),
    #[error("git {what} exited with {status}: {stderr}")]
    Failed {
        what: String,
        status: String,
        stderr: String,
    },
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

/// How a git command or a step of a run ended, when git could be run.
#[derive(Debug, PartialEq, Eq)]
enum Ran<T> {
    Done(T),
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
        // Before the volume is served, what the last process's last roll-up
        // superseded goes. A failure is said and the volume served (keep
        // operating).
        if let Err(error) = expire(&program(&git_dir), &git_dir, &child) {
            eprintln!("tree RepackFailed: {error}");
        }
        let slot = Arc::clone(&child);
        let thread = std::thread::Builder::new()
            .name("tree-repack".into())
            .spawn(move || run(&history, &git_dir, &slot))?;
        Ok(Self { thread, child })
    }

    /// Waits for the task to end: History has stopped, and a git command in
    /// progress was killed. Called after History stops
    /// (`TreeActor::shut_down`).
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

/// The task: one run per built event seen, until History stops. A failed
/// run is logged when it differs from the last and tried again at the next
/// event (keep operating).
fn run(history: &History, git_dir: &Path, slot: &Mutex<Slot>) {
    let mut seen = 0;
    // The built epoch the last roll-up ended at, until its expiry.
    let mut pending: Option<u64> = None;
    let mut last: Option<String> = None;
    loop {
        seen = history.wait_epoch(seen);
        if history.stopping() {
            return;
        }
        if pending.is_some_and(|after| seen <= after) {
            continue;
        }
        match run_once(history, &program(git_dir), git_dir, slot, &mut pending) {
            Ok(Ran::Done(())) => last = None,
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

/// One run, on a built event after the epoch `pending` holds, if any: the
/// expiry of the last roll-up, then a roll-up, which `pending` then holds
/// the end of.
fn run_once(
    history: &History,
    git: &Path,
    git_dir: &Path,
    slot: &Mutex<Slot>,
    pending: &mut Option<u64>,
) -> Result<Ran<()>, TaskError> {
    if pending.is_some() {
        if expire(git, git_dir, slot)? == Ran::Stopped {
            return Ok(Ran::Stopped);
        }
        *pending = None;
    }
    Ok(match repack_once(git, git_dir, slot)? {
        Ran::Done(rolled) => {
            if rolled {
                *pending = Some(history.wait_epoch_for(u64::MAX, Duration::ZERO));
            }
            Ran::Done(())
        }
        Ran::Stopped => Ran::Stopped,
    })
}

/// The `git` program, as `PATH` ([`crate::git_cli::GIT_PATH`]) finds it. A
/// test stands a wrapper in the git directory to hold or skip a run.
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

/// `git multi-pack-index expire`: deletes the packs in which the
/// multi-pack index finds no object, those the last roll-up superseded.
/// Nothing when there is no index.
fn expire(git: &Path, git_dir: &Path, slot: &Mutex<Slot>) -> Result<Ran<()>, TaskError> {
    Ok(
        match command(git, git_dir, slot, &["multi-pack-index", "expire"], None)? {
            Ran::Done(_) => Ran::Done(()),
            Ran::Stopped => Ran::Stopped,
        },
    )
}

/// How many of `sizes`, sorted smallest first, `git repack --geometric=2`
/// rolls up (git's `split_pack_geometry`): the smallest packs, up to where
/// each larger pack is at least twice the one before it and twice all the
/// packs rolled up together. Fewer than two is none.
fn roll_up(sizes: &[u64]) -> usize {
    const FACTOR: u64 = 2;
    let mut split = sizes.len().saturating_sub(1);
    while split > 0 && sizes[split] >= FACTOR.saturating_mul(sizes[split - 1]) {
        split -= 1;
    }
    if split > 0 {
        split += 1;
    }
    let mut total: u64 = sizes[..split].iter().sum();
    while split < sizes.len() && sizes[split] < FACTOR.saturating_mul(total) {
        total = total.saturating_add(sizes[split]);
        split += 1;
    }
    if split < 2 { 0 } else { split }
}

/// One roll-up of `git_dir`'s store, of the packs [`roll_up`] picks: true
/// when it wrote a pack and made the multi-pack index prefer it, so the
/// packs it rolled up expire at a later run.
fn repack_once(git: &Path, git_dir: &Path, slot: &Mutex<Slot>) -> Result<Ran<bool>, TaskError> {
    let _timer = Timer::start(Which::Repack);
    let listed = packs(&git_dir.join("objects").join("pack"))?;
    let mut by_size: Vec<(u64, &String)> =
        listed.iter().map(|(name, size)| (*size, name)).collect();
    by_size.sort();
    let sizes: Vec<u64> = by_size.iter().map(|(size, _)| *size).collect();
    let n = roll_up(&sizes);
    if n == 0 {
        return Ok(Ran::Done(false));
    }
    let names: Vec<&str> = by_size[..n].iter().map(|(_, name)| name.as_str()).collect();
    roll(git, git_dir, slot, &names)
}

/// Writes the packs `names` into one new pack and makes the multi-pack
/// index prefer it: true when there was something to write.
fn roll(
    git: &Path,
    git_dir: &Path,
    slot: &Mutex<Slot>,
    names: &[&str],
) -> Result<Ran<bool>, TaskError> {
    let mut input = String::new();
    for name in names {
        input.push_str(name);
        input.push_str(".pack\n");
    }
    let out = match command(git, git_dir, slot, &PACK_OBJECTS, Some(input.as_bytes()))? {
        Ran::Done(out) => out,
        Ran::Stopped => return Ok(Ran::Stopped),
    };
    let hash = String::from_utf8_lossy(&out).trim().to_owned();
    if hash.is_empty() {
        return Ok(Ran::Done(false));
    }
    let preferred = format!("--preferred-pack=pack-{hash}.pack");
    let write = ["multi-pack-index", "write", preferred.as_str()];
    if command(git, git_dir, slot, &write, None)? == Ran::Stopped {
        return Ok(Ran::Stopped);
    }
    // The new pack's name is durable before any pack it rolled up can go.
    let store = git_dir.join("objects").join("pack");
    File::open(&store)
        .and_then(|dir| dir.sync_all())
        .map_err(io_error("syncing", &store))?;
    Ok(Ran::Done(true))
}

/// The packs listed in `dir` a roll-up may take: each whole one (its index
/// and its pack there) no `.keep` holds back, with its pack's size.
fn packs(dir: &Path) -> Result<Packs, TaskError> {
    let mut packs = Packs::new();
    for entry in std::fs::read_dir(dir).map_err(io_error("listing", dir))? {
        let name = entry.map_err(io_error("listing", dir))?.file_name();
        let name = name.to_string_lossy();
        let Some(pack) = name.strip_suffix(".idx") else {
            continue;
        };
        if !pack.starts_with("pack-") || dir.join(format!("{pack}.keep")).exists() {
            continue;
        }
        // A pack being expired, its pack gone first, is left out.
        if let Ok(meta) = std::fs::metadata(dir.join(format!("{pack}.pack"))) {
            packs.insert(pack.to_owned(), meta.len());
        }
    }
    Ok(packs)
}

/// Runs one `git <FSYNC> <PACK_CONFIG> <args>` on `git_dir`, `input` on its
/// stdin, and returns its stdout. git runs in a process group of its own so
/// a stop kills every process it started. The child is reaped only after
/// the slot lets go of it, so a stop never signals a process id reused
/// since.
fn command(
    git: &Path,
    git_dir: &Path,
    slot: &Mutex<Slot>,
    args: &[&str],
    input: Option<&[u8]>,
) -> Result<Ran<Vec<u8>>, TaskError> {
    let mut held = lock(slot);
    if held.stopped {
        return Ok(Ran::Stopped);
    }
    let mut cmd = crate::git_cli::command(git, git_dir);
    cmd.args(FSYNC)
        .args(PACK_CONFIG)
        .args(args)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    // git dies with this process; a pack it was writing is a temporary file.
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
    let mut child = cmd.spawn().map_err(TaskError::Spawn)?;
    let stdin = child.stdin.take();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let pid = child.id();
    held.child = Some(child);
    drop(held);
    // Until every process of the group has let go of the pipes: they have
    // exited or were killed. stderr is read beside stdout so neither fills.
    let (out, err) = std::thread::scope(|scope| {
        let err = scope.spawn(|| {
            let mut buf = Vec::new();
            if let Some(mut pipe) = stderr {
                let _ = pipe.read_to_end(&mut buf);
            }
            buf
        });
        if let (Some(mut pipe), Some(input)) = (stdin, input) {
            // A git that died first is told by its status below.
            let _ = pipe.write_all(input);
        }
        let mut out = Vec::new();
        if let Some(mut pipe) = stdout {
            let _ = pipe.read_to_end(&mut out);
        }
        (out, err.join().unwrap_or_default())
    });
    // Exited, not yet reaped. Should this fail, the reap below waits instead.
    let _ = exited(pid);
    let mut held = lock(slot);
    let status = match held.child.take() {
        Some(mut child) => child.wait().map_err(TaskError::Spawn)?,
        None => return Ok(Ran::Stopped),
    };
    if held.stopped {
        return Ok(Ran::Stopped);
    }
    drop(held);
    if !status.success() {
        return Err(TaskError::Failed {
            what: args[..args.len().min(2)].join(" "),
            status: status.to_string(),
            stderr: String::from_utf8_lossy(&err)
                .lines()
                .next()
                .unwrap_or_default()
                .to_owned(),
        });
    }
    Ok(Ran::Done(out))
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
    use crate::tree_actor::test_support::{ATTRS, new_actor, reopen, rowless_fork, writer};

    /// The program the task runs instead of git when the git directory holds it.
    pub(in crate::tree_actor) const WRAPPER: &str = "repack-git";

    /// How long a step that should not block may take before the test fails.
    const HANG_GUARD: Duration = Duration::from_secs(120);

    /// A wrapper that skips every run: the packs stay as the builder wrote them.
    const SKIP: &str = "exit 0";

    /// A wrapper that, before git writes a roll-up's pack, says so on
    /// `$0.written` and waits for `$0.release`.
    const HOLD: &str = r#"case " $* " in *" pack-objects "*)
  echo written > "$0.written"
  read x < "$0.release";;
esac
exec git "$@""#;

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

    /// Returns once the held wrapper at `path` is about to write its pack.
    fn wait_written(path: &Path) {
        let fifo = PathBuf::from(format!("{}.written", path.display()));
        guarded("the roll-up's write", move || {
            std::fs::read(fifo).unwrap();
        });
    }

    fn release(path: &Path) {
        let fifo = PathBuf::from(format!("{}.release", path.display()));
        guarded("the release", move || {
            std::fs::write(fifo, b"go\n").unwrap();
        });
    }

    /// The store's packs, as a reader lists them.
    fn listed(git_dir: &Path) -> BTreeSet<String> {
        super::packs(&git_dir.join("objects/pack"))
            .unwrap()
            .into_keys()
            .collect()
    }

    fn entries(git_dir: &Path) -> BTreeSet<std::ffi::OsString> {
        std::fs::read_dir(git_dir.join("objects/pack"))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect()
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

    /// Every object reachable from every ref reads through the `git`
    /// program, which finds objects through the multi-pack index.
    fn git_reads_everything(git_dir: &Path, when: &str) {
        let out = std::process::Command::new("git")
            .env("GIT_DIR", git_dir)
            .args(["rev-list", "--objects", "--all", "--missing=print"])
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        let missing: Vec<&str> = text.lines().filter(|l| l.starts_with('?')).collect();
        assert!(
            out.status.success() && missing.is_empty(),
            "{when}: missing {missing:?}; stderr {}",
            String::from_utf8_lossy(&out.stderr)
        );
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

    fn rolled(ran: Ran<bool>) -> bool {
        match ran {
            Ran::Done(rolled) => rolled,
            Ran::Stopped => panic!("the run stopped"),
        }
    }

    fn expire_now(git_dir: &Path) {
        let slot = Mutex::new(Slot::default());
        assert_eq!(
            expire(Path::new("git"), git_dir, &slot).unwrap(),
            Ran::Done(())
        );
    }

    /// The packs picked are the ones `git repack --geometric=2` rolls up.
    #[test]
    fn a_roll_up_picks_as_geometric_repack_does() {
        assert_eq!(roll_up(&[]), 0);
        assert_eq!(roll_up(&[5]), 0);
        assert_eq!(roll_up(&[1, 1]), 2);
        // Already a progression: nothing.
        assert_eq!(roll_up(&[1, 2, 4, 8]), 0);
        assert_eq!(roll_up(&[10, 100, 1000]), 0);
        // Small packs under a large one: the small ones only.
        assert_eq!(roll_up(&[1, 1, 1, 100]), 3);
        // The large one is under twice the small ones together: all.
        assert_eq!(roll_up(&[3, 3, 10]), 3);
        // A broken progression in the middle takes everything below it.
        assert_eq!(roll_up(&[1, 2, 3, 100, 1000]), 3);
    }

    /// E1: 64 batches are 64 packs; one roll-up writes one more and deletes
    /// none, and the expiry after it leaves a handful. Every object of every
    /// built commit reads throughout, through libgit2 and through git.
    #[test]
    fn a_roll_up_folds_the_batches_packs() {
        let tmp = tempfile::tempdir().unwrap();
        let git_dir = tmp.path().join("history.git");
        let mut v = new_actor(tmp.path());
        script(&git_dir.join(WRAPPER), SKIP);
        batches(&mut v, 0, 64);
        let before = listed(&git_dir);
        assert!(before.len() >= 64, "{} packs", before.len());
        let reachable = read_reachable(&git_dir);
        let objects = all_objects(&git_dir);

        let slot = Mutex::new(Slot::default());
        assert!(rolled(
            repack_once(Path::new("git"), &git_dir, &slot).unwrap()
        ));
        let after = listed(&git_dir);
        assert!(after.is_superset(&before), "a roll-up deletes nothing");
        assert_eq!(after.len(), before.len() + 1);
        assert_eq!(read_reachable(&git_dir), reachable);
        git_reads_everything(&git_dir, "after the roll-up");

        expire_now(&git_dir);
        let left = listed(&git_dir).len();
        assert!(left <= 8, "{} packs became {left}", before.len());
        assert_eq!(read_reachable(&git_dir), reachable);
        git_reads_everything(&git_dir, "after the expiry");
        assert!(all_objects(&git_dir).is_superset(&objects));
        drop(v);
    }

    /// A pack a reader listed is still there after the roll-up that
    /// supersedes it: git and libgit2 look again only once on a miss, so a
    /// pack deleted between a reader's look and its open is an object
    /// reported missing that the store holds (`git cat-file` said `<oid>
    /// missing` in the phase 1 replay when `git repack -d` ran in the store).
    #[test]
    fn a_pack_a_reader_listed_outlives_the_roll_up() {
        let tmp = tempfile::tempdir().unwrap();
        let git_dir = tmp.path().join("history.git");
        let mut v = new_actor(tmp.path());
        script(&git_dir.join(WRAPPER), SKIP);
        batches(&mut v, 0, 8);
        let seen = listed(&git_dir);
        let slot = Mutex::new(Slot::default());
        assert!(rolled(
            repack_once(Path::new("git"), &git_dir, &slot).unwrap()
        ));
        for pack in &seen {
            for ext in ["idx", "pack"] {
                let path = git_dir.join(format!("objects/pack/{pack}.{ext}"));
                assert!(File::open(&path).is_ok(), "{path:?}, listed before the run");
            }
        }
        drop(v);
    }

    /// The defect this design replaced: a roll-up that writes a pack the
    /// store already holds (the same packs rolled up again write the same
    /// pack, its name a hash of its content) must keep it. The old task
    /// set packs aside by name and deleted the live copy, losing history.
    /// git expires a pack only when the index finds no object in it.
    #[test]
    fn a_roll_up_that_writes_a_pack_the_store_holds_keeps_it() {
        let tmp = tempfile::tempdir().unwrap();
        let git_dir = tmp.path().join("history.git");
        let mut v = new_actor(tmp.path());
        script(&git_dir.join(WRAPPER), SKIP);
        batches(&mut v, 0, 8);
        let objects = all_objects(&git_dir);
        let set = listed(&git_dir);
        let names: Vec<&str> = set.iter().map(String::as_str).collect();
        let slot = Mutex::new(Slot::default());
        let git = Path::new("git");
        assert!(rolled(roll(git, &git_dir, &slot, &names).unwrap()));
        let first: BTreeSet<String> = listed(&git_dir).difference(&set).cloned().collect();
        assert_eq!(first.len(), 1, "{first:?}");
        // The same packs again: the same pack, already in the store.
        assert!(rolled(roll(git, &git_dir, &slot, &names).unwrap()));
        let second: BTreeSet<String> = listed(&git_dir).difference(&set).cloned().collect();
        assert_eq!(second, first, "the same packs roll up to the same name");

        expire_now(&git_dir);
        assert_eq!(listed(&git_dir), first, "only the roll-up is left");
        git_reads_everything(&git_dir, "after the expiry");
        assert!(all_objects(&git_dir).is_superset(&objects));
        drop(v);
    }

    /// E2: while git writes a roll-up's pack, the store's `objects/pack` is
    /// as it was, and a snapshot taken before reads every object then, after
    /// the roll-up, and after the expiry.
    #[test]
    fn a_snapshot_reads_through_a_roll_up() {
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
                repack_once(&held, &git_dir, &slot).unwrap()
            })
        };
        wait_written(&held);
        assert_eq!(entries(&git_dir), before, "the store's packs mid-roll-up");
        let reached = read_snapshot(&snapshot);
        release(&held);
        assert!(rolled(guarded("the roll-up", move || run.join().unwrap())));
        assert_eq!(read_snapshot(&snapshot), reached);
        expire_now(&git_dir);
        assert!(listed(&git_dir).len() <= 8, "{:?}", listed(&git_dir));
        assert_eq!(read_snapshot(&snapshot), reached);
        assert_eq!(read_snapshot(&v.history().snapshot()), reached);
        drop(v);
    }

    /// E3: an unreachable blob in a pack of its own is in the store after a
    /// roll-up takes that pack, and after the expiry.
    #[test]
    fn a_roll_up_keeps_unreachable_objects() {
        let tmp = tempfile::tempdir().unwrap();
        let git_dir = tmp.path().join("history.git");
        let mut v = new_actor(tmp.path());
        script(&git_dir.join(WRAPPER), SKIP);
        batches(&mut v, 0, 4);
        let blob = Repository::open_bare(&git_dir)
            .unwrap()
            .blob(b"nothing names this\n")
            .unwrap();
        let mut pack = std::process::Command::new("git")
            .env("GIT_DIR", &git_dir)
            .args(["pack-objects", "-q", "objects/pack/pack"])
            .current_dir(&git_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        writeln!(pack.stdin.take().unwrap(), "{blob}").unwrap();
        assert!(pack.wait().unwrap().success());
        let loose = git_dir.join(format!("objects/{}", &blob.to_string()[..2]));
        std::fs::remove_dir_all(loose).unwrap();
        let objects = all_objects(&git_dir);
        assert!(objects.contains(&blob));

        let slot = Mutex::new(Slot::default());
        assert!(rolled(
            repack_once(Path::new("git"), &git_dir, &slot).unwrap()
        ));
        expire_now(&git_dir);
        assert!(listed(&git_dir).len() < 5, "{:?}", listed(&git_dir));
        let repo = Repository::open_bare(&git_dir).unwrap();
        assert_eq!(
            repo.find_blob(blob).unwrap().content(),
            b"nothing names this\n"
        );
        assert!(all_objects(&git_dir).is_superset(&objects));
        drop(v);
    }

    /// E4: the task rolls up at a built event; a shutdown with that roll-up
    /// held returns, leaving no temporary file after the reopen, and every
    /// object reads.
    #[test]
    fn a_stop_kills_a_held_roll_up() {
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
        for entry in std::fs::read_dir(git_dir.join("objects/pack")).unwrap() {
            let name = entry.unwrap().file_name();
            let name = name.to_string_lossy();
            assert!(
                !name.starts_with(".tmp-") && !name.starts_with("tmp_"),
                "{name:?} is left"
            );
        }
        drop(v);
    }

    /// What a roll-up superseded when the volume stopped is gone once it
    /// opens again, and every object reads: a restart neither keeps the
    /// copies nor rolls them up again into a third.
    #[test]
    fn a_stops_copies_go_at_the_next_open() {
        let tmp = tempfile::tempdir().unwrap();
        let git_dir = tmp.path().join("history.git");
        let mut v = new_actor(tmp.path());
        script(&git_dir.join(WRAPPER), SKIP);
        batches(&mut v, 0, 16);
        let reachable = read_reachable(&git_dir);
        let objects = all_objects(&git_dir);
        let packs = super::packs(&git_dir.join("objects/pack")).unwrap();
        let mut by_size: Vec<(u64, String)> = packs.into_iter().map(|(n, s)| (s, n)).collect();
        by_size.sort();
        let sizes: Vec<u64> = by_size.iter().map(|(size, _)| *size).collect();
        let picked: BTreeSet<String> = by_size[..roll_up(&sizes)]
            .iter()
            .map(|(_, name)| name.clone())
            .collect();
        assert!(picked.len() >= 2);
        let before = listed(&git_dir);
        // The task's state at a stop: a roll-up, no built event after it.
        let slot = Mutex::new(Slot::default());
        assert!(rolled(
            repack_once(Path::new("git"), &git_dir, &slot).unwrap()
        ));
        drop(v);
        assert!(
            listed(&git_dir).is_superset(&before),
            "a stop deletes nothing"
        );

        std::fs::remove_file(git_dir.join(WRAPPER)).unwrap();
        let v = reopen(tmp.path());
        let left = listed(&git_dir);
        assert!(left.is_disjoint(&picked), "{picked:?} left: {left:?}");
        assert_eq!(read_reachable(&git_dir), reachable);
        git_reads_everything(&git_dir, "after the reopen");
        assert!(all_objects(&git_dir).is_superset(&objects));
        drop(v);
    }

    /// A stale `multi-pack-index.lock` (a stop killed a write) goes at the
    /// next open, so the task's writes do not fail for good.
    #[test]
    fn a_stale_index_lock_goes_at_the_next_open() {
        let tmp = tempfile::tempdir().unwrap();
        let git_dir = tmp.path().join("history.git");
        let mut v = new_actor(tmp.path());
        script(&git_dir.join(WRAPPER), SKIP);
        batches(&mut v, 0, 4);
        drop(v);
        let lock = git_dir.join("objects/pack/multi-pack-index.lock");
        std::fs::write(&lock, b"").unwrap();
        let v = reopen(tmp.path());
        assert!(!lock.exists());
        let slot = Mutex::new(Slot::default());
        assert!(rolled(
            repack_once(Path::new("git"), &git_dir, &slot).unwrap()
        ));
        drop(v);
    }

    /// Batches built while the task rolls up beside them: none fails,
    /// though a roll-up may take a batch's pack, and expire it, before the
    /// batch fsyncs it (history/feed.rs).
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
        git_reads_everything(&git_dir, "after the reopen");
        drop(v);
    }

    /// Forks landing and dropped beside the task, with epoch moves that are
    /// not built events: every object of every ref stays readable. The old
    /// task lost objects here (a roll-up wrote a pack with the name of one
    /// it had set aside, then deleted it): 2 to 6 runs in 10 to 20.
    #[test]
    #[allow(
        clippy::disallowed_methods,
        reason = "the actor's own fork drop, no row to write"
    )]
    fn forks_beside_the_task_keep_every_object() {
        let tmp = tempfile::tempdir().unwrap();
        let git_dir = tmp.path().join("history.git");
        let mut v = new_actor(tmp.path());
        let w = writer("w");
        let put = |v: &mut TreeActor, tree: TreeId, path: &str, bytes: &[u8]| {
            let create = v.node(tree, path).is_err();
            let h = v
                .write_open(tree, path, &w, create.then_some(ATTRS))
                .unwrap();
            v.truncate(h, 0).unwrap();
            v.write(h, 0, bytes, false).unwrap();
            v.close(h).unwrap();
        };
        put(&mut v, TreeId::Root, "shared.txt", b"from the source");
        v.settle().unwrap();
        for i in 0..40 {
            let fork = rowless_fork(&mut v);
            put(
                &mut v,
                fork,
                "f.txt",
                format!("from the fork {i}").as_bytes(),
            );
            v.settle().unwrap();
            while v.progress().unwrap() {}
            v.settle().unwrap();
            git_reads_everything(&git_dir, &format!("iteration {i} after the landing"));
            v.history.wake_waiters();
            v.drop_fork(fork, |_| Ok(Vec::new())).unwrap();
            v.history.wake_waiters();
            put(&mut v, TreeId::Root, &format!("after-{i}"), b"after");
            v.history.wake_waiters();
            v.settle().unwrap();
            git_reads_everything(&git_dir, &format!("iteration {i} after the drop"));
        }
        drop(v);
        let v = reopen(tmp.path());
        git_reads_everything(&git_dir, "after the reopen");
        drop(v);
    }

    /// A killed run's temporary files go at the next open.
    #[test]
    fn clean_packs_removes_a_killed_runs_files() {
        let tmp = tempfile::tempdir().unwrap();
        let pack_dir = tmp.path().join("objects/pack");
        std::fs::create_dir_all(&pack_dir).unwrap();
        let tmp_pack = pack_dir.join(".tmp-123-pack-abc.pack");
        std::fs::write(&tmp_pack, b"").unwrap();
        crate::tree_actor::history::clean_packs(tmp.path()).unwrap();
        assert!(!tmp_pack.exists());
    }

    /// A failed git command is an error the task logs, and the next event
    /// runs another.
    #[test]
    fn a_failed_run_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let fails = tmp.path().join("fails");
        script(&fails, "echo broken >&2; exit 3");
        let slot = Mutex::new(Slot::default());
        let error = expire(&fails, tmp.path(), &slot).unwrap_err();
        assert!(error.to_string().contains("broken"), "{error}");
        // A stop asked first: nothing starts.
        lock(&slot).stopped = true;
        assert_eq!(expire(&fails, tmp.path(), &slot).unwrap(), Ran::Stopped);
    }
}
