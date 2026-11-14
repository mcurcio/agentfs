//! Git plumbing for tree actors (target architecture §§3, 12).
//!
//! The repository is in Git's files format, and its refs are a copy: the
//! actor's journal is the commit point and `rebuild_refs` remakes them after a
//! crash. Objects are durable before a record names them. agentfs writes
//! `config` and `HEAD` itself, atomically, before any git command: git replaces
//! `config` with no fsync, and an emptied one silently loses `core.fsync`.
//!
//! A save's work runs in this process, through libgit2 (docs/architecture.md,
//! "Technology choices"): objects, the ref copy and the checkout. Only reads
//! that walk history, and `init`, run the `git` program.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::io::{self, Write};
use std::os::fd::{AsRawFd, BorrowedFd};
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::file_lock::FileLock;

use git2::{ErrorCode, ObjectType, Oid, Repository};
use thiserror::Error;

/// Errors from the Git executable or its storage.
#[derive(Debug, Error)]
pub enum Error {
    #[error("I/O: {0}")]
    Io(#[from] io::Error),
    #[error("state area full: {0}")]
    NoSpace(String),
    #[error("git failed: {0}")]
    Git(String),
    #[error("invalid git output: {0}")]
    Invalid(String),
}

/// libgit2 says what failed in its message, with the system's text for an
/// I/O error: a full state area must still reach the call as `ENOSPC`.
impl From<git2::Error> for Error {
    fn from(error: git2::Error) -> Self {
        classify(error.message().as_bytes())
    }
}

type Result<T> = std::result::Result<T, Error>;

/// What git's stderr says went wrong. A full state area must reach the call as
/// `ENOSPC`, and git's exit status does not say (children run under `LC_ALL=C`).
fn classify(stderr: &[u8]) -> Error {
    let text = String::from_utf8_lossy(stderr).trim().to_owned();
    if text.contains("No space left on device") || text.contains("Disk quota exceeded") {
        Error::NoSpace(text)
    } else {
        Error::Git(text)
    }
}

static OBJECT_WRITES: AtomicU64 = AtomicU64::new(0);
static OBJECT_DIR_SYNCS: AtomicU64 = AtomicU64::new(0);

/// Objects handed to a tree repository's store in this process, new or not.
/// A measurement, like [`crate::git_cli::children`].
pub(crate) fn object_writes() -> u64 {
    OBJECT_WRITES.load(Ordering::Relaxed)
}

/// fsyncs of a tree repository's `objects/` directory in this process. A
/// measurement.
pub(crate) fn object_dir_syncs() -> u64 {
    OBJECT_DIR_SYNCS.load(Ordering::Relaxed)
}

// The repository's whole configuration; nothing else writes it.
const CONFIG: &str = "[core]\n\trepositoryformatversion = 0\n\tfilemode = true\n\tbare = true\n\tfsync = objects,derived-metadata\n\tfsyncMethod = fsync\n";

/// A single tree entry. Names are raw Unix filename bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeEntry {
    pub name: Vec<u8>,
    pub mode: u32,
    pub oid: Oid,
}

/// The tree, parents, and raw message of a commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Commit {
    pub tree: Oid,
    pub parents: Vec<Oid>,
    pub message: Vec<u8>,
}

/// An atomic compare-and-swap of one reference. None means absent/deleted.
#[derive(Clone, Debug)]
pub struct RefUpdate {
    pub refname: String,
    pub new: Option<Oid>,
    pub expected_old: Option<Oid>,
}

/// A bare Git repository whose refs are a copy of the journal's pointers.
#[derive(Clone, Debug)]
pub struct TreeGit {
    path: PathBuf,
    lease: Option<Arc<FileLock>>,
    store: Arc<Store>,
}

