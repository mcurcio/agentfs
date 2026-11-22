//! The tree history's repack, a task of its own (docs/trees.md, "History
//! trails the files"). Each batch the builder installs is one pack, and a
//! read's cost grows with the pack count, so at each built event the task
//! rolls the small packs up, one run at a time, with the memory bounds of
//! the recording engine's repack ([`crate::repack::PACK_CONFIG`]). It runs
//! beside `git fast-import`, as a git server repacks beside receive-pack,
//! never under the builder's lease and never stopping it. Only a built
//! event starts a run ([`History::wait_batch_end`]), not a failed batch or a
//! wake; events that come while a run is in progress are one more run after
//! it, not one each.
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
//! if one look of its spans a whole built batch. Each run expires what the
//! last one superseded before it rolls up, so at most one roll-up's packs
//! wait to go.
//!
//! **A failed step.** An expiry deletes its packs before it writes the index
//! that leaves them out, and every index write starts from the index it
//! reads, so after any failed step the index is in doubt: kept, a later
//! write over it exits 0 yet leaves an index naming gone packs (git 2.43) or
//! pointing into the wrong pack (git 2.47), which `git rev-list` and `git
//! fast-import` die on. The task removes the index and its lock at once
//! ([`Midx::InDoubt`]), and its next run only writes the index afresh,
//! preferring the last roll-up it wrote ([`Memory`]), so no roll-up is
//! written while a failing index write would leave it unexpired; the run
//! after expires what that rewrite left unreferenced.
//!
//! **Room and strays.** A roll-up runs only with [`ROOM`] times the size of
//! the packs it takes free, so it never fills the disk the builder and the
//! journal write to (a recording failure): too little is said and the
//! roll-up skipped. A git that dies mid-write (out of space, killed) leaves
//! its temporary pack (`tmp_*`); the task lists those at each batch end and
//! removes, at the next, each still there as the same file ([`sweep`]).
//!
//! **A stop.** A stop kills a run in progress, with every process it
//! started (its own process group), but lets an expiry finish. git writes
//! a pack under a temporary name and renames it in, index last, so a killed
//! run leaves at most a temporary file, which the next start removes
//! (history/mod.rs, `clean_packs`), or a whole new pack the index does not
//! prefer, whose packs a later run rolls up again. A stale
//! `multi-pack-index.lock` goes at the next start with the other locks
//! (`remove_stale_locks`). An expiry deletes its packs before it writes the
//! index that leaves them out, so a power cut or a kill of the daemon
//! between the two leaves an index naming packs that are gone, which `git
//! fsck` fails on: the next start removes such an index ([`repair`]). The start then expires what the last roll-up
//! superseded before the volume is served, so a stop leaves no copy
//! behind. git fsyncs what it writes
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

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use super::built::{Ends, History};
use crate::repack::PACK_CONFIG;
use crate::tree_actor::stages::{Timer, Which};

/// The task's child and whether a stop was asked, behind one lock: a stop
/// and a start never cross, so no git command starts after a stop.
#[derive(Default)]
struct Slot {
    child: Option<Child>,
    /// The child is an expiry, which a stop lets finish rather than kill.
    finishing: bool,
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

/// What the task knows of the multi-pack index, which decides what a run
/// may do: an expiry reads only an index this process trusts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Midx {
    /// Whole: this process wrote it, or found it whole at start, or there is
    /// none. `pending`: a roll-up or a rewrite left packs it finds no object
    /// in, which the next run expires.
    Trusted { pending: bool },
    /// A step failed, so the index may name packs that are gone or point
    /// into the wrong pack (git deletes an expiry's packs before it writes
    /// the index, and writes an index over the one it read). The next run
    /// drops it and writes it afresh, and nothing reads it before.
    InDoubt,
}

/// What the task carries from one run to the next.
#[derive(Debug)]
struct Memory {
    midx: Midx,
    /// The last roll-up this process wrote (`pack-<hash>`): a rewrite of
    /// the index prefers it, so the packs it rolled up hold no object the
    /// index finds there, whichever of them git would pick by its own order
    /// (the pack's mtime in seconds, then its name).
    rolled: Option<String>,
}

impl Default for Memory {
    fn default() -> Self {
        Self {
            midx: Midx::Trusted { pending: false },
            rolled: None,
        }
    }
}

/// The temporary files (`tmp_*`) listed in `objects/pack` once, each with
/// its inode: a git that died mid-write (a roll-up out of space, a killed
/// `fast-import`) leaves one, and nothing renames it. Each is removed at the
/// next batch end if it is still there, the same file ([`sweep`]).
#[derive(Debug, Default)]
struct Strays(BTreeSet<(OsString, u64)>);

/// The events the task runs on: History's batch ends ([`History::ends`]).
trait BatchEvents {
    /// Returns once more than `after` batches have ended, built or failed;
    /// `None` once the volume is stopping.
    fn wait_batch_end(&self, after: u64) -> Option<Ends>;
    /// The batches ended so far.
    fn ends(&self) -> Ends;
}

impl BatchEvents for History {
    fn wait_batch_end(&self, after: u64) -> Option<Ends> {
        History::wait_batch_end(self, after)
    }

    fn ends(&self) -> Ends {
        History::ends(self)
    }
}

