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
//!
//! A front of a history service (design §14) keeps its queue in its slot's
//! file, which the next front leasing the slot loads. Each entry keeps the
//! pod that failed to record it (`@origin` lines), so a repair a front
//! inherits from a dead predecessor is committed as that pod's
//! ([`Recorder::record_as`]), not as the front's own.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use agentfs_core::fsync_class::GroupMembers;
use agentfs_core::nested;
use agentfs_core::paths::PathError;
use agentfs_core::record::{
    EntryPoint, Origin, SavedRepairs, Trigger, Writer, read_repairs, repairs_tmp,
};
use agentfs_core::volume::{Input, MoveOutcome, Recorded, Recorder, VolumeError};

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
/// writer that changed them and a generation, so a repair made from an
/// older entry never drops a newer one.
#[derive(Default)]
struct Repairs {
    queue: BTreeMap<Vec<u8>, Entry>,
    /// This front's own pod, written for its own entries so the next
    /// holder of the slot commits them as this pod's.
    own: Option<Origin>,
    next: u64,
    /// Where the queue is kept across mounts, if anywhere.
    store: Option<PathBuf>,
    /// The last save failed, with this message.
    save_error: Option<String>,
}

/// One queued path.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    writer: Writer,
    generation: u64,
    /// The pod that failed to record it, if not this process's own: a
    /// predecessor in the slot.
    origin: Option<Origin>,
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
    pub writer: Writer,
    /// The pod it is committed as, if not this process's own.
    pub origin: Option<Origin>,
    generation: u64,
}

impl Boundary {
    /// The recorder for the backing tree at `tree`, with an empty repair
    /// queue kept only in memory.
    pub fn new(recorder: Recorder, tree: PathBuf) -> Self {
        let own = recorder.front_origin().cloned();
        Self {
            recorder,
            tree,
            repairs: Mutex::new(Repairs {
                own,
                ..Repairs::default()
            }),
            git_dirs: Mutex::new(GitDirs::default()),
        }
    }

    /// Keeps the repair queue in the file `store` from now on, first
    /// loading what a previous mount left there (design §3.5). Returns how
    /// many paths were loaded.
    pub fn persist_repairs(&self, store: PathBuf) -> io::Result<usize> {
        // A crash between writing the new queue and renaming it over the
        // store leaves the new queue in the temporary file: its complete
        // lines are loaded too (`read_repairs`).
        let SavedRepairs { entries, had_tmp } = read_repairs(&store)?;
        let tmp = repairs_tmp(&store);
        let mut repairs = self.lock();
        repairs.store = Some(store);
        let n = entries.len();
        for (path, writer, origin) in entries {
            let generation = repairs.next;
            repairs.next += 1;
            // This pod's own entries (a restart in the same pod) are its own.
            let origin = origin.filter(|o| Some(o) != repairs.own.as_ref());
            repairs.queue.entry(path).or_insert(Entry {
                writer,
                generation,
                origin,
            });
        }
        if had_tmp {
            // Into the store (renaming the temporary file over it), or
            // gone when nothing is queued.
            repairs.save();
            if repairs.queue.is_empty() {
                match fs::remove_file(&tmp) {
                    Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
                    _ => {}
                }
            }
        }
        Ok(n)
    }

    pub fn recorder(&self) -> &Recorder {
        &self.recorder
    }

    /// Records `changes` as one record. Returns what was journaled (its
    /// `seq` is `None` if nothing was); an error means nothing was (and
    /// `RecordFailed` was raised).
    pub fn record(
        &self,
        trigger: Trigger,
        writer: Writer,
        changes: Vec<Change>,
    ) -> Result<Recorded, VolumeError> {
        let inputs = self.inputs(changes);
        if inputs.is_empty() {
            return Ok(Recorded::default());
        }
        self.recorder.record(trigger, writer, inputs)
    }

    /// Records `changes` like [`Boundary::record`], as the pod `origin`
    /// when given ([`Recorder::record_as`]): an inherited repair.
    pub fn record_as(
        &self,
        trigger: Trigger,
        writer: Writer,
        changes: Vec<Change>,
        origin: Option<&Origin>,
    ) -> Result<bool, VolumeError> {
        let Some(origin) = origin else {
            return Ok(self.record(trigger, writer, changes)?.seq.is_some());
        };
        let inputs = self.inputs(changes);
        if inputs.is_empty() {
            return Ok(false);
        }
        Ok(self
            .recorder
            .record_as(trigger, writer, inputs, Some(origin.clone()))?
            .seq
            .is_some())
    }

    /// Records the rename of directory `from` to `to` as one move
    /// ([`Recorder::record_move`], design §3.1). Repairs queued beneath
    /// `from` are queued again beneath `to`, where the files now are.
    /// [`MoveOutcome::Unsupported`] when either path is not one to record
    /// (inside a `.git`, or not a valid name) or the history service
    /// cannot take moves: the caller records the tree instead.
    pub fn record_move(
        &self,
        trigger: Trigger,
        writer: Writer,
        from: &[u8],
        to: &[u8],
    ) -> Result<MoveOutcome, VolumeError> {
        let (Class::Record(from_path), Class::Record(to_path)) = (classify(from), classify(to))
        else {
            return Ok(MoveOutcome::Unsupported);
        };
        let outcome = self
            .recorder
            .record_move(trigger, writer, from_path, to_path)?;
        if outcome != MoveOutcome::Unsupported {
            self.reroot_repairs(from, to);
        }
        Ok(outcome)
    }