/// The repository as libgit2 opens it, shared by every clone of a [`TreeGit`].
#[derive(Default)]
struct Store {
    /// Opened at first use, after `write_config` has made a cut repository whole.
    repo: Mutex<Option<Repository>>,
    /// An object was written since `objects/` was last fsynced.
    unsynced: AtomicBool,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store")
            .field("unsynced", &self.unsynced.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl TreeGit {
    /// Initialize a bare repository. For an actor, use `init_with_lease`.
    pub fn init(path: impl AsRef<Path>) -> Result<Self> {
        Self::init_inner(path.as_ref(), None)
    }

    /// Initialize while retaining the actor's exclusive lock across Git children.
    pub fn init_with_lease(path: impl AsRef<Path>, lease: Arc<FileLock>) -> Result<Self> {
        Self::init_inner(path.as_ref(), Some(lease))
    }

    fn init_inner(path: &Path, lease: Option<Arc<FileLock>>) -> Result<Self> {
        std::fs::create_dir_all(path)?;
        let repo = Self {
            path: std::fs::canonicalize(path)?,
            lease,
            store: Arc::default(),
        };
        repo.run(&["init", "--bare"], None, false)?;
        sync_initialized_repository(&repo.path)?;
        Ok(repo)
    }

    /// Open a repository. For an actor, use `open_with_lease`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_inner(path.as_ref(), None)
    }

    /// Keeps the path and the actor's lock across Git children. Runs no git and
    /// checks no format: after a crash `write_config` comes first.
    pub fn open_with_lease(path: impl AsRef<Path>, lease: Arc<FileLock>) -> Result<Self> {
        Self::open_inner(path.as_ref(), Some(lease))
    }

    fn open_inner(path: &Path, lease: Option<Arc<FileLock>>) -> Result<Self> {
        Ok(Self {
            path: std::fs::canonicalize(path)?,
            lease,
            store: Arc::default(),
        })
    }

    /// Replaces `config` and `HEAD`, each atomically (temporary file, fsync,
    /// rename, directory fsync). Runs no git, and leaves what git needs to call
    /// this a repository, `refs/` included: a cut rebuild can leave none.
    /// `head` is HEAD's full ref; `merges` maps each branch section's name to
    /// the full ref it merges.
    pub fn write_config(&self, head: &str, merges: &BTreeMap<String, String>) -> Result<()> {
        let mut config = CONFIG.to_owned();
        for (branch, merge) in merges {
            let section = branch.replace('\\', "\\\\").replace('"', "\\\"");
            config.push_str(&format!(
                "[branch \"{section}\"]\n\tremote = .\n\tmerge = {merge}\n"
            ));
        }
        replace_file(&self.path, "config", config.as_bytes())?;
        replace_file(&self.path, "HEAD", format!("ref: {head}\n").as_bytes())?;
        std::fs::create_dir_all(self.path.join("refs"))?;
        Ok(())
    }

    /// Removes `refs/` and `packed-refs`, makes `refs/`, and creates every
    /// pointer in one [`TreeGit::ref_transaction`]. A copy, which needs no
    /// fsync; libgit2's process-wide setting fsyncs each ref all the same.
    pub fn rebuild_refs(&self, pointers: &[RefUpdate]) -> Result<()> {
        let refs = self.path.join("refs");
        match std::fs::remove_dir_all(&refs) {
            Err(err) if err.kind() != io::ErrorKind::NotFound => return Err(err.into()),
            _ => {}
        }
        match std::fs::remove_file(self.path.join("packed-refs")) {
            Err(err) if err.kind() != io::ErrorKind::NotFound => return Err(err.into()),
            _ => {}
        }
        std::fs::create_dir(&refs)?;
        self.ref_transaction(pointers)
    }

    /// What is lost of each tip's commit, tree and blobs, parents not walked.
    pub fn missing_objects(&self, tips: &[Oid]) -> Result<Vec<Oid>> {
        let mut input = String::new();
        for tip in tips {
            input.push_str(&format!("{tip}\n"));
        }
        let out = self.run(
            &[
                "rev-list",
                "--objects",
                "--no-walk",
                "--missing=print",
                "--stdin",
            ],
            Some(input.as_bytes()),
            true,
        )?;
        out.stdout
            .split(|b| *b == b'\n')
            .filter_map(|line| line.strip_prefix(b"?"))
            .map(parse_oid)
            .collect()
    }

    /// Each commit reachable from `tips` with its raw message, in one Git
    /// child, not one per commit.
    pub fn commit_messages(&self, tips: &[Oid]) -> Result<Vec<(Oid, Vec<u8>)>> {
        if tips.is_empty() {
            return Ok(Vec::new());
        }
        let mut input = String::new();
        for tip in tips {
            input.push_str(&format!("{tip}\n"));
        }
        let out = self.run(
            &["log", "--stdin", "--no-color", "--format=%x01%H%n%B"],
            Some(input.as_bytes()),
            true,
        )?;
        let mut commits = Vec::new();
        for record in out.stdout.split(|b| *b == 1).filter(|r| !r.is_empty()) {
            let (oid, message) = match record.iter().position(|b| *b == b'\n') {
                Some(end) => (&record[..end], &record[end + 1..]),
                None => (record, &[][..]),
            };
            commits.push((parse_oid(oid)?, message.to_vec()));
        }
        Ok(commits)
    }

    fn command(&self, bare: bool) -> Command {
        let mut cmd = Command::new("git");
        cmd.arg("-c").arg("core.hooksPath=/dev/null");
        if let Some(lease) = &self.lease {
            attach_lease(&mut cmd, lease);
        }
        cmd.env_clear()
            .env("PATH", crate::git_cli::GIT_PATH)
            .env("LC_ALL", "C")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .current_dir(&self.path);
        if bare {
            cmd.env("GIT_DIR", &self.path);
        }
        cmd
    }

    fn run(&self, args: &[&str], input: Option<&[u8]>, bare: bool) -> Result<Output> {
        let mut cmd = self.command(bare);
        cmd.args(args);
        run_command(cmd, input)
    }

    /// Return a reference's value, or None when it does not exist.
    pub fn head(&self, name: &str) -> Result<Option<Oid>> {
        self.run(&["check-ref-format", name], None, true)?;
        let mut cmd = self.command(true);
        cmd.args(["rev-parse", "--verify", "--quiet", name]);
        let out = output(cmd)?;
        if out.status.success() {
            Ok(Some(parse_oid(trim(&out.stdout))?))
        } else if out.status.code() == Some(1) {
            Ok(None)
        } else {
            Err(classify(&out.stderr))
        }
    }

    /// List references beneath a prefix, in Git's reference order.
    pub fn list_refs(&self, prefix: &str) -> Result<Vec<(String, Oid)>> {
        let out = self.run(
            &[
                "for-each-ref",
                "--format=%(refname)%00%(objectname)",
                prefix,
            ],
            None,
            true,
        )?;
        out.stdout
            .split(|b| *b == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| {
                let index = line
                    .iter()
                    .position(|b| *b == 0)
                    .ok_or_else(|| Error::Invalid("reference record".into()))?;
                let (name, oid) = (&line[..index], &line[index + 1..]);
                Ok((
                    String::from_utf8(name.to_vec())
                        .map_err(|_| Error::Invalid("reference name".into()))?,
                    parse_oid(oid)?,
                ))
            })
            .collect()
    }

    /// libgit2's handle on the repository, opened at first use with the
    /// process's libgit2 setup ([`crate::history`]): no configuration but the
    /// repository's own, and objects and refs fsynced.
    fn with_repo<R>(&self, f: impl FnOnce(&Repository) -> Result<R>) -> Result<R> {
        let mut slot = self
            .store
            .repo
            .lock()
            .map_err(|_| Error::Git("the repository's lock is poisoned".into()))?;
        let repo = match &mut *slot {
            Some(repo) => repo,
            empty => {
                crate::history::init_libgit2();
                empty.insert(Repository::open_bare(&self.path)?)
            }
        };
        f(repo)
    }

    /// Stores one object. libgit2 fsyncs a new object's file and its fan-out
    /// directory and leaves one it already holds as it is; `objects/`, which
    /// names a fan-out directory the write may have made, waits for
    /// [`TreeGit::sync_objects`].
    fn write_object(&self, kind: ObjectType, bytes: &[u8]) -> Result<Oid> {
        OBJECT_WRITES.fetch_add(1, Ordering::Relaxed);
        let oid = self.with_repo(|repo| Ok(repo.odb()?.write(kind, bytes)?))?;
        self.store.unsynced.store(true, Ordering::Release);
        Ok(oid)
    }

    /// Makes every object written so far durable: one fsync of `objects/`
    /// when any was written since the last. A step calls it before its
    /// journal record names them.
    pub fn sync_objects(&self) -> Result<()> {
        if self.store.unsynced.swap(false, Ordering::AcqRel) {
            let synced = std::fs::File::open(self.path.join("objects")).and_then(|d| d.sync_all());
            if let Err(err) = synced {
                self.store.unsynced.store(true, Ordering::Release);
                return Err(err.into());
            }
            OBJECT_DIR_SYNCS.fetch_add(1, Ordering::Relaxed);
        }
        Ok(())
    }

    fn read_object(&self, oid: Oid, kind: ObjectType) -> Result<Vec<u8>> {
        self.with_repo(|repo| {
            let odb = repo.odb()?;
            let object = odb.read(oid)?;
            if object.kind() != kind {
                return Err(Error::Invalid(format!("{oid} is a {}", object.kind())));
            }
            Ok(object.data().to_vec())
        })
    }

    /// Store raw blob bytes.
    pub fn write_blob(&self, bytes: &[u8]) -> Result<Oid> {
        self.write_object(ObjectType::Blob, bytes)
    }
    /// Read raw blob bytes without filters.
    pub fn read_blob(&self, oid: Oid) -> Result<Vec<u8>> {
        self.read_object(oid, ObjectType::Blob)
    }
    /// Read immediate tree entries with raw names, in the tree's order.
    pub fn list_tree(&self, oid: Oid) -> Result<Vec<TreeEntry>> {
        self.with_repo(|repo| {
            Ok(repo
                .find_tree(oid)?
                .iter()
                .map(|entry| TreeEntry {
                    name: entry.name_bytes().to_vec(),
                    mode: u32::try_from(entry.filemode_raw()).unwrap_or(0),
                    oid: entry.id(),
                })
                .collect())
        })
    }
    /// Store immediate entries, checked as `git hash-object -t tree` checks
    /// them: known modes, names that are not empty, `.` or `..` and hold no
    /// `/` or NUL, each name once. The bytes are the ones Git would write.
    pub fn write_tree(&self, entries: &[TreeEntry]) -> Result<Oid> {
        let mut sorted = Vec::with_capacity(entries.len());
        for e in entries {
            if !matches!(e.mode, 0o40000 | 0o100644 | 0o100755 | 0o120000 | 0o160000) {
                return Err(Error::Invalid("tree mode".into()));
            }
            if e.name.is_empty()
                || e.name == b"."
                || e.name == b".."
                || e.name.contains(&0)
                || e.name.contains(&b'/')
            {
                return Err(Error::Invalid("tree entry name".into()));
            }
            // Git orders a directory as if its name ended in a slash.
            let mut key = e.name.clone();
            if e.mode == 0o40000 {
                key.push(b'/');
            }
            sorted.push((key, e));
        }
        sorted.sort_by(|a, b| a.0.cmp(&b.0));
        if sorted.windows(2).any(|w| w[0].1.name == w[1].1.name) {
            return Err(Error::Invalid("duplicate tree entry".into()));
        }
        let mut data = Vec::new();
        for (_, e) in sorted {
            write!(&mut data, "{:o} ", e.mode)?;
            data.extend_from_slice(&e.name);
            data.push(0);
            data.extend_from_slice(e.oid.as_bytes());
        }
        self.write_object(ObjectType::Tree, &data)
    }
    /// Read a commit's tree, parents, and message.
    pub fn read_commit(&self, oid: Oid) -> Result<Commit> {
        let bytes = self.read_object(oid, ObjectType::Commit)?;
        let boundary = bytes
            .windows(2)
            .position(|w| w == b"\n\n")
            .ok_or_else(|| Error::Invalid("commit header".into()))?;
        let mut tree = None;
        let mut parents = Vec::new();
        for line in bytes[..boundary].split(|b| *b == b'\n') {
            if let Some(value) = line.strip_prefix(b"tree ") {
                tree = Some(parse_oid(value)?);
            }
            if let Some(value) = line.strip_prefix(b"parent ") {
                parents.push(parse_oid(value)?);
            }
        }
        Ok(Commit {
            tree: tree.ok_or_else(|| Error::Invalid("commit tree".into()))?,
            parents,
            message: bytes[boundary + 2..].to_vec(),
        })
    }
    /// Write a commit with the fixed service identity, now, in the local
    /// time zone: the bytes `git commit-tree` writes, message verbatim. Its
    /// tree and parents must be stored, as `commit-tree` requires.
    pub fn write_commit(&self, tree: Oid, parents: &[Oid], message: &str) -> Result<Oid> {
        self.with_repo(|repo| {
            let odb = repo.odb()?;
            for (oid, kind) in std::iter::once((tree, ObjectType::Tree))
                .chain(parents.iter().map(|p| (*p, ObjectType::Commit)))
            {
                let (_, found) = odb.read_header(oid)?;
                if found != kind {
                    return Err(Error::Invalid(format!("{oid} is a {found}, not a {kind}")));
                }
            }
            Ok(())
        })?;
        let signature = git2::Signature::now("agentfs", "agentfs@localhost")?;
        let when = signature.when();
        let offset = when.offset_minutes().abs();
        let ident = format!(
            "agentfs <agentfs@localhost> {} {}{:02}{:02}",
            when.seconds(),
            when.sign(),
            offset / 60,
            offset % 60
        );
        let mut data = format!("tree {tree}\n");
        for parent in parents {
            data.push_str(&format!("parent {parent}\n"));
        }
        data.push_str(&format!("author {ident}\ncommitter {ident}\n\n"));
        data.push_str(message);
        self.write_object(ObjectType::Commit, data.as_bytes())
    }

    /// Atomically apply all reference changes, checking every old value: each
    /// ref is locked, then every old value read under the locks, then all
    /// move. A name Git would refuse fails before any lock.
    pub fn ref_transaction(&self, updates: &[RefUpdate]) -> Result<()> {
        for u in updates {
            if !git2::Reference::is_valid_name(&u.refname) {
                return Err(Error::Invalid(format!("reference name {:?}", u.refname)));
            }
        }
        self.with_repo(|repo| {
            let mut transaction = repo.transaction()?;
            for u in updates {
                transaction.lock_ref(&u.refname)?;
            }
            for u in updates {
                let found = match repo.find_reference(&u.refname) {
                    Ok(reference) => reference.target(),
                    Err(err) if err.code() == ErrorCode::NotFound => None,
                    Err(err) => return Err(err.into()),
                };
                if found != u.expected_old {
                    return Err(Error::Git(format!(
                        "cannot lock ref '{}': expected {:?}, found {:?}",
                        u.refname, u.expected_old, found
                    )));
                }
                match (u.new, found) {
                    (Some(new), _) => transaction.set_target(&u.refname, new, None, "agentfs")?,
                    (None, Some(_)) => transaction.remove(&u.refname)?,
                    (None, None) => {}
                }
            }
            transaction.commit()?;
            Ok(())
        })
    }
    /// Whether the first commit is an ancestor of the second.
    pub fn is_ancestor(&self, ancestor: Oid, descendant: Oid) -> Result<bool> {
        let mut cmd = self.command(true);
        cmd.args([
            "merge-base",
            "--is-ancestor",
            &ancestor.to_string(),
            &descendant.to_string(),
        ]);
        let out = output(cmd)?;
        match out.status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err(classify(&out.stderr)),
        }
    }
    /// Make a private cache directory the exact tree, without Git filters.
    /// With `previous`, the tree the directory holds now, only entries that
    /// differ are rewritten (git-model §3); without it the directory is
    /// replaced whole.
    pub fn checkout_tree(&self, tree: Oid, dest: &Path, previous: Option<Oid>) -> Result<()> {
        if let Some(previous) = previous {
            return self.checkout_changes(previous, tree, dest);
        }
        if dest.exists() {
            std::fs::remove_dir_all(dest)?;
        }
        std::fs::create_dir_all(dest)?;
        self.checkout_entries(tree, dest)
    }
    fn checkout_entries(&self, tree: Oid, dest: &Path) -> Result<()> {
        for entry in self.list_tree(tree)? {
            self.checkout_entry(&entry, dest)?;
        }
        Ok(())
    }
    fn checkout_entry(&self, entry: &TreeEntry, dest: &Path) -> Result<()> {
        let path = dest.join(entry_name(&entry.name)?);
        match entry.mode {
            0o40000 => {
                std::fs::create_dir(&path)?;
                self.checkout_entries(entry.oid, &path)?;
            }
            0o100644 | 0o100755 => {
                std::fs::write(&path, self.read_blob(entry.oid)?)?;
                std::fs::set_permissions(
                    &path,
                    std::fs::Permissions::from_mode(entry.mode & 0o777),
                )?;
            }
            0o120000 => symlink(OsString::from_vec(self.read_blob(entry.oid)?), path)?,
            _ => return Err(Error::Invalid("unsupported checkout mode".into())),
        }
        Ok(())
    }
    /// `dest` holds `old`; leaves it holding `new`, touching only what differs.
    fn checkout_changes(&self, old: Oid, new: Oid, dest: &Path) -> Result<()> {
        if old == new {
            return Ok(());
        }
        let key = |e: TreeEntry| (e.name.clone(), e);
        let old: BTreeMap<Vec<u8>, TreeEntry> = self.list_tree(old)?.into_iter().map(key).collect();
        let new: BTreeMap<Vec<u8>, TreeEntry> = self.list_tree(new)?.into_iter().map(key).collect();
        for (name, was) in &old {
            let now = new.get(name);
            if now.is_some_and(|now| now.mode == was.mode && now.oid == was.oid) {
                continue;
            }
            if let Some(now) = now
                && now.mode == 0o40000
                && was.mode == 0o40000
            {
                self.checkout_changes(was.oid, now.oid, &dest.join(entry_name(name)?))?;
                continue;
            }
            let path = dest.join(entry_name(name)?);
            if was.mode == 0o40000 {
                std::fs::remove_dir_all(&path)?;
            } else {
                std::fs::remove_file(&path)?;
            }
            if let Some(now) = now {
                self.checkout_entry(now, dest)?;
            }
        }
        for (name, now) in &new {
            if !old.contains_key(name) {
                self.checkout_entry(now, dest)?;
            }
        }
        Ok(())
    }
}

