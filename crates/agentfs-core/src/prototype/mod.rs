//! First real-filesystem slice of target git-model §3, §6, §7 and §18.
//!
//! One exclusively locked actor owns a repository, private checkouts and a
//! byte-call WAL. This is NOT the production recorder or a landing referee.
//! It supports flat regular files and full-lineage saves on main and forks.
//! Every call is fsynced before acknowledgement (deliberately stronger and
//! slower than the target's group commit). Checkouts are disposable caches;
//! the branches are authoritative. See docs/target-architecture/prototype.md.

mod wal;

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};

use git2::{Oid, Repository, Signature};
use serde::{Deserialize, Serialize};

use crate::file_lock::{FileLock, Mode};
use crate::paths::RepoPath;

/// A bounded prototype, not a production volume's size policy.
pub const MAX_FILE_SIZE: usize = 16 << 20;
const FORMAT: &str = "agentfs-prototype-1\n";
type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Git(#[from] git2::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("prototype volume is busy")]
    Busy,
    #[error("not a prototype state area (or unsupported format)")]
    Format,
    #[error("prototype supports flat, git-safe UTF-8 names only")]
    Name,
    #[error("writer must be a nonempty single-line label")]
    Writer,
    #[error("no such tree, file or handle")]
    NotFound,
    #[error("file already exists")]
    Exists,
    #[error("prototype file limit is 16 MiB")]
    TooLarge,
    #[error("journal corruption at byte {0}; operator recovery required")]
    CorruptJournal(usize),
    #[error("actor stopped after a recording/save failure; reopen to recover")]
    Stopped,
}

/// Calls name byte positions, never lines. Append finds EOF at save time.
#[derive(Clone, Debug, Serialize, Deserialize)]
enum Call {
    Write { offset: u64, bytes: Vec<u8> },
    Append(Vec<u8>),
    Truncate(u64),
}