    /// Queues again beneath `to` every repair queued at or beneath `from`.
    /// The entries at `from` stay: their repair records the removal.
    fn reroot_repairs(&self, from: &[u8], to: &[u8]) {
        let mut repairs = self.lock();
        let moved: Vec<(Vec<u8>, Entry)> = repairs
            .queue
            .iter()
            .filter_map(|(path, entry)| {
                let rest = path.strip_prefix(from)?;
                (rest.is_empty() || rest[0] == b'/').then(|| {
                    let mut new = to.to_vec();
                    new.extend_from_slice(rest);
                    (new, entry.clone())
                })
            })
            .collect();
        if moved.is_empty() {
            return;
        }
        for (path, mut entry) in moved {
            entry.generation = repairs.next;
            repairs.next += 1;
            repairs.queue.insert(path, entry);
        }
        repairs.save();
    }

    /// Records an fsync-class group's members as one all-or-nothing record
    /// ([`Recorder::record_group`], design §8). Returns whether anything was
    /// journaled; a refusal is not an error.
    pub fn record_group(
        &self,
        trigger: Trigger,
        writer: Writer,
        changes: Vec<Change>,
    ) -> Result<bool, VolumeError> {
        let inputs = self.inputs(changes);
        if inputs.is_empty() {
            return Ok(false);
        }
        Ok(self
            .recorder
            .record_group(trigger, writer, inputs)?
            .seq
            .is_some())
    }

    /// The fsync-class group the path `raw` belongs to, if any.
    pub fn group_of(&self, raw: &[u8]) -> Option<GroupMembers> {
        if !self.recorder.has_fsync_class() {
            return None;
        }
        match classify(raw) {
            Class::Record(path) => self.recorder.fsync_group(&path),
            _ => None,
        }
    }

