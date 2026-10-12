//! Recording a change boundary (design §3.1, §3.2, §3.5).
//!
//! The FUSE operations describe what changed as [`Change`]s on raw paths.
//! [`Boundary::record`] classifies each path ([`crate::names`]): paths under
//! a `.git` that is git's own data (a git directory, being created or
//! complete, or a gitfile; [`agentfs_core::nested::is_git_metadata`]) are
//! dropped silently; paths under any other `.git` raise `PathRefused` once
//! per such `.git`; invalid names raise `PathRefused`; and the rest go to
//! [`Recorder::record`] as one record, synchronously, before the operation
//! replies.
//!
//! When recording fails (design §3.5) the core has already raised
//! `RecordFailed`. FLUSH returns `EIO` and leaves the inode dirty, so the
//! next boundary records it again. A namespace operation (rename, unlink,
//! link, ...) has already changed the backing tree by then, so its paths go
//! on the *repair queue*: the sweep re-derives each queued path's absolute
//! state from the backing tree and records it. Records hold absolute state,
//! so a repair is idempotent and cannot undo a later change.
//!
//! The queue outlives the process: [`Boundary::persist_repairs`] keeps it
//! in `meta/repairs` in the state area, rewritten whenever it changes, and
//! `agentfs mount` repairs what a previous mount left there before it
//! serves any request. If that file cannot be written either (the state
//! area is full), shutdown lists the paths: history differs from the
//! backing tree there until each is changed again.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use agentfs_core::nested;
use agentfs_core::paths::PathError;
use agentfs_core::record::{Caller, Trigger};
use agentfs_core::volume::{Input, Recorder, VolumeError};

use crate::names::{Class, classify};

/// One path's new state, before classification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    /// Relative to the mount root.
    pub path: Vec<u8>,
    pub state: State,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    /// A regular file whose content is at `backing` (a `/proc/self/fd`
    /// path the caller keeps open until the record returns).
    File {
        backing: PathBuf,
        executable: bool,
    },
    Symlink {
        target: Vec<u8>,
    },
    Removed,
}

impl Change {
    pub fn removed(path: Vec<u8>) -> Self {
        Self {
            path,
            state: State::Removed,
        }
    }
}

/// The recorder as the FUSE layer uses it, with the repair queue.
pub struct Boundary {
    recorder: Recorder,
    /// The backing tree, to tell what a `.git` in a path is.
    tree: PathBuf,
    repairs: Mutex<Repairs>,
    git_dirs: Mutex<GitDirs>,
}

/// Paths whose record failed after the backing tree changed, with the
/// caller that changed them and a generation, so a repair made from an
/// older entry never drops a newer one.
#[derive(Default)]
struct Repairs {
    queue: BTreeMap<Vec<u8>, (Caller, u64)>,
    next: u64,
    /// Where the queue is kept across mounts, if anywhere.
    store: Option<PathBuf>,
    /// The last save failed, with this message.
    save_error: Option<String>,
}

/// What [`Boundary::record`] learned about `.git` directories, by the path
/// of the directory holding them. Bounded: cleared when it grows past
/// [`GIT_DIRS_MAX`].
#[derive(Default)]
struct GitDirs {
    /// `.git` is git's own data: paths under it are dropped silently.
    git: HashSet<Vec<u8>>,
    /// `.git` is something else, already reported once.
    refused: HashSet<Vec<u8>>,
}

const GIT_DIRS_MAX: usize = 4096;

/// One queued repair, as [`Boundary::repairs`] lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repair {
    pub path: Vec<u8>,
    pub caller: Caller,
    generation: u64,
}

impl Boundary {
    /// The recorder for the backing tree at `tree`, with an empty repair
    /// queue kept only in memory.
    pub fn new(recorder: Recorder, tree: PathBuf) -> Self {
        Self {
            recorder,
            tree,
            repairs: Mutex::new(Repairs::default()),
            git_dirs: Mutex::new(GitDirs::default()),
        }
    }