impl Call {
    fn apply(&self, bytes: &mut Vec<u8>) -> Result<()> {
        match self {
            Self::Write {
                offset,
                bytes: data,
            } => {
                let offset = usize::try_from(*offset).map_err(|_| Error::TooLarge)?;
                let end = offset.checked_add(data.len()).ok_or(Error::TooLarge)?;
                if end > MAX_FILE_SIZE {
                    return Err(Error::TooLarge);
                }
                // A zero-length pwrite past EOF does not extend a file.
                if !data.is_empty() {
                    bytes.resize(bytes.len().max(end), 0);
                    bytes[offset..end].copy_from_slice(data);
                }
            }
            Self::Append(data) => {
                if bytes.len().saturating_add(data.len()) > MAX_FILE_SIZE {
                    return Err(Error::TooLarge);
                }
                bytes.extend_from_slice(data);
            }
            Self::Truncate(size) => {
                let size = usize::try_from(*size).map_err(|_| Error::TooLarge)?;
                if size > MAX_FILE_SIZE {
                    return Err(Error::TooLarge);
                }
                bytes.resize(size, 0);
            }
        }
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
struct Record {
    seq: u64,
    event: Event,
}

#[derive(Serialize, Deserialize)]
enum Event {
    Fork {
        name: String,
        parent: String,
        at: String,
    },
    Open {
        tree: String,
        path: String,
        writer: String,
        object: u64,
        create: bool,
    },
    Call {
        handle: u64,
        call: Call,
    },
    Close {
        handle: u64,
    },
    Rename {
        tree: String,
        from: String,
        to: String,
        writer: String,
    },
    Unlink {
        tree: String,
        path: String,
        writer: String,
    },
}

#[derive(Clone)]
struct Tree {
    names: BTreeMap<String, u64>,
    bytes: BTreeMap<u64, Vec<u8>>,
    head: Oid,
    committed: u64,
}

struct Handle {
    tree: String,
    writer: String,
    object: u64,
    bytes: Vec<u8>,
    calls: Vec<Call>,
}

/// Result of a release or crash-recovered release.
#[derive(Debug, PartialEq, Eq)]
pub enum Saved {
    Unchanged,
    Commit(Oid),
    /// An old handle after unlink/rename-over must never reach a replacement.
    OrphanDropped,
}

/// A single synchronous actor. Sharing requires a mutex; a second process
/// cannot open this volume. No method exposes its state area to workloads.
pub struct Volume {
    root: PathBuf,
    repo: Repository,
    wal: wal::Wal,
    trees: BTreeMap<String, Tree>,
    handles: BTreeMap<u64, Handle>,
    next: u64,
    stopped: bool,
    _lock: FileLock,
}

impl Volume {
    /// Initialize an empty directory or recover a prototype volume before serving.
    /// Recovery closes every abandoned write-open exactly once.
    pub fn open(root: &Path) -> Result<Self> {
        crate::history::init_libgit2();
        if !root.exists() {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(root)?;
        }
        let root = root.canonicalize()?;
        let lock =
            FileLock::try_take(&root.join("actor.lock"), Mode::Exclusive)?.ok_or(Error::Busy)?;
        let format = root.join("format");
        if !format.exists() {
            if fs::read_dir(&root)?.any(|e| e.map_or(true, |e| e.file_name() != "actor.lock")) {
                return Err(Error::Format);
            }
            fs::write(&format, FORMAT)?;
            File::open(&format)?.sync_all()?;
            File::open(&root)?.sync_all()?;
        }
        if fs::read_to_string(format)? != FORMAT {
            return Err(Error::Format);
        }
        // Never grant another user access to a newly initialized state area.
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
        if root.join("history.git").exists() {
            remove_stale_locks(&root.join("history.git"))?;
        }
        let repo = if root.join("history.git").exists() {
            Repository::open_bare(root.join("history.git"))?
        } else {
            Repository::init_bare(root.join("history.git"))?
        };
        // libgit2's object fsync is opt-in. Ref updates and the WAL are durable too.
        repo.config()?.set_bool("core.fsyncObjectFiles", true)?;
        let head = match repo.refname_to_id("refs/heads/main") {
            Ok(oid) => oid,
            Err(e) if e.code() == git2::ErrorCode::NotFound => {
                let tree = repo.treebuilder(None)?.write()?;
                let sig = Signature::now("agentfs", "agentfs@localhost")?;
                repo.commit(Some("refs/heads/main"), &sig, &sig,
                    "Root\n\nAgentfs-Writer: agentfs\nAgentfs-Cause: Root\nAgentfs-Tree: main\nAgentfs-Event: 0\n",
                    &repo.find_tree(tree)?, &[])?
            }
            Err(e) => return Err(e.into()),
        };
        File::open(repo.path().join("objects"))?.sync_all()?;
        sync_ref_dirs(&repo, "refs/heads/main")?;
        let (wal, records) = wal::Wal::open(&root.join("calls.wal"))?;
        let mut volume = Self {
            root,
            repo,
            wal,
            trees: BTreeMap::new(),
            handles: BTreeMap::new(),
            next: records.len() as u64 + 1,
            stopped: false,
            _lock: lock,
        };
        let committed = volume.watermark(head)?;
        volume.trees.insert(
            "main".into(),
            Tree {
                names: BTreeMap::new(),
                bytes: BTreeMap::new(),
                head,
                committed,
            },
        );
        for record in records {
            volume.apply(record)?;
        }
        let abandoned: Vec<_> = volume.handles.keys().copied().collect();
        for handle in abandoned {
            volume.close(handle)?;
        }
        for name in volume.tree_names() {
            volume.checkout(&name)?;
        }
        Ok(volume)
    }

    pub fn tree_names(&self) -> Vec<String> {
        self.trees.keys().cloned().collect()
    }

    /// A failed actor must not keep answering reads from stale state.
    pub fn is_stopped(&self) -> bool {
        self.stopped
    }

    pub fn head(&self, tree: &str) -> Result<Oid> {
        Ok(self.tree(tree)?.head)
    }

    /// Fork IDs are numeric and never reused. Forking a fork is not exposed yet.
    pub fn fork(&mut self) -> Result<String> {
        let name = format!("fork/{}", self.next);
        let at = self.head("main")?.to_string();
        self.record(Event::Fork {
            name: name.clone(),
            parent: "main".into(),
            at,
        })?;
        Ok(name)
    }

    /// Saved bytes; an independent reader never sees an unfinished save.
    pub fn read(&self, tree: &str, path: &str) -> Result<&[u8]> {
        let tree = self.tree(tree)?;
        let object = tree.names.get(path).ok_or(Error::NotFound)?;
        Ok(tree.bytes.get(object).ok_or(Error::NotFound)?)
    }

    /// Stable object IDs for a view's inode table (not merge identities).
    pub fn files(&self, tree: &str) -> Result<Vec<(String, u64, usize)>> {
        let tree = self.tree(tree)?;
        Ok(tree
            .names
            .iter()
            .map(|(p, o)| (p.clone(), *o, tree.bytes[o].len()))
            .collect())
    }

    /// Open a private write buffer. Creation is itself a recorded namespace call.
    /// The returned number is persisted before it can be used and never repeats.
    pub fn write_open(
        &mut self,
        tree: &str,
        path: &str,
        writer: &str,
        create: bool,
    ) -> Result<u64> {
        name(path)?;
        writer_label(writer)?;
        let object = self.tree(tree)?.names.get(path).copied();
        if object.is_none() && !create {
            return Err(Error::NotFound);
        }
        let handle = self.next;
        self.record(Event::Open {
            tree: tree.into(),
            path: path.into(),
            writer: writer.into(),
            object: object.unwrap_or(handle),
            create: object.is_none(),
        })?;
        Ok(handle)
    }

    pub fn read_open(&self, handle: u64) -> Result<&[u8]> {
        Ok(&self.handles.get(&handle).ok_or(Error::NotFound)?.bytes)
    }

    pub fn write(&mut self, handle: u64, offset: u64, bytes: &[u8], append: bool) -> Result<()> {
        let call = if append {
            Call::Append(bytes.to_vec())
        } else {
            Call::Write {
                offset,
                bytes: bytes.to_vec(),
            }
        };
        self.call(handle, call)
    }

    pub fn truncate(&mut self, handle: u64, size: u64) -> Result<()> {
        self.call(handle, Call::Truncate(size))
    }

    fn call(&mut self, handle: u64, call: Call) -> Result<()> {
        let h = self.handles.get(&handle).ok_or(Error::NotFound)?;
        // Check both the writer's view and replay on the head before journaling.
        let mut bytes = h.bytes.clone();
        call.apply(&mut bytes)?;
        let mut replay = self.tree(&h.tree)?.bytes[&h.object].clone();
        for c in h.calls.iter().chain(std::iter::once(&call)) {
            c.apply(&mut replay)?;
        }
        // Reserve enough space for EVERY close order. Without this, two
        // accepted appends could exceed the limit only at RELEASE/restart.
        let mut max_base = self.tree(&h.tree)?.bytes[&h.object].len() as u64;
        let mut appended = 0u64;
        for c in self
            .handles
            .values()
            .filter(|other| other.tree == h.tree && other.object == h.object)
            .flat_map(|other| other.calls.iter())
            .chain(std::iter::once(&call))
        {
            match c {
                Call::Append(data) => appended = appended.saturating_add(data.len() as u64),
                Call::Write { offset, bytes } if !bytes.is_empty() => {
                    max_base = max_base.max(offset.saturating_add(bytes.len() as u64));
                }
                Call::Truncate(size) => max_base = max_base.max(*size),
                _ => {}
            }
        }
        if max_base.saturating_add(appended) > MAX_FILE_SIZE as u64 {
            return Err(Error::TooLarge);
        }
        self.record(Event::Call { handle, call })?;
        Ok(())
    }

    /// fsync/FLUSH records durability, not publication. Every call is already durable.
    pub fn sync(&self) -> Result<()> {
        if self.stopped {
            return Err(Error::Stopped);
        }
        self.wal.sync()
    }

    /// RELEASE publishes calls against this tree's head, never the stale open base.
    /// On failure the WAL remains intact; stop and reopen before serving again.
    pub fn close(&mut self, handle: u64) -> Result<Saved> {
        if !self.handles.contains_key(&handle) {
            return Err(Error::NotFound);
        }
        self.record(Event::Close { handle })
    }

    pub fn rename(&mut self, tree: &str, from: &str, to: &str, writer: &str) -> Result<()> {
        name(from)?;
        name(to)?;
        writer_label(writer)?;
        if !self.tree(tree)?.names.contains_key(from) {
            return Err(Error::NotFound);
        }
        if from != to {
            self.record(Event::Rename {
                tree: tree.into(),
                from: from.into(),
                to: to.into(),
                writer: writer.into(),
            })?;
        }
        Ok(())
    }

    pub fn unlink(&mut self, tree: &str, path: &str, writer: &str) -> Result<()> {
        name(path)?;
        writer_label(writer)?;
        if !self.tree(tree)?.names.contains_key(path) {
            return Err(Error::NotFound);
        }
        self.record(Event::Unlink {
            tree: tree.into(),
            path: path.into(),
            writer: writer.into(),
        })?;
        Ok(())
    }

    fn tree(&self, tree: &str) -> Result<&Tree> {
        if self.stopped {
            return Err(Error::Stopped);
        }
        self.trees.get(tree).ok_or(Error::NotFound)
    }

    fn record(&mut self, event: Event) -> Result<Saved> {
        if self.stopped {
            return Err(Error::Stopped);
        }
        let record = Record {
            seq: self.next,
            event,
        };
        if let Err(e) = self.wal.append(&record) {
            self.stopped = true;
            return Err(e);
        }
        self.next += 1;
        let result = self.apply(record);
        if result.is_err() {
            self.stopped = true;
        }
        result
    }

    fn apply(&mut self, record: Record) -> Result<Saved> {
        let seq = record.seq;
        let mut save = None;
        let mut open_trailer = None;
        let mut outcome = Saved::Unchanged;
        match record.event {
            Event::Fork { name, parent, at } => {
                let mut tree = self.tree(&parent)?.clone();
                let branch = format!("refs/heads/{name}");
                tree.head = match self.repo.refname_to_id(&branch) {
                    Ok(oid) => oid,
                    Err(e) if e.code() == git2::ErrorCode::NotFound => {
                        let oid = Oid::from_str(&at)?;
                        self.repo.reference(&branch, oid, false, "prototype fork")?;
                        sync_ref_dirs(&self.repo, &branch)?;
                        oid
                    }
                    Err(e) => return Err(e.into()),
                };
                tree.committed = self.watermark(tree.head)?;
                self.repo
                    .config()?
                    .set_str(&format!("branch.{name}.remote"), ".")?;
                self.repo.config()?.set_str(
                    &format!("branch.{name}.merge"),
                    &format!("refs/heads/{parent}"),
                )?;
                self.trees.insert(name.clone(), tree);
                self.checkout(&name)?;
            }
            Event::Open {
                tree,
                path,
                writer,
                object,
                create,
            } => {
                if create {
                    let t = self.trees.get_mut(&tree).ok_or(Error::NotFound)?;
                    t.names.insert(path, object);
                    t.bytes.insert(object, Vec::new());
                    save = Some((tree.clone(), writer.clone(), "Create"));
                }
                let bytes = self.tree(&tree)?.bytes[&object].clone();
                self.handles.insert(
                    seq,
                    Handle {
                        tree,
                        writer,
                        object,
                        bytes,
                        calls: Vec::new(),
                    },
                );
            }
            Event::Call { handle, call } => {
                let h = self.handles.get_mut(&handle).ok_or(Error::NotFound)?;
                call.apply(&mut h.bytes)?;
                h.calls.push(call);
            }
            Event::Close { handle } => {
                let h = self.handles.remove(&handle).ok_or(Error::NotFound)?;
                let tree = self.trees.get_mut(&h.tree).ok_or(Error::NotFound)?;
                if !tree.names.values().any(|o| *o == h.object) {
                    outcome = Saved::OrphanDropped;
                } else if !h.calls.is_empty() {
                    let mut bytes = tree.bytes[&h.object].clone();
                    for call in h.calls {
                        call.apply(&mut bytes)?;
                    }
                    tree.bytes.insert(h.object, bytes);
                    open_trailer = Some(handle);
                    save = Some((h.tree, h.writer, "Direct"));
                }
            }
            Event::Rename {
                tree,
                from,
                to,
                writer,
            } => {
                let t = self.trees.get_mut(&tree).ok_or(Error::NotFound)?;
                let object = t.names.remove(&from).ok_or(Error::NotFound)?;
                t.names.insert(to, object);
                save = Some((tree, writer, "Rename"));
            }
            Event::Unlink { tree, path, writer } => {
                self.trees
                    .get_mut(&tree)
                    .ok_or(Error::NotFound)?
                    .names
                    .remove(&path)
                    .ok_or(Error::NotFound)?;
                save = Some((tree, writer, "Unlink"));
            }
        }
        if let Some((tree, writer, cause)) = save
            && seq > self.tree(&tree)?.committed
        {
            outcome = Saved::Commit(self.commit(&tree, &writer, cause, seq, open_trailer)?);
        }
        Ok(outcome)
    }

    fn watermark(&self, head: Oid) -> Result<u64> {
        let commit = self.repo.find_commit(head)?;
        commit
            .message()
            .ok()
            .and_then(|m| m.lines().find_map(|l| l.strip_prefix("Agentfs-Event: ")))
            .and_then(|n| n.parse().ok())
            .ok_or(Error::Format)
    }

    fn commit(
        &mut self,
        name: &str,
        writer: &str,
        cause: &str,
        seq: u64,
        open: Option<u64>,
    ) -> Result<Oid> {
        let t = self.tree(name)?;
        let mut builder = self.repo.treebuilder(None)?;
        for (path, object) in &t.names {
            let blob = self.repo.blob(&t.bytes[object])?;
            builder.insert(path, blob, 0o100644)?;
        }
        let tree = self.repo.find_tree(builder.write()?)?;
        let parent = self.repo.find_commit(t.head)?;
        let sig = Signature::now("agentfs", "agentfs@localhost")?;
        let mut message = format!(
            "{cause} by {writer}\n\nAgentfs-Writer: {writer}\nAgentfs-Cause: Direct\nAgentfs-Tree: {name}\nAgentfs-Event: {seq}\n"
        );
        if let Some(open) = open {
            message.push_str(&format!("Agentfs-Open: {open}\n"));
        }
        let head = self
            .repo
            .commit(None, &sig, &sig, &message, &tree, &[&parent])?;
        File::open(self.repo.path().join("objects"))?.sync_all()?;
        let branch = format!("refs/heads/{name}");
        self.repo
            .reference_matching(&branch, head, true, t.head, "prototype save")?;
        sync_ref_dirs(&self.repo, &branch)?;
        let t = self.trees.get_mut(name).ok_or(Error::NotFound)?;
        t.head = head;
        t.committed = seq;
        self.checkout(name)?;
        Ok(head)
    }

    fn checkout(&self, name: &str) -> Result<()> {
        let dir = self.root.join("trees").join(name);
        fs::create_dir_all(&dir)?;
        // Rebuild from the BRANCH, never replay's intermediate in-memory state.
        let tree = self.repo.find_commit(self.tree(name)?.head)?.tree()?;
        let mut checkout = git2::build::CheckoutBuilder::new();
        checkout
            .target_dir(&dir)
            .force()
            .remove_untracked(true)
            .update_index(false)
            .disable_filters(true);
        self.repo
            .checkout_tree(tree.as_object(), Some(&mut checkout))?;
        Ok(())
    }
}

fn sync_ref_dirs(repo: &Repository, reference: &str) -> io::Result<()> {
    let mut path = repo.path().join(reference);
    while path.pop() && path.starts_with(repo.path()) {
        File::open(&path)?.sync_all()?;
    }
    Ok(())
}

// The exclusive actor lock is held first; no other writer can own these.
fn remove_stale_locks(gitdir: &Path) -> io::Result<()> {
    fn refs(dir: &Path) -> io::Result<()> {
        if !dir.exists() {
            return Ok(());
        }
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                refs(&entry.path())?;
            } else if entry.file_type()?.is_file()
                && entry.path().extension().is_some_and(|s| s == "lock")
            {
                fs::remove_file(entry.path())?;
            }
        }
        Ok(())
    }
    refs(&gitdir.join("refs"))?;
    for name in ["HEAD.lock", "config.lock", "packed-refs.lock", "index.lock"] {
        let path = gitdir.join(name);
        if path.is_file() {
            fs::remove_file(path)?;
        }
    }
    Ok(())
}

fn name(path: &str) -> Result<()> {
    RepoPath::new(path).map_err(|_| Error::Name)?;
    if path.contains('/') {
        return Err(Error::Name);
    }
    Ok(())
}

fn writer_label(writer: &str) -> Result<()> {
    if writer.is_empty() || writer.len() > 256 || writer.chars().any(char::is_control) {
        return Err(Error::Writer);
    }
    Ok(())
}