/// A tree entry's name as a path component; `.` and `..` never leave `dest`.
fn entry_name(name: &[u8]) -> Result<OsString> {
    let name = OsString::from_vec(name.to_vec());
    if name.is_empty() || name == OsStr::new(".") || name == OsStr::new("..") {
        return Err(Error::Invalid("tree entry name".into()));
    }
    Ok(name)
}

// Git creates its skeleton before agentfs writes the config, with no
// core.fsync. Flush the files first, then every containing
// directory from the leaves up, including the state directory entry naming
// the repository. Symlinks are entries in their parent, not files to follow.
fn sync_initialized_repository(path: &Path) -> io::Result<()> {
    fn sync_tree(dir: &Path) -> io::Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_dir() {
                sync_tree(&entry.path())?;
            } else if kind.is_file() {
                std::fs::File::open(entry.path())?.sync_all()?;
            }
        }
        std::fs::File::open(dir)?.sync_all()
    }
    sync_tree(path)?;
    // The actor may have created the state directory immediately before the
    // repository. Flush both its contents and its entry in its parent.
    for dir in path.ancestors().skip(1).take(2) {
        std::fs::File::open(dir)?.sync_all()?;
    }
    Ok(())
}

// Run with nothing on stdin and collect both streams; the status is the caller's.
fn output(mut cmd: Command) -> io::Result<Output> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    crate::git_cli::counted(&mut cmd)?.wait_with_output()
}