/// How much free space a roll-up needs, as a multiple of the packs it
/// rolls up: its pack, and as much again left for the builder and the
/// journal, so a roll-up never fills the disk (a recording failure).
const ROOM: u64 = 2;

/// Why one run failed: the task logs it and tries again at the next event.
#[derive(Debug, thiserror::Error)]
enum TaskError {
    /// Too little free space for the roll-up: nothing was written, and the
    /// index is as whole as before.
    #[error("no room for a roll-up: it needs {needed} bytes free, {free} are")]
    NoRoom { needed: u64, free: u64 },
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
    /// History's batch ends ([`History::wait_batch_end`]) and repacks
    /// `git_dir` after each built one, until History stops.
    ///
    /// # Errors
    ///
    /// The thread could not be started.
    pub(in crate::tree_actor) fn start(history: History, git_dir: PathBuf) -> io::Result<Self> {
        let child = Arc::new(Mutex::new(Slot::default()));
        // Before the volume is served: an index left naming a pack an
        // expiry deleted goes, then what the last process's last roll-up
        // superseded goes. A failure is said, the index dropped, and the
        // volume served (keep operating).
        let mut memory = Memory::default();
        let started = repair(&git_dir).and_then(|()| expire(&program(&git_dir), &git_dir, &child));
        if let Err(error) = started {
            eprintln!("tree RepackFailed: {error}");
            memory.midx = in_doubt(&git_dir);
        }
        let slot = Arc::clone(&child);
        let thread = std::thread::Builder::new()
            .name("tree-repack".into())
            .spawn(move || run(&history, &git_dir, &slot, memory))?;
        Ok(Self { thread, child })
    }