    /// Keeps the repair queue in the file `store` from now on, first
    /// loading what a previous mount left there (design §3.5). Returns how
    /// many paths were loaded.
    pub fn persist_repairs(&self, store: PathBuf) -> io::Result<usize> {
        let loaded = match fs::read(&store) {
            Ok(bytes) => parse_repairs(&bytes),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e),
        };
        let mut repairs = self.lock();
        repairs.store = Some(store);
        let n = loaded.len();
        for (path, caller) in loaded {
            let generation = repairs.next;
            repairs.next += 1;
            repairs.queue.entry(path).or_insert((caller, generation));
        }
        Ok(n)
    }

    pub fn recorder(&self) -> &Recorder {
        &self.recorder
    }

    /// Records `changes` as one record. Returns whether anything was
    /// journaled; an error means nothing was (and `RecordFailed` was
    /// raised).
    pub fn record(
        &self,
        trigger: Trigger,
        caller: Caller,
        changes: Vec<Change>,
    ) -> Result<bool, VolumeError> {
        let mut inputs = Vec::with_capacity(changes.len());
        for change in changes {
            let path = match classify(&change.path) {
                Class::Record(path) => path,
                Class::InGitDir { dir } => {
                    // Removing never-recorded paths needs no word.
                    if change.state != State::Removed {
                        self.check_git_dir(&dir, &change.path);
                    }
                    continue;
                }
                Class::Invalid { raw, error } => {
                    // A removal of a path git cannot store removes nothing
                    // that was ever recorded; only new content is refused.
                    if change.state != State::Removed {
                        self.recorder.refuse_invalid(&raw, error);
                    }
                    continue;
                }
            };
            inputs.push(match change.state {
                State::File {
                    backing,
                    executable,
                } => Input::Write {
                    path,
                    backing,
                    executable,
                },
                State::Symlink { target } => match String::from_utf8(target) {
                    Ok(target) => Input::Symlink { path, target },
                    Err(e) => {
                        let target = String::from_utf8_lossy(e.as_bytes()).into_owned();
                        self.recorder.refuse_invalid(
                            path.as_str(),
                            PathError::NotUtf8(format!("symlink target {target}")),
                        );
                        continue;
                    }
                },
                State::Removed => Input::Remove { path },
            });
        }
        if inputs.is_empty() {
            return Ok(false);
        }
        Ok(self.recorder.record(trigger, caller, inputs)?.seq.is_some())
    }

    /// New content at `raw`, under `dir/.git`: silent if that `.git` is
    /// git's own data, else `PathRefused`, once per `.git` (design §5).
    fn check_git_dir(&self, dir: &[u8], raw: &[u8]) {
        {
            let known = self.git_dirs();
            if known.git.contains(dir) {
                return;
            }
        }
        let is_git = std::str::from_utf8(dir)
            .ok()
            .is_some_and(|d| nested::is_git_metadata(&self.tree, d).unwrap_or(false));
        let mut known = self.git_dirs();
        if known.git.len() + known.refused.len() > GIT_DIRS_MAX {
            known.git.clear();
            known.refused.clear();
        }
        if is_git {
            known.refused.remove(dir);
            known.git.insert(dir.to_vec());
            return;
        }
        if known.refused.insert(dir.to_vec()) {
            drop(known);
            let raw = String::from_utf8_lossy(raw).into_owned();
            self.recorder
                .refuse_invalid(&raw, PathError::DotGit(raw.clone()));
        }
    }

    fn git_dirs(&self) -> std::sync::MutexGuard<'_, GitDirs> {
        self.git_dirs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Queues `paths` for the sweep to re-derive and record (design §3.5),
    /// and saves the queue.
    pub fn queue_repair(&self, paths: impl IntoIterator<Item = Vec<u8>>, caller: Caller) {
        let mut repairs = self.lock();
        for path in paths {
            let generation = repairs.next;
            repairs.next += 1;
            repairs.queue.insert(path, (caller, generation));
        }
        repairs.save();
    }

    /// Everything queued, oldest path first. The entries stay queued until
    /// [`Boundary::repaired`] removes them.
    pub fn repairs(&self) -> Vec<Repair> {
        self.lock()
            .queue
            .iter()
            .map(|(path, (caller, generation))| Repair {
                path: path.clone(),
                caller: *caller,
                generation: *generation,
            })
            .collect()
    }

    /// Removes `done` from the queue, unless a path was queued again since
    /// it was listed, and saves the queue.
    pub fn repaired(&self, done: &[Repair]) {
        let mut repairs = self.lock();
        for r in done {
            if repairs
                .queue
                .get(&r.path)
                .is_some_and(|(_, g)| *g == r.generation)
            {
                repairs.queue.remove(&r.path);
            }
        }
        repairs.save();
    }

    pub fn pending_repairs(&self) -> usize {
        self.lock().queue.len()
    }

    /// The queued paths, for the shutdown report.
    pub fn pending_paths(&self) -> Vec<String> {
        self.lock()
            .queue
            .keys()
            .map(|p| String::from_utf8_lossy(p).into_owned())
            .collect()
    }

    /// Where the queue is saved, and whether the last save worked.
    pub fn repair_store(&self) -> Option<(PathBuf, Option<String>)> {
        let repairs = self.lock();
        repairs
            .store
            .clone()
            .map(|s| (s, repairs.save_error.clone()))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Repairs> {
        self.repairs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Repairs {
    /// Writes the queue to the store (atomically: a temporary file renamed
    /// over it), or removes the store when the queue is empty. A failure is
    /// reported once and remembered for the shutdown report.
    fn save(&mut self) {
        let Some(store) = &self.store else {
            return;
        };
        let result = if self.queue.is_empty() {
            match fs::remove_file(store) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            }
        } else {
            write_atomic(store, &format_repairs(&self.queue))
        };
        match result {
            Ok(()) => self.save_error = None,
            Err(e) => {
                let message = format!("saving the repair queue to {}: {e}", store.display());
                if self.save_error.is_none() {
                    eprintln!("agentfs: {message}");
                }
                self.save_error = Some(message);
            }
        }
    }
}

fn write_atomic(path: &Path, content: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(content)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    if let Some(dir) = path.parent() {
        fs::File::open(dir)?.sync_all()?;
    }
    Ok(())
}

/// One line per path: `uid gid pid <path in hex>`.
fn format_repairs(queue: &BTreeMap<Vec<u8>, (Caller, u64)>) -> Vec<u8> {
    let mut out = String::new();
    for (path, (c, _)) in queue {
        let hex: String = path.iter().map(|b| format!("{b:02x}")).collect();
        out.push_str(&format!("{} {} {} {hex}\n", c.uid, c.gid, c.pid));
    }
    out.into_bytes()
}

/// The entries of a saved queue; malformed lines are skipped.
fn parse_repairs(bytes: &[u8]) -> Vec<(Vec<u8>, Caller)> {
    let text = String::from_utf8_lossy(bytes);
    text.lines()
        .filter_map(|line| {
            let mut f = line.split(' ');
            let uid = f.next()?.parse().ok()?;
            let gid = f.next()?.parse().ok()?;
            let pid = f.next()?.parse().ok()?;
            let hex = f.next()?;
            if f.next().is_some() || hex.len() % 2 != 0 {
                return None;
            }
            let path = (0..hex.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok())
                .collect::<Option<Vec<u8>>>()?;
            Some((path, Caller { uid, gid, pid }))
        })
        .collect()
}

/// The trigger a recorded change gets when the sweep repairs it.
pub const REPAIR_TRIGGER: Trigger = Trigger::Sweep;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saved_queues_round_trip() {
        let c = Caller {
            uid: 1000,
            gid: 100,
            pid: 7,
        };
        let mut q = BTreeMap::new();
        q.insert(b"big.log".to_vec(), (c, 0));
        q.insert(b"dir/with space/\xff".to_vec(), (NOBODY_CALLER, 1));
        let text = format_repairs(&q);
        let parsed = parse_repairs(&text);
        assert_eq!(
            parsed,
            vec![
                (b"big.log".to_vec(), c),
                (b"dir/with space/\xff".to_vec(), NOBODY_CALLER),
            ]
        );
        assert!(parse_repairs(b"garbage\n1 2 3 zz\n1 2 3 abc\n").is_empty());
    }

    const NOBODY_CALLER: Caller = Caller {
        uid: 0,
        gid: 0,
        pid: 0,
    };
}