// Spawn, feed stdin and wait. The exit status wins over a failed stdin write: a
// git that died of a full disk also breaks the pipe, and stderr says why.
fn run_command(mut cmd: Command, input: Option<&[u8]>) -> Result<Output> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    if input.is_some() {
        cmd.stdin(Stdio::piped());
    } else {
        cmd.stdin(Stdio::null());
    }
    let mut child = crate::git_cli::counted(&mut cmd)?;
    let (output, write_result) = std::thread::scope(|scope| {
        let writer = input.map(|bytes| {
            let mut stdin = child.stdin.take().expect("piped stdin");
            scope.spawn(move || stdin.write_all(bytes))
        });
        let output = child.wait_with_output();
        let write_result = writer
            .map(|writer| {
                writer
                    .join()
                    .map_err(|_| io::Error::other("git stdin writer panicked"))
            })
            .transpose();
        (output, write_result)
    });
    let output = output?;
    let write_result = write_result?;
    if !output.status.success() {
        return Err(classify(&output.stderr));
    }
    if let Some(result) = write_result {
        result?;
    }
    Ok(output)
}

fn replace_file(dir: &Path, name: &str, bytes: &[u8]) -> io::Result<()> {
    let temporary = dir.join(format!("{name}.agentfs-tmp"));
    let mut file = std::fs::File::create(&temporary)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    std::fs::rename(&temporary, dir.join(name))?;
    std::fs::File::open(dir)?.sync_all()
}