    /// Waits for the task to end: History has stopped, and a git command in
    /// progress was killed, or finished if it is an expiry (see the module
    /// docs). Called after History stops (`TreeActor::shut_down`).
    pub(in crate::tree_actor) fn join(self) {
        {
            let mut slot = lock(&self.child);
            slot.stopped = true;
            if let Some(child) = &slot.child
                && !slot.finishing
            {
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

/// The task: at each batch end, the strays listed at the one before go
/// ([`sweep`]); at each built one, one run. Ends that come while a run is
/// in progress are one more run after it, not one each. A failed run is
/// logged when it differs from the last and tried again at the next built
/// event (keep operating).
fn run<E: BatchEvents>(events: &E, git_dir: &Path, slot: &Mutex<Slot>, mut memory: Memory) {
    let pack_dir = git_dir.join("objects").join("pack");
    let mut last: Option<String> = None;
    // Listed before the counts are read: a batch that ends between the two
    // is counted, so its files are swept only after a later end.
    let mut strays = observe(&pack_dir).unwrap_or_default();
    let mut seen = events.ends();
    loop {
        let Some(now) = events.wait_batch_end(seen.ended) else {
            return;
        };
        let mut ran = sweep(&pack_dir, &strays)
            .map(|()| Ran::Done(()))
            .map_err(io_error("sweeping", &pack_dir));
        if ran.is_ok() && now.built > seen.built {
            ran = run_once(&program(git_dir), git_dir, slot, &mut memory);
        }
        match ran {
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
        strays = observe(&pack_dir).unwrap_or_default();
        seen = events.ends();
    }
}

/// One run, on a built event after the last run ended. With the index in
/// doubt it drops the index and writes it afresh, nothing else. With it
/// trusted, it expires what the last run left unreferenced, then rolls up,
/// whose superseded packs the next run expires: never in the same run, so
/// a reader that listed a pack before the roll-up has a built batch's time
/// to open it. A failed step leaves the index in doubt, dropped at once so
/// no later step reads it; too little room for a roll-up does not.
fn run_once(
    git: &Path,
    git_dir: &Path,
    slot: &Mutex<Slot>,
    memory: &mut Memory,
) -> Result<Ran<()>, TaskError> {
    let ran = steps(git, git_dir, slot, memory);
    if let Err(error) = &ran
        && !matches!(error, TaskError::NoRoom { .. })
    {
        memory.midx = in_doubt(git_dir);
    }
    ran
}

/// [`run_once`]'s steps, each moving `memory` on as it ends.
fn steps(
    git: &Path,
    git_dir: &Path,
    slot: &Mutex<Slot>,
    memory: &mut Memory,
) -> Result<Ran<()>, TaskError> {
    match memory.midx {
        Midx::InDoubt => {
            drop_index(git_dir)?;
            let listed = packs(&git_dir.join("objects").join("pack"))?;
            if listed.is_empty() {
                // `multi-pack-index write` fails on a store without packs.
                memory.midx = Midx::Trusted { pending: false };
                return Ok(Ran::Done(()));
            }
            let preferred = memory
                .rolled
                .as_ref()
                .filter(|name| listed.contains_key(*name))
                .map(|name| format!("--preferred-pack={name}.pack"));
            let mut write = vec!["multi-pack-index", "write"];
            write.extend(preferred.as_deref());
            if command(git, git_dir, slot, &write, None, false)? == Ran::Stopped {
                return Ok(Ran::Stopped);
            }
            sync_store(git_dir)?;
            // The rewrite finds no object in what the last roll-up took.
            memory.midx = Midx::Trusted { pending: true };
        }
        Midx::Trusted { pending } => {
            if pending {
                if expire(git, git_dir, slot)? == Ran::Stopped {
                    return Ok(Ran::Stopped);
                }
                memory.midx = Midx::Trusted { pending: false };
            }
            match roll_up_once(git, git_dir, slot, &mut memory.rolled)? {
                Ran::Done(rolled) => memory.midx = Midx::Trusted { pending: rolled },
                Ran::Stopped => return Ok(Ran::Stopped),
            }
        }
    }
    Ok(Ran::Done(()))
}

/// Drops the index after a failed step: the index is in doubt. A drop that
/// fails is said; the next run tries it again before anything else.
fn in_doubt(git_dir: &Path) -> Midx {
    if let Err(error) = drop_index(git_dir) {
        eprintln!("tree RepackFailed: {error}");
    }
    Midx::InDoubt
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
        match command(
            git,
            git_dir,
            slot,
            &["multi-pack-index", "expire"],
            None,
            true,
        )? {
            Ran::Done(_) => Ran::Done(()),
            Ran::Stopped => Ran::Stopped,
        },
    )
}

/// The multi-pack index of `git_dir`'s store.
fn index_path(git_dir: &Path) -> PathBuf {
    git_dir
        .join("objects")
        .join("pack")
        .join("multi-pack-index")
}

/// The packs the multi-pack index names (`pack-<hash>`), from its `PNAM`
/// chunk (git's `gitformat-pack`, "multi-pack-index files"): `None` when
/// there is no index or it does not read as one.
fn indexed(git_dir: &Path) -> Option<Vec<String>> {
    let bytes = std::fs::read(index_path(git_dir)).ok()?;
    let be32 = |at: usize| -> Option<u32> {
        Some(u32::from_be_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
    };
    let be64 = |at: usize| -> Option<usize> {
        usize::try_from(u64::from_be_bytes(bytes.get(at..at + 8)?.try_into().ok()?)).ok()
    };
    if bytes.get(..4)? != b"MIDX" {
        return None;
    }
    let chunks = usize::from(*bytes.get(6)?);
    let packs = usize::try_from(be32(8)?).ok()?;
    let (mut start, mut end) = (None, None);
    for i in 0..chunks {
        let entry = 12 + 12 * i;
        if bytes.get(entry..entry + 4)? == b"PNAM" {
            start = Some(be64(entry + 4)?);
            end = Some(be64(entry + 16)?);
        }
    }
    let names = bytes.get(start?..end?)?;
    let names: Vec<String> = names
        .split(|b| *b == 0)
        .filter(|name| !name.is_empty())
        .map(|name| {
            let name = std::str::from_utf8(name).ok()?;
            Some(name.strip_suffix(".idx").unwrap_or(name).to_owned())
        })
        .collect::<Option<_>>()?;
    (names.len() == packs).then_some(names)
}

/// Removes the multi-pack index when it names a pack whose files are gone:
/// `git multi-pack-index expire` deletes the packs before it writes the
/// index that leaves them out, so a power cut or a kill between the two
/// leaves an index `git fsck` and every later index write fail on. Readers
/// find every object without it, by listing `objects/pack`. What that
/// expiry had yet to delete is rolled up again by a later run, once.
fn repair(git_dir: &Path) -> Result<(), TaskError> {
    let Some(names) = indexed(git_dir) else {
        return Ok(());
    };
    let store = git_dir.join("objects").join("pack");
    let whole = |name: &String| {
        ["idx", "pack"]
            .iter()
            .all(|ext| store.join(format!("{name}.{ext}")).exists())
    };
    if names.iter().all(whole) {
        return Ok(());
    }
    drop_index(git_dir)
}

/// Removes the multi-pack index and its lock, and fsyncs `objects/pack`:
/// readers find every object without it, by listing the packs. Only this
/// task writes the index, and its git has exited, so the lock is stale.
fn drop_index(git_dir: &Path) -> Result<(), TaskError> {
    let index = index_path(git_dir);
    for path in [index.with_extension("lock"), index] {
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(io_error("removing", &path)(e)),
        }
    }
    sync_store(git_dir)
}

/// Fsyncs `objects/pack`: the names in it are durable.
fn sync_store(git_dir: &Path) -> Result<(), TaskError> {
    let store = git_dir.join("objects").join("pack");
    File::open(&store)
        .and_then(|dir| dir.sync_all())
        .map_err(io_error("syncing", &store))
}

/// Whether `free` bytes leave room for a roll-up of `size` bytes: [`ROOM`]
/// times its size.
fn has_room(size: u64, free: u64) -> bool {
    free >= ROOM.saturating_mul(size)
}

/// The bytes free to this process on `dir`'s filesystem. A test stands a
/// figure in the git directory (`dir`'s grandparent) in its place.
fn free_bytes(dir: &Path) -> io::Result<u64> {
    #[cfg(test)]
    if let Some(git_dir) = dir.parent().and_then(Path::parent)
        && let Ok(text) = std::fs::read_to_string(git_dir.join(tests::FREE))
    {
        return text.trim().parse().map_err(io::Error::other);
    }
    let stat = rustix::fs::statvfs(dir)?;
    Ok(stat.f_bavail.saturating_mul(stat.f_frsize))
}

/// The temporary files in `dir` now, with their inodes.
fn observe(dir: &Path) -> io::Result<Strays> {
    let mut strays = Strays::default();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        if !name.as_encoded_bytes().starts_with(b"tmp_") {
            continue;
        }
        match entry.metadata() {
            Ok(meta) => {
                strays.0.insert((name, meta.ino()));
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(strays)
}

/// Removes each of `earlier` still in `dir` as the same file (its inode),
/// at a batch end after they were listed. No writer holds one then: a
/// temporary file is the builder's `fast-import`, one batch at a time,
/// which renames it or is killed and reaped before its batch ends
/// (history/feed.rs), or this task's git, which has exited.
fn sweep(dir: &Path, earlier: &Strays) -> io::Result<()> {
    for (name, ino) in &earlier.0 {
        let path = dir.join(name);
        match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.ino() == *ino => match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            },
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
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
/// packs it rolled up expire at a later run. The pack written, if any, is
/// `rolled`, even when the index write after it fails.
fn roll_up_once(
    git: &Path,
    git_dir: &Path,
    slot: &Mutex<Slot>,
    rolled: &mut Option<String>,
) -> Result<Ran<bool>, TaskError> {
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
    let size: u64 = sizes[..n].iter().sum();
    let store = git_dir.join("objects").join("pack");
    let free = free_bytes(&store).map_err(io_error("measuring", &store))?;
    if !has_room(size, free) {
        return Err(TaskError::NoRoom {
            needed: ROOM.saturating_mul(size),
            free,
        });
    }
    let names: Vec<&str> = by_size[..n].iter().map(|(_, name)| name.as_str()).collect();
    roll(git, git_dir, slot, &names, rolled)
}

/// Writes the packs `names` into one new pack, which `rolled` then names,
/// and makes the multi-pack index prefer it: true when there was something
/// to write.
fn roll(
    git: &Path,
    git_dir: &Path,
    slot: &Mutex<Slot>,
    names: &[&str],
    rolled: &mut Option<String>,
) -> Result<Ran<bool>, TaskError> {
    let mut input = String::new();
    for name in names {
        input.push_str(name);
        input.push_str(".pack\n");
    }
    let out = match command(
        git,
        git_dir,
        slot,
        &PACK_OBJECTS,
        Some(input.as_bytes()),
        false,
    )? {
        Ran::Done(out) => out,
        Ran::Stopped => return Ok(Ran::Stopped),
    };
    let hash = String::from_utf8_lossy(&out).trim().to_owned();
    if hash.is_empty() {
        return Ok(Ran::Done(false));
    }
    *rolled = Some(format!("pack-{hash}"));
    let preferred = format!("--preferred-pack=pack-{hash}.pack");
    let write = ["multi-pack-index", "write", preferred.as_str()];
    if command(git, git_dir, slot, &write, None, false)? == Ran::Stopped {
        return Ok(Ran::Stopped);
    }
    // The new pack's name is durable before any pack it rolled up can go.
    sync_store(git_dir)?;
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
/// a stop kills every process it started, unless it is `finishing` (an
/// expiry), which a stop waits for. The child is reaped only after
/// the slot lets go of it, so a stop never signals a process id reused
/// since.
fn command(
    git: &Path,
    git_dir: &Path,
    slot: &Mutex<Slot>,
    args: &[&str],
    input: Option<&[u8]>,
    finishing: bool,
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
    held.finishing = finishing;
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

    /// The free bytes the task sees when the git directory holds this file.
    pub(in crate::tree_actor) const FREE: &str = "repack-free";

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

    /// An executable script at `path`, written by a child process and
    /// renamed into place. Never through a descriptor of this process: a
    /// child any other test's thread forks shares every descriptor open
    /// here until its exec, and a file some process holds open for writing
    /// cannot be run (`ETXTBSY`), so a wrapper written here could fail to
    /// spawn for as long as such a child had not yet reached its exec.
    fn script(path: &Path, body: &str) {
        let tmp = path.with_extension("tmp");
        let mut writer = std::process::Command::new("sh")
            .args(["-c", r#"cat > "$1" && chmod 755 "$1""#, "sh"])
            .arg(&tmp)
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdin = writer.stdin.take().unwrap();
        write!(stdin, "#!/bin/sh\n{body}\n").unwrap();
        drop(stdin);
        assert!(writer.wait().unwrap().success());
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

    /// `git fsck --full --strict` passes, the multi-pack index's own check
    /// included: what the power-loss replay asks of each checkpoint. Only
    /// on a stopped volume, where no run moves a pack meanwhile.
    fn fsck(git_dir: &Path, when: &str) {
        let out = std::process::Command::new("git")
            .env("GIT_DIR", git_dir)
            .args(["fsck", "--full", "--strict", "--no-progress"])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{when}: git fsck failed ({}): {}{}; objects/pack {:?}",
            out.status,
            String::from_utf8_lossy(&out.stderr),
            String::from_utf8_lossy(&out.stdout),
            entries(git_dir)
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

    /// The packs a roll-up of the store would take now.
    fn picked(git_dir: &Path) -> BTreeSet<String> {
        let packs = super::packs(&git_dir.join("objects/pack")).unwrap();
        let mut by_size: Vec<(u64, String)> = packs.into_iter().map(|(n, s)| (s, n)).collect();
        by_size.sort();
        let sizes: Vec<u64> = by_size.iter().map(|(size, _)| *size).collect();
        by_size[..roll_up(&sizes)]
            .iter()
            .map(|(_, name)| name.clone())
            .collect()
    }

    /// One roll-up, as a run makes it.
    fn repack_once(git: &Path, git_dir: &Path, slot: &Mutex<Slot>) -> Result<Ran<bool>, TaskError> {
        roll_up_once(git, git_dir, slot, &mut None)
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
        fsck(&git_dir, "after the stop");
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
        fsck(&git_dir, "after the stop");
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
        assert!(rolled(
            roll(git, &git_dir, &slot, &names, &mut None).unwrap()
        ));
        let first: BTreeSet<String> = listed(&git_dir).difference(&set).cloned().collect();
        assert_eq!(first.len(), 1, "{first:?}");
        // The same packs again: the same pack, already in the store.
        assert!(rolled(
            roll(git, &git_dir, &slot, &names, &mut None).unwrap()
        ));
        let second: BTreeSet<String> = listed(&git_dir).difference(&set).cloned().collect();
        assert_eq!(second, first, "the same packs roll up to the same name");

        expire_now(&git_dir);
        assert_eq!(listed(&git_dir), first, "only the roll-up is left");
        git_reads_everything(&git_dir, "after the expiry");
        assert!(all_objects(&git_dir).is_superset(&objects));
        drop(v);
        fsck(&git_dir, "after the stop");
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
        fsck(&git_dir, "after the stop");
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
        // The blob's loose file only: the verification notes are loose
        // objects too, and may share its directory.
        let hex = blob.to_string();
        std::fs::remove_file(git_dir.join(format!("objects/{}/{}", &hex[..2], &hex[2..]))).unwrap();
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
        fsck(&git_dir, "after the stop");
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
        fsck(&git_dir, "after the killed run");

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
        fsck(&git_dir, "after the stop");
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
        let picked = picked(&git_dir);
        assert!(picked.len() >= 2);
        let before = listed(&git_dir);
        // The task's state at a stop: a roll-up, no built event after it.
        let slot = Mutex::new(Slot::default());
        assert!(rolled(
            repack_once(Path::new("git"), &git_dir, &slot).unwrap()
        ));
        drop(v);
        fsck(&git_dir, "after the stop");
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
        fsck(&git_dir, "after the stop");
    }

    /// An expiry cut short between its deletes and its index write (a power
    /// cut: git deletes the packs first) leaves an index naming a pack that
    /// is gone, which `git fsck` fails on. The next open removes that index,
    /// and the store passes `git fsck` and reads whole.
    #[test]
    fn an_index_naming_a_deleted_pack_goes_at_the_next_open() {
        let tmp = tempfile::tempdir().unwrap();
        let git_dir = tmp.path().join("history.git");
        let mut v = new_actor(tmp.path());
        script(&git_dir.join(WRAPPER), SKIP);
        batches(&mut v, 0, 8);
        let reachable = read_reachable(&git_dir);
        let before = listed(&git_dir);
        let slot = Mutex::new(Slot::default());
        assert!(rolled(
            repack_once(Path::new("git"), &git_dir, &slot).unwrap()
        ));
        drop(v);
        let named = indexed(&git_dir).expect("the index reads");
        assert_eq!(
            named.iter().cloned().collect::<BTreeSet<_>>(),
            listed(&git_dir),
            "the index names every pack"
        );
        // Half an expiry: one superseded pack deleted, the index as it was.
        let gone = before.iter().next().unwrap();
        for ext in ["idx", "pack", "rev"] {
            let _ = std::fs::remove_file(git_dir.join(format!("objects/pack/{gone}.{ext}")));
        }
        let broken = std::process::Command::new("git")
            .env("GIT_DIR", &git_dir)
            .args(["fsck", "--full", "--strict", "--no-progress"])
            .output()
            .unwrap();
        assert!(!broken.status.success(), "git fsck fails on such an index");

        std::fs::remove_file(git_dir.join(WRAPPER)).unwrap();
        let v = reopen(tmp.path());
        assert_eq!(read_reachable(&git_dir), reachable);
        drop(v);
        fsck(&git_dir, "after the reopen");
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
        fsck(&git_dir, "after the stop");
        let lock = git_dir.join("objects/pack/multi-pack-index.lock");
        std::fs::write(&lock, b"").unwrap();
        let v = reopen(tmp.path());
        assert!(!lock.exists());
        let slot = Mutex::new(Slot::default());
        assert!(rolled(
            repack_once(Path::new("git"), &git_dir, &slot).unwrap()
        ));
        drop(v);
        fsck(&git_dir, "after the stop");
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
        fsck(&git_dir, "after the stop");
        let v = reopen(tmp.path());
        assert_eq!(read_reachable(&git_dir), reachable);
        git_reads_everything(&git_dir, "after the reopen");
        drop(v);
        fsck(&git_dir, "after the stop");
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
        fsck(&git_dir, "after the stop");
        let v = reopen(tmp.path());
        git_reads_everything(&git_dir, "after the reopen");
        drop(v);
        fsck(&git_dir, "after the stop");
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

    /// A wrapper that logs each command's arguments to `$0.log`, then runs
    /// git.
    const LOG: &str = r#"echo "$*" >> "$0.log"
exec git "$@""#;

    /// A wrapper that logs as [`LOG`] does and runs `pack-objects` unable to
    /// write a byte (a full disk): it dies leaving its temporary pack.
    const FULL: &str = r#"echo "$*" >> "$0.log"
case " $* " in *" pack-objects "*) trap '' XFSZ; ulimit -f 0;; esac
exec git "$@""#;

    /// A wrapper whose `multi-pack-index expire` cannot write the index (a
    /// full disk): git deletes the packs, then fails.
    const EXPIRY_FULL: &str = r#"case " $* " in *" expire "*) trap '' XFSZ; ulimit -f 0;; esac
exec git "$@""#;

    /// The commands a logging wrapper at `git_dir`'s [`WRAPPER`] ran that
    /// contain `what`.
    fn ran(git_dir: &Path, what: &str) -> usize {
        std::fs::read_to_string(git_dir.join(format!("{WRAPPER}.log")))
            .unwrap_or_default()
            .lines()
            .filter(|line| line.contains(what))
            .count()
    }

    /// History's batch ends as a script: each wait runs `check` with its
    /// number, then returns the ends now if they are past `after` (a batch
    /// ended meanwhile), else the next ends, or `None` when the script is
    /// out. `meanwhile`, if set, runs once after the [`BatchEvents::ends`]
    /// read of the number it holds: what happens between that read and the
    /// task's next step.
    struct Script<F: FnMut(usize)> {
        ends: Mutex<(Vec<Ends>, Ends, usize)>,
        check: Mutex<F>,
        reads: Mutex<usize>,
        meanwhile: Mutex<Option<(usize, Meanwhile)>>,
    }

    /// What happens to History's counts between a read and the next step.
    type Meanwhile = Box<dyn FnOnce(&mut Ends) + Send>;

    impl<F: FnMut(usize)> Script<F> {
        fn new(ends: &[(u64, u64)], check: F) -> Self {
            let mut ends: Vec<Ends> = ends
                .iter()
                .map(|&(built, ended)| Ends { built, ended })
                .collect();
            ends.reverse();
            Self {
                ends: Mutex::new((ends, Ends::default(), 0)),
                check: Mutex::new(check),
                reads: Mutex::new(0),
                meanwhile: Mutex::new(None),
            }
        }
    }

    impl<F: FnMut(usize)> BatchEvents for Script<F> {
        fn wait_batch_end(&self, after: u64) -> Option<Ends> {
            let mut ends = self.ends.lock().unwrap();
            ends.2 += 1;
            (self.check.lock().unwrap())(ends.2);
            if ends.1.ended > after {
                return Some(ends.1);
            }
            let next = ends.0.pop()?;
            ends.1 = next;
            Some(next)
        }

        fn ends(&self) -> Ends {
            let mut ends = self.ends.lock().unwrap();
            let now = ends.1;
            let mut reads = self.reads.lock().unwrap();
            *reads += 1;
            let mut meanwhile = self.meanwhile.lock().unwrap();
            if meanwhile.as_ref().is_some_and(|(at, _)| *at == *reads)
                && let Some((_, then)) = meanwhile.take()
            {
                then(&mut ends.1);
            }
            now
        }
    }

    /// A stopped volume's store holding `n` batches' packs.
    fn stopped_store(tmp: &Path, n: usize) -> PathBuf {
        let git_dir = tmp.join("history.git");
        let mut v = new_actor(tmp);
        script(&git_dir.join(WRAPPER), SKIP);
        batches(&mut v, 0, n);
        drop(v);
        std::fs::remove_file(git_dir.join(WRAPPER)).unwrap();
        git_dir
    }

    fn tmp_files(git_dir: &Path) -> BTreeSet<std::ffi::OsString> {
        entries(git_dir)
            .into_iter()
            .filter(|name| name.as_encoded_bytes().starts_with(b"tmp_"))
            .collect()
    }

    /// A roll-up's packs go at a run on a built event after the roll-up's
    /// run, never in the roll-up's own run and never on a failed batch's
    /// end: a reader that listed a pack before the roll-up has a built
    /// batch's time to open it.
    #[test]
    fn the_expiry_waits_for_a_built_event_after_the_roll_up() {
        let tmp = tempfile::tempdir().unwrap();
        let git_dir = stopped_store(tmp.path(), 16);
        let before = listed(&git_dir);
        let reachable = read_reachable(&git_dir);
        script(&git_dir.join(WRAPPER), LOG);
        let script = {
            let (git_dir, before) = (git_dir.clone(), before.clone());
            Script::new(&[(1, 1), (1, 2), (2, 3)], move |wait| match wait {
                1 => assert_eq!(ran(&git_dir, ""), 0),
                // The roll-up's run: a pack written, nothing deleted.
                2 | 3 => {
                    assert_eq!(ran(&git_dir, "pack-objects"), 1, "wait {wait}");
                    assert_eq!(ran(&git_dir, " expire"), 0, "wait {wait}");
                    let now = listed(&git_dir);
                    assert!(now.is_superset(&before), "wait {wait}: a pack went");
                    assert_eq!(now.len(), before.len() + 1, "wait {wait}");
                }
                // The next built event's run expires them.
                _ => {
                    assert_eq!(ran(&git_dir, " expire"), 1);
                    assert!(listed(&git_dir).len() <= 8, "{:?}", listed(&git_dir));
                }
            })
        };
        let slot = Mutex::new(Slot::default());
        run(&script, &git_dir, &slot, Memory::default());
        assert_eq!(script.ends.lock().unwrap().2, 4);
        assert_eq!(read_reachable(&git_dir), reachable);
        git_reads_everything(&git_dir, "after the expiry");
        fsck(&git_dir, "after the expiry");
    }

    /// A roll-up that runs out of space leaves git's temporary pack, which
    /// goes at the next batch end, built or failed, without a restart.
    #[test]
    fn a_failed_roll_ups_temporary_pack_goes_at_the_next_batch_end() {
        let tmp = tempfile::tempdir().unwrap();
        let git_dir = stopped_store(tmp.path(), 8);
        let reachable = read_reachable(&git_dir);
        assert!(tmp_files(&git_dir).is_empty());
        script(&git_dir.join(WRAPPER), FULL);
        let script = {
            let git_dir = git_dir.clone();
            let mut left = BTreeSet::new();
            Script::new(&[(1, 1), (1, 2)], move |wait| match wait {
                1 => {}
                2 => {
                    assert_eq!(ran(&git_dir, "pack-objects"), 1);
                    left = tmp_files(&git_dir);
                    assert!(!left.is_empty(), "the failed roll-up left no file");
                }
                _ => {
                    // A failed batch's end: no run, and the file is gone.
                    assert_eq!(ran(&git_dir, "pack-objects"), 1);
                    let now = tmp_files(&git_dir);
                    assert!(now.is_disjoint(&left), "{left:?} left: {now:?}");
                }
            })
        };
        let slot = Mutex::new(Slot::default());
        run(&script, &git_dir, &slot, Memory::default());
        assert_eq!(script.ends.lock().unwrap().2, 3);
        assert_eq!(read_reachable(&git_dir), reachable);
        fsck(&git_dir, "after the sweep");
    }

    /// A batch that ends just after the task lists the store, and the next
    /// batch's `fast-import` starting its temporary pack: that file is not
    /// swept at the next end, which is its own batch's. The task reads the
    /// counts after it lists, so the end that comes between is counted.
    #[test]
    fn a_live_batchs_temporary_pack_is_not_swept() {
        let tmp = tempfile::tempdir().unwrap();
        let git_dir = stopped_store(tmp.path(), 2);
        let live = git_dir.join("objects/pack/tmp_pack_live");
        let script = {
            let live = live.clone();
            Script::new(&[(0, 1), (0, 3)], move |wait| {
                // At 4, its own batch has ended: it may go.
                if wait == 3 {
                    assert!(live.exists(), "a live batch's file went");
                }
            })
        };
        // Read after the first end's sweep: batch 2 fails, batch 3 starts
        // its pack.
        *script.meanwhile.lock().unwrap() = Some((
            2,
            Box::new(move |ends: &mut Ends| {
                ends.ended += 1;
                std::fs::write(&live, b"PACK").unwrap();
            }),
        ));
        let slot = Mutex::new(Slot::default());
        run(&script, &git_dir, &slot, Memory::default());
        assert_eq!(script.ends.lock().unwrap().2, 4);
    }

    /// Too little free space for a roll-up: nothing is written, the error
    /// is said, and the index stays trusted.
    #[test]
    fn a_roll_up_without_room_writes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let git_dir = stopped_store(tmp.path(), 8);
        let before = entries(&git_dir);
        std::fs::write(git_dir.join(FREE), b"1000").unwrap();
        let git = Path::new("git");
        let slot = Mutex::new(Slot::default());
        let mut memory = Memory::default();
        let error = run_once(git, &git_dir, &slot, &mut memory).unwrap_err();
        assert!(
            matches!(error, TaskError::NoRoom { free: 1000, .. }),
            "{error}"
        );
        assert_eq!(memory.midx, Midx::Trusted { pending: false });
        assert_eq!(entries(&git_dir), before, "a roll-up without room wrote");
        std::fs::remove_file(git_dir.join(FREE)).unwrap();
        assert_eq!(
            run_once(git, &git_dir, &slot, &mut memory).unwrap(),
            Ran::Done(())
        );
        assert_eq!(memory.midx, Midx::Trusted { pending: true });
    }

    /// A temporary file goes only if it is still the file listed: a newer
    /// one, or the same name on a new file, stays.
    #[test]
    fn strays_go_only_as_listed() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        std::fs::write(dir.join("tmp_pack_old"), b"x").unwrap();
        std::fs::write(dir.join("tmp_pack_same"), b"x").unwrap();
        std::fs::write(dir.join("pack-a.pack"), b"x").unwrap();
        let strays = observe(dir).unwrap();
        assert_eq!(strays.0.len(), 2, "{strays:?}");
        std::fs::write(dir.join("tmp_pack_new"), b"x").unwrap();
        // The same name on another file, made while the first exists.
        std::fs::write(dir.join("tmp_other"), b"y").unwrap();
        std::fs::rename(dir.join("tmp_other"), dir.join("tmp_pack_same")).unwrap();
        sweep(dir, &strays).unwrap();
        let left: BTreeSet<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        let want: BTreeSet<String> = ["pack-a.pack", "tmp_pack_new", "tmp_pack_same"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        assert_eq!(left, want);
        // A file gone meanwhile is no error.
        sweep(dir, &observe(dir).unwrap()).unwrap();
        sweep(dir, &strays).unwrap();
    }

    /// A roll-up needs twice its size free.
    #[test]
    fn a_roll_up_needs_twice_its_size_free() {
        assert!(has_room(0, 0));
        assert!(has_room(10, 20));
        assert!(!has_room(10, 19));
        assert!(!has_room(u64::MAX, u64::MAX - 1));
        assert!(has_room(u64::MAX / 2, u64::MAX));
    }

    /// An expiry whose index write fails (a full disk: git deletes the packs
    /// first) leaves the index in doubt: it goes at once, the store reads
    /// whole and passes `git fsck` and `git multi-pack-index verify`, and
    /// later runs, a roll-up among them, write a whole index again. Kept,
    /// that index names packs that are gone, and git 2.47 then writes an
    /// index pointing into the wrong pack, which `fast-import` and `git
    /// rev-list` die on.
    #[test]
    fn a_failed_expiry_drops_the_index() {
        let tmp = tempfile::tempdir().unwrap();
        let git_dir = stopped_store(tmp.path(), 8);
        let objects = all_objects(&git_dir);
        let before = listed(&git_dir);
        let git = Path::new("git");
        let slot = Mutex::new(Slot::default());
        let mut memory = Memory::default();
        assert_eq!(
            run_once(git, &git_dir, &slot, &mut memory).unwrap(),
            Ran::Done(())
        );
        assert_eq!(memory.midx, Midx::Trusted { pending: true });
        assert!(
            listed(&git_dir).is_superset(&before),
            "a roll-up deletes nothing"
        );

        let full = tmp.path().join("full-git");
        script(&full, EXPIRY_FULL);
        run_once(&full, &git_dir, &slot, &mut memory).unwrap_err();
        assert!(
            !listed(&git_dir).is_superset(&before),
            "the expiry deleted packs before it failed"
        );
        assert_eq!(memory.midx, Midx::InDoubt);
        assert!(!index_path(&git_dir).exists(), "the index in doubt is kept");
        git_reads_everything(&git_dir, "after the failed expiry");
        fsck(&git_dir, "after the failed expiry");

        // More batches, then runs: a rewrite, an expiry, roll-ups.
        script(&git_dir.join(WRAPPER), SKIP);
        let mut v = reopen(tmp.path());
        batches(&mut v, 8, 6);
        let reachable = read_reachable(&git_dir);
        for _ in 0..4 {
            assert_eq!(
                run_once(git, &git_dir, &slot, &mut memory).unwrap(),
                Ran::Done(())
            );
            git_reads_everything(&git_dir, "after a run");
        }
        assert!(index_path(&git_dir).exists());
        drop(v);
        midx_verify(&git_dir);
        fsck(&git_dir, "after the runs");
        assert_eq!(read_reachable(&git_dir), reachable);
        assert!(all_objects(&git_dir).is_superset(&objects));
    }

    /// An index write that fails (here a stale lock, as a write the OOM
    /// killer ended leaves) drops the index and its lock, so the next run
    /// writes it, preferring the roll-up written before the failure, and the
    /// run after expires the roll-up's inputs, rather than roll up again at
    /// every event with nothing ever expired. The inputs are made newer than
    /// the roll-up, which git would otherwise pick by its own order.
    #[test]
    fn a_failed_index_write_drops_the_index_and_its_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let git_dir = stopped_store(tmp.path(), 16);
        let objects = all_objects(&git_dir);
        let reachable = read_reachable(&git_dir);
        let before = picked(&git_dir);
        assert!(before.len() >= 2);
        let lock = index_path(&git_dir).with_extension("lock");
        std::fs::write(&lock, b"").unwrap();
        let git = Path::new("git");
        let slot = Mutex::new(Slot::default());
        let mut memory = Memory::default();
        run_once(git, &git_dir, &slot, &mut memory).unwrap_err();
        assert_eq!(memory.midx, Midx::InDoubt);
        assert!(!lock.exists(), "the stale lock is kept");
        let roll_up = memory.rolled.clone().expect("the roll-up's pack is known");
        assert!(listed(&git_dir).contains(&roll_up));
        let later = std::time::SystemTime::now() + Duration::from_secs(60);
        // Git writes packs read-only; the owner may set times through a
        // read-only descriptor (futimens), so no write access is needed.
        for pack in &before {
            File::open(git_dir.join(format!("objects/pack/{pack}.pack")))
                .unwrap()
                .set_modified(later)
                .unwrap();
        }
        // The rewrite, then the expiry (and maybe a roll-up).
        for _ in 0..2 {
            assert_eq!(
                run_once(git, &git_dir, &slot, &mut memory).unwrap(),
                Ran::Done(())
            );
        }
        let left = listed(&git_dir);
        assert!(left.contains(&roll_up), "the roll-up went: {left:?}");
        assert!(
            left.is_disjoint(&before),
            "the roll-up's inputs are left: {left:?}"
        );
        assert_eq!(
            run_once(git, &git_dir, &slot, &mut memory).unwrap(),
            Ran::Done(())
        );
        midx_verify(&git_dir);
        fsck(&git_dir, "after the runs");
        assert_eq!(read_reachable(&git_dir), reachable);
        assert!(all_objects(&git_dir).is_superset(&objects));
    }

    /// `git multi-pack-index verify` passes: every object the index maps is
    /// in the pack it names, at the offset it says.
    fn midx_verify(git_dir: &Path) {
        let out = std::process::Command::new("git")
            .env("GIT_DIR", git_dir)
            .args(["multi-pack-index", "verify", "--no-progress"])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git multi-pack-index verify: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}