    /// Classifies `changes` into recorder inputs, reporting what cannot be
    /// recorded.
    fn inputs(&self, changes: Vec<Change>) -> Vec<Input> {
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
        inputs
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
    pub fn queue_repair(&self, paths: impl IntoIterator<Item = Vec<u8>>, writer: Writer) {
        let mut repairs = self.lock();
        for path in paths {
            let generation = repairs.next;
            repairs.next += 1;
            repairs.queue.insert(
                path,
                Entry {
                    writer: writer.clone(),
                    generation,
                    origin: None,
                },
            );
        }
        repairs.save();
    }

    /// Everything queued, oldest path first. The entries stay queued until
    /// [`Boundary::repaired`] removes them.
    pub fn repairs(&self) -> Vec<Repair> {
        self.lock()
            .queue
            .iter()
            .map(|(path, e)| Repair {
                path: path.clone(),
                writer: e.writer.clone(),
                origin: e.origin.clone(),
                generation: e.generation,
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
                .is_some_and(|e| e.generation == r.generation)
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
            write_atomic(store, &format_repairs(&self.queue, self.own.as_ref()))
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
    let tmp = repairs_tmp(path);
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(content)?;
        f.sync_all()?;
    }
    agentfs_core::failpoints::hit("repairs.before_rename")?;
    fs::rename(&tmp, path)?;
    if let Some(dir) = path.parent() {
        fs::File::open(dir)?.sync_all()?;
    }
    Ok(())
}

/// One line per path, `uid gid pid <path in hex>`, after an
/// `@origin <JSON>` line naming the pod its entries are committed as
/// (`@origin -`: the loading process's own) and an `@entry <entry point>`
/// line naming the entry point they came through, each written before the
/// first entry whose value differs from the line before. Each entry of
/// this process is written with `own`, so the next holder of the slot
/// commits it as this pod's. Older builds skip the header lines (they are
/// not four fields).
fn format_repairs(queue: &BTreeMap<Vec<u8>, Entry>, own: Option<&Origin>) -> Vec<u8> {
    let mut out = String::new();
    let mut current: Option<&Origin> = None;
    let mut entry: Option<&EntryPoint> = None;
    for (path, e) in queue {
        let origin = e.origin.as_ref().or(own);
        if origin != current {
            match origin {
                Some(o) => {
                    let json = serde_json::to_string(o).expect("an origin serializes");
                    out.push_str(&format!("@origin {json}\n"));
                }
                None => out.push_str("@origin -\n"),
            }
            current = origin;
        }
        if entry != Some(&e.writer.entry) {
            out.push_str(&format!("@entry {}\n", e.writer.entry));
            entry = Some(&e.writer.entry);
        }
        let c = e.writer.caller;
        let hex: String = path.iter().map(|b| format!("{b:02x}")).collect();
        out.push_str(&format!("{} {} {} {hex}\n", c.uid, c.gid, c.pid));
    }
    out.into_bytes()
}

/// The trigger a recorded change gets when the sweep repairs it.
pub const REPAIR_TRIGGER: Trigger = Trigger::Sweep;

#[cfg(test)]
mod tests {
    use super::*;
    use agentfs_core::record::{Caller, SandboxId, parse_repairs};

    fn main(caller: Caller) -> Writer {
        Writer {
            caller,
            entry: EntryPoint::Main,
        }
    }

    fn entry(writer: Writer, generation: u64, origin: Option<Origin>) -> Entry {
        Entry {
            writer,
            generation,
            origin,
        }
    }

    fn pod(name: &str) -> Origin {
        Origin {
            namespace: "ns".into(),
            pod: name.into(),
            pod_uid: format!("{name}-uid"),
            node: "n".into(),
            replica: name.into(),
        }
    }

    #[test]
    fn saved_queues_round_trip() {
        let c = Caller {
            uid: 1000,
            gid: 100,
            pid: 7,
        };
        let sandbox = Writer {
            caller: c,
            entry: EntryPoint::Sandbox(SandboxId::new("s1").unwrap()),
        };
        let mut q = BTreeMap::new();
        q.insert(b"big.log".to_vec(), entry(main(c), 0, None));
        q.insert(b"c".to_vec(), entry(main(c), 1, None));
        q.insert(b"d".to_vec(), entry(sandbox.clone(), 2, None));
        q.insert(
            b"dir/with space/\xff".to_vec(),
            entry(Writer::agentfs(NOBODY_CALLER), 3, None),
        );
        let text = format_repairs(&q, None);
        let lines = String::from_utf8_lossy(&text).into_owned();
        assert!(!lines.contains("@origin {"));
        // One `@entry` line where the entry point changes.
        assert_eq!(
            lines.matches("@entry ").collect::<Vec<_>>().len(),
            3,
            "{lines}"
        );
        assert!(lines.contains("@entry sandbox/s1\n"), "{lines}");
        let parsed = parse_repairs(&text);
        assert_eq!(
            parsed,
            vec![
                (b"big.log".to_vec(), main(c), None),
                (b"c".to_vec(), main(c), None),
                (b"d".to_vec(), sandbox, None),
                (
                    b"dir/with space/\xff".to_vec(),
                    Writer::agentfs(NOBODY_CALLER),
                    None
                ),
            ]
        );
        assert!(parse_repairs(b"garbage\n1 2 3 zz\n1 2 3 abc\n").is_empty());
    }

    #[test]
    fn saved_queues_keep_each_entrys_pod() {
        let c = Caller {
            uid: 1000,
            gid: 100,
            pid: 7,
        };
        let mut q = BTreeMap::new();
        // Inherited from pod-a; queued by this front (pod-b) itself.
        q.insert(b"a1".to_vec(), entry(main(c), 0, Some(pod("pod-a"))));
        q.insert(b"a2".to_vec(), entry(main(c), 1, Some(pod("pod-a"))));
        q.insert(b"b1".to_vec(), entry(main(c), 2, None));
        let text = format_repairs(&q, Some(&pod("pod-b")));
        let lines = String::from_utf8(text.clone()).unwrap();
        assert_eq!(lines.matches("@origin ").count(), 2, "{lines}");
        assert_eq!(
            parse_repairs(&text),
            vec![
                (b"a1".to_vec(), main(c), Some(pod("pod-a"))),
                (b"a2".to_vec(), main(c), Some(pod("pod-a"))),
                (b"b1".to_vec(), main(c), Some(pod("pod-b"))),
            ]
        );
        // Single-process mode (no own pod) after an inherited entry.
        let text = format_repairs(&q, None);
        let parsed = parse_repairs(&text);
        assert_eq!(parsed[2], (b"b1".to_vec(), main(c), None));
        assert!(String::from_utf8(text).unwrap().contains("@origin -\n"));
        // A malformed origin line counts as none; older files have none.
        // A line with no `@entry` before it is agentfs's: its writer is
        // unknown, never `main`.
        assert_eq!(
            parse_repairs(b"@origin {nope\n1 2 3 61\n"),
            vec![(
                b"a".to_vec(),
                Writer::agentfs(Caller {
                    uid: 1,
                    gid: 2,
                    pid: 3
                }),
                None
            )]
        );
    }

    #[test]
    fn an_older_build_reads_entries_and_skips_origin_lines() {
        // What an older build's parser accepted: exactly four fields.
        let mut q = BTreeMap::new();
        let c = Caller {
            uid: 1,
            gid: 2,
            pid: 3,
        };
        q.insert(b"a".to_vec(), entry(main(c), 0, Some(pod("pod-a"))));
        let text = String::from_utf8(format_repairs(&q, None)).unwrap();
        let four_fields: Vec<&str> = text
            .lines()
            .filter(|l| {
                l.split(' ').count() == 4 && l.split(' ').next().unwrap().parse::<u32>().is_ok()
            })
            .collect();
        assert_eq!(four_fields, vec!["1 2 3 61"]);
    }

    const NOBODY_CALLER: Caller = Caller {
        uid: 0,
        gid: 0,
        pid: 0,
    };
}