fn trim(bytes: &[u8]) -> &[u8] {
    bytes.strip_suffix(b"\n").unwrap_or(bytes)
}
fn parse_oid(bytes: &[u8]) -> Result<Oid> {
    let text = std::str::from_utf8(bytes).map_err(|_| Error::Invalid("object id".into()))?;
    Oid::from_str(text).map_err(|_| Error::Invalid("object id".into()))
}

// Only tree Git subprocesses inherit this one descriptor. The Arc keeps the
// FileLock alive through synchronous waits, so its Drop cannot explicitly
// unlock while a child is active. On a crash the kernel retains the lock on
// the shared open file until the child exits, even before PDEATHSIG arrives.
fn attach_lease(cmd: &mut Command, lease: &FileLock) {
    attach_lease_signal(cmd, lease, libc::SIGKILL);
}

fn attach_lease_signal(cmd: &mut Command, lease: &FileLock, signal: i32) {
    let fd = lease.file().as_raw_fd();
    #[cfg(target_os = "linux")]
    let parent = unsafe { libc::getpid() };
    // SAFETY: pre_exec runs after fork, before exec; it uses only async-signal-
    // safe syscalls and the fd remains open because the caller owns the lease.
    unsafe {
        cmd.pre_exec(move || {
            #[cfg(target_os = "linux")]
            {
                if libc::prctl(libc::PR_SET_PDEATHSIG, signal) == -1 {
                    return Err(io::Error::last_os_error());
                }
                if libc::getppid() != parent {
                    return Err(io::Error::from_raw_os_error(libc::ECHILD));
                }
            }
            let borrowed = BorrowedFd::borrow_raw(fd);
            let mut flags = rustix::io::fcntl_getfd(borrowed)?;
            flags.remove(rustix::io::FdFlags::CLOEXEC);
            rustix::io::fcntl_setfd(borrowed, flags)?;
            Ok(())
        });
    }
}

#[cfg(test)]
mod classify_tests {
    use super::*;

    #[test]
    fn git_no_space_is_enospc() {
        for stderr in [
            &b"error: unable to create temporary file: No space left on device\nfatal: bad\n"[..],
            b"fatal: write error: Disk quota exceeded\n",
        ] {
            let error = crate::tree_actor::Error::from(classify(stderr));
            assert_eq!(error.errno(), libc::ENOSPC);
        }
        let error = crate::tree_actor::Error::from(classify(b"fatal: not a git repository\n"));
        assert_eq!(error.errno(), libc::EIO);
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use crate::file_lock::Mode;
    use std::time::{Duration, Instant};

    // Invoked as a separate test process by the lifecycle test below.
    #[test]
    fn lease_child_process() {
        let Some(root) = std::env::var_os("AGENTFS_TREE_LEASE_TEST") else {
            return;
        };
        let root = PathBuf::from(root);
        let lease = FileLock::try_take(&root.join("actor.lock"), Mode::Exclusive)
            .unwrap()
            .unwrap();
        let mut command = Command::new("git");
        command
            .env_clear()
            .env("PATH", crate::git_cli::GIT_PATH)
            .env("LC_ALL", "C")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .args(["upload-pack", "--stateless-rpc"])
            .arg(root.join("repo.git"))
            // The test driver retains the write end even after this helper is
            // killed, so Git cannot exit on EOF before the death signal.
            .stdin(Stdio::inherit())
            .stdout(Stdio::null())
            .stderr(Stdio::from(
                std::fs::File::create(root.join("git.stderr")).unwrap(),
            ));
        let signal = if std::env::var_os("AGENTFS_TREE_LEASE_STOP").is_some() {
            libc::SIGSTOP
        } else {
            libc::SIGKILL
        };
        attach_lease_signal(&mut command, &lease, signal);
        let mut child = command.spawn().unwrap();
        std::fs::write(root.join("pid.tmp"), child.id().to_string()).unwrap();
        std::fs::rename(root.join("pid.tmp"), root.join("pid")).unwrap();
        child.wait().unwrap();
    }

    fn run_death_case(stop: bool) {
        let temp = tempfile::tempdir().unwrap();
        TreeGit::init(temp.path().join("repo.git")).unwrap();
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "tree_git::tests::lease_child_process",
                "--nocapture",
            ])
            .env("AGENTFS_TREE_LEASE_TEST", temp.path())
            .env_remove("AGENTFS_TREE_LEASE_STOP")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::from(
                std::fs::File::create(temp.path().join("helper.stderr")).unwrap(),
            ));
        if stop {
            command.env("AGENTFS_TREE_LEASE_STOP", "1");
        }
        let mut parent = command.spawn().unwrap();
        let keep_stdin = parent.stdin.take().unwrap();
        let diagnostic = || {
            format!(
                "helper stderr: {}; git stderr: {}",
                std::fs::read_to_string(temp.path().join("helper.stderr")).unwrap_or_default(),
                std::fs::read_to_string(temp.path().join("git.stderr")).unwrap_or_default(),
            )
        };
        let pidfile = temp.path().join("pid");
        let deadline = Instant::now() + Duration::from_secs(10);
        while !pidfile.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let pid: i32 = std::fs::read_to_string(pidfile)
            .unwrap_or_else(|err| panic!("helper did not publish Git pid: {err}; {}", diagnostic()))
            .parse()
            .unwrap();
        // Wait until Git has entered its blocking read; merely spawning it
        // does not prove it survived startup or is still attached to stdin.
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let status = std::fs::read_to_string(format!("/proc/{pid}/status"))
                .unwrap_or_else(|err| panic!("Git exited before ready: {err}; {}", diagnostic()));
            if status.lines().any(|line| line.starts_with("State:\tS")) {
                break;
            }
            assert!(
                !status.lines().any(|line| line.starts_with("State:\tZ")),
                "Git exited before ready; {}",
                diagnostic()
            );
            assert!(
                Instant::now() < deadline,
                "Git did not become ready; {}",
                diagnostic()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        parent.kill().unwrap();
        parent.wait().unwrap();
        if stop {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let status =
                    std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap_or_else(|err| {
                        panic!("Git exited before lease check: {err}; {}", diagnostic())
                    });
                if status.lines().any(|line| line.starts_with("State:\tT")) {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "Git did not receive parent death signal"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            assert!(
                FileLock::try_take(&temp.path().join("actor.lock"), Mode::Exclusive)
                    .unwrap()
                    .is_none()
            );
            assert_eq!(unsafe { libc::kill(pid, libc::SIGKILL) }, 0);
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if FileLock::try_take(&temp.path().join("actor.lock"), Mode::Exclusive)
                .unwrap()
                .is_some()
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "Git retained lease after parent death"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        drop(keep_stdin);
        // A zombie has exited even if the system has not yet reaped it.
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let status = std::fs::read_to_string(format!("/proc/{pid}/status"));
            if status.is_err()
                || status
                    .unwrap()
                    .lines()
                    .any(|line| line.starts_with("State:\tZ"))
            {
                break;
            }
            assert!(Instant::now() < deadline, "Git did not exit");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    #[test]
    fn killed_parent_leaves_lease_until_git_exits() {
        // Stop on parent death to inspect the lease, then exercise the actual
        // SIGKILL production path separately.
        run_death_case(true);
        run_death_case(false);
    }
}
