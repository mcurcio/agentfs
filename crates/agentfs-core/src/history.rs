//! The history repository: turns journal records into commits on the
//! volume's branch (design §3.3, §3.4, §9).
//!
//! The repository is bare and lives in the state area, never in the mount.
//! Each commit carries `Agentfs-Seq: <n>`, the last journal record it
//! includes, which is where recovery resumes after a crash.
//!
//! An operator restore or revert (design §15) is its own commit: its
//! subject is the operation's summary, and `Agentfs-Restore: <commit>` or
//! `Agentfs-Revert: <commit>` follows `Agentfs-Trigger: restore`.

use std::collections::BTreeSet;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Once;

use git2::{Index, IndexEntry, IndexTime, Oid, Repository, Signature, Time};

use crate::checks::{Checks, Refusal, path_refusal};
use crate::config::{ConfigError, Settings};
use crate::events::CheckStage;
use crate::fork::finish::FinishRecord;
use crate::identity::{Context, IdentityError, IdentityRenderer};
use crate::lfs::{ObjectCache, Store, attributes};
use crate::paths::{RepoPath, SpecialFile};
use crate::pending::PendingStore;
use crate::record::{Change, Operation, OperationKind, PatchAuthor, Record};

pub const SEQ_TRAILER: &str = "Agentfs-Seq";
/// The inbox number of the patch a commit applies (design §12).
pub const INBOX_TRAILER: &str = "Agentfs-Inbox";

/// How long libgit2 waits for a remote to accept a connection.
pub const SERVER_CONNECT_TIMEOUT_MS: std::ffi::c_int = 30_000;
/// How long libgit2 waits on a stalled read or write to a remote.
pub const SERVER_IO_TIMEOUT_MS: std::ffi::c_int = 300_000;

const MODE_FILE: u32 = 0o100644;
const MODE_EXECUTABLE: u32 = 0o100755;
const MODE_SYMLINK: u32 = 0o120000;

#[derive(Debug, thiserror::Error)]
pub enum HistoryError {
    #[error(transparent)]
    Git(#[from] git2::Error),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Identity(#[from] IdentityError),
    #[error("branch head {0} has no valid {SEQ_TRAILER} trailer")]
    MissingSeq(Oid),
    #[error("caching LFS content: {0}")]
    Lfs(#[source] io::Error),
}

/// Process-wide libgit2 setup, done once before any repository is opened:
///
/// - objects and refs are fsynced (libgit2 does not by default; design
///   §3.3);
/// - the system, XDG, global and ProgramData config search paths are
///   cleared, so no config outside the history repository's own file can
///   reach a volume: not `url.<base>.insteadOf` redirecting a push, not
///   `http.*`, not `core.excludesFile` (design §4.4, §11.1);
/// - the home directory is cleared, so `~/.ssh/known_hosts` never decides
///   or steers an SSH host key check (design §11.1);
/// - remote connections and reads/writes time out
///   ([`SERVER_CONNECT_TIMEOUT_MS`], [`SERVER_IO_TIMEOUT_MS`]), so a hung
///   remote ends as an unreachable push (design §11.2).
pub(crate) fn init_libgit2() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        libgit2_sys::init();
        // SAFETY: GIT_OPT_ENABLE_FSYNC_GITDIR takes a single int argument.
        let rc = unsafe {
            libgit2_sys::git_libgit2_opts(
                libgit2_sys::GIT_OPT_ENABLE_FSYNC_GITDIR as std::ffi::c_int,
                1 as std::ffi::c_int,
            )
        };
        assert_eq!(rc, 0, "libgit2 rejected GIT_OPT_ENABLE_FSYNC_GITDIR");
        for level in [
            git2::ConfigLevel::System,
            git2::ConfigLevel::XDG,
            git2::ConfigLevel::Global,
            git2::ConfigLevel::ProgramData,
        ] {
            // SAFETY: called once, inside `call_once`, before this process
            // opens any repository or config through agentfs; libgit2's
            // search paths are not otherwise touched by agentfs.
            let result = unsafe { git2::opts::set_search_path(level, "") };
            assert!(
                result.is_ok(),
                "libgit2 rejected clearing {level:?}: {result:?}"
            );
        }
        // No home directory either: libgit2's SSH transport reads
        // `~/.ssh/known_hosts` from it, and agentfs checks host keys only
        // against each remote's own agentfsKnownHosts (design §11.1).
        // SAFETY: GIT_OPT_SET_HOMEDIR takes one C string, which libgit2
        // copies; called once, before any transport is created.
        let rc = unsafe {
            libgit2_sys::git_libgit2_opts(
                libgit2_sys::GIT_OPT_SET_HOMEDIR as std::ffi::c_int,
                c"".as_ptr(),
            )
        };
        assert_eq!(rc, 0, "libgit2 rejected GIT_OPT_SET_HOMEDIR");
        // A remote that accepts the connection and then stalls must end as
        // a failed push, not hang its thread forever (design §11.2).
        // SAFETY: plain integer options, set once before any transport is
        // created.
        let connect = unsafe {
            git2::opts::set_server_connect_timeout_in_milliseconds(SERVER_CONNECT_TIMEOUT_MS)
        };
        assert!(
            connect.is_ok(),
            "libgit2 rejected the connect timeout: {connect:?}"
        );
        // SAFETY: as above.
        let io = unsafe { git2::opts::set_server_timeout_in_milliseconds(SERVER_IO_TIMEOUT_MS) };
        assert!(io.is_ok(), "libgit2 rejected the I/O timeout: {io:?}");
        // SSH remotes go through agentfs's own transport (design §11.1).
        #[cfg(feature = "ssh")]
        crate::ssh_transport::register();
    });
}

pub struct History {
    repo: Repository,
    settings: Settings,
}

/// What one commit did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Committed {
    /// `None` when the records left the tree unchanged.
    pub commit: Option<Oid>,
    pub last_seq: u64,
    /// Changes dropped at commit time because the managed `.gitattributes`
    /// block could not be written otherwise (design §7), with why: raise
    /// `PathRefused` with stage `commit` for each.
    pub refused: Vec<(RepoPath, Refusal)>,
    /// LFS writes stored as blobs for the same reason.
    pub lfs_as_blobs: usize,
}

impl History {
    /// Creates a bare history repository for `volume` at `git_dir`.
    pub fn init(git_dir: &Path, volume: &str, branch: Option<&str>) -> Result<Self, HistoryError> {
        init_libgit2();
        let repo = Repository::init_opts(
            git_dir,
            git2::RepositoryInitOptions::new().bare(true).mkpath(true),
        )?;
        {
            let mut config = repo.config()?.open_level(git2::ConfigLevel::Local)?;
            config.set_str("agentfs.volume", volume)?;
            if let Some(branch) = branch {
                config.set_str("agentfs.branch", branch)?;
            }
            // Keep a reflog of every branch update, for recovery.
            config.set_str("core.logAllRefUpdates", "always")?;
        }
        for name in ["exclude", "attributes"] {
            let path = git_dir.join("info").join(name);
            if !path.exists() {
                std::fs::write(path, "")?;
            }
        }
        Self::open(git_dir)
    }

    pub fn open(git_dir: &Path) -> Result<Self, HistoryError> {
        init_libgit2();
        let repo = Repository::open_bare(git_dir)?;
        let settings = Settings::load(git_dir)?;
        Ok(Self { repo, settings })
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    /// Makes `includes` exactly the `include.path` entries of the
    /// repository's own config, in order (design §4.3). The settings of
    /// this `History` are not reloaded; open it again to see them.
    pub fn set_includes(&self, includes: &[PathBuf]) -> Result<(), HistoryError> {
        write_includes(self.repo.path(), includes)
    }

    pub fn repo(&self) -> &Repository {
        &self.repo
    }

    /// The branch head, if the branch exists yet.
    pub fn head(&self) -> Result<Option<git2::Commit<'_>>, HistoryError> {
        match self.repo.find_reference(&self.settings.branch_ref) {
            Ok(r) => Ok(Some(r.peel_to_commit()?)),
            Err(e) if e.code() == git2::ErrorCode::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// The last journal sequence number in history.
    pub fn head_seq(&self) -> Result<Option<u64>, HistoryError> {
        let Some(head) = self.head()? else {
            return Ok(None);
        };
        let message = head.message_raw().unwrap_or_default();
        seq_from_message(message)
            .map(Some)
            .ok_or(HistoryError::MissingSeq(head.id()))
    }

    /// Commits `records` (already grouped) as one commit on the branch.
    ///
    /// Writes stored as LFS (design §7) are hashed into `lfs`, the object
    /// cache, and committed as pointer blobs; the root `.gitattributes` in
    /// history then gets its managed block rebuilt
    /// ([`crate::lfs::attributes`]). The cache object is durable before the
    /// branch moves.
    ///
    /// A pointer is only committed when its managed line can be written
    /// too, so it never lands at a path no clone or upload treats as LFS.
    /// When the block cannot be placed (the root `.gitattributes` is not a
    /// regular file) or the composed file fails `checks` (design §3.4), the
    /// commit falls back, step by step, and never fails for it:
    ///
    /// 1. this commit's LFS writes are stored as blobs, each checked
    ///    against `checks` as a blob and dropped if refused;
    /// 2. then its writes at or beneath the root `.gitattributes` are
    ///    dropped too (refused with the composed file's reason), and the
    ///    block is rebuilt on the head's file;
    /// 3. then the head's root `.gitattributes` is kept as it is.
    ///
    /// Dropped changes are listed in [`Committed::refused`], for
    /// `PathRefused` alarms with stage `commit`.
    ///
    /// Records at or below the head's `Agentfs-Seq` are already in history
    /// and are skipped.
    pub fn commit(
        &self,
        records: &[Record],
        pending: &PendingStore,
        lfs: &ObjectCache,
        checks: &Checks,
        identities: &IdentityRenderer,
        context: &Context,
    ) -> Result<Committed, HistoryError> {
        let last = records.last().expect("commit needs at least one record");
        let head = self.head()?;
        // Records already in history (a crash between a commit and the
        // journal's truncation) are not applied again: a move is not
        // idempotent.
        let done = head
            .as_ref()
            .and_then(|h| seq_from_message(h.message_raw().unwrap_or_default()));
        let records = &records[records.partition_point(|r| done.is_some_and(|d| r.seq <= d))..];
        if records.is_empty() {
            return Ok(Committed {
                commit: None,
                last_seq: last.seq,
                refused: Vec::new(),
                lfs_as_blobs: 0,
            });
        }
        let head_tree = head.as_ref().map(|h| h.tree()).transpose()?;
        let managed = match &head_tree {
            Some(tree) => attributes::managed_paths(&self.repo, tree)?,
            None => BTreeSet::new(),
        };
        let changes: Vec<&Change> = records.iter().flat_map(|r| &r.draft.changes).collect();
        let mut fallback = Fallback::None;
        // Why the attributes could not be written, once they could not.
        let mut attributes_refusal: Option<Refusal> = None;
        let (mut index, refused, lfs_as_blobs) = loop {
            let mut index = Index::new()?;
            if let Some(tree) = &head_tree {
                index.read_tree(tree)?;
            }
            let lfs_as_blob = fallback >= Fallback::LfsAsBlobs;
            let mut refused = Vec::new();
            let mut lfs_as_blobs = 0;
            let mut written = Written::default();
            for change in &changes {
                if let Some(reason) = &attributes_refusal
                    && !matches!(change, Change::Remove { .. })
                    && attributes::at_or_beneath_root(change.path())
                {
                    refused.push((change.path().clone(), reason.clone()));
                    continue;
                }
                if lfs_as_blob
                    && let Change::Write {
                        path,
                        pending: id,
                        store: Store::Lfs,
                        ..
                    } = change
                {
                    let content = pending.path(*id);
                    let refusal = checks.check_file_stored(
                        path,
                        &content,
                        CheckStage::Commit,
                        Store::Blob,
                    )?;
                    if let Some(refusal) = refusal {
                        refused.push((path.clone(), refusal));
                        continue;
                    }
                    lfs_as_blobs += 1;
                }
                self.apply(&mut index, change, pending, lfs, lfs_as_blob)?;
                written.note(change, lfs_as_blob, &managed);
            }
            if fallback == Fallback::KeepHeadAttributes {
                break (index, refused, lfs_as_blobs);
            }
            match self.update_managed_attributes(&mut index, &managed, &written, checks)? {
                None => break (index, refused, lfs_as_blobs),
                Some(refusal) => {
                    fallback = match fallback {
                        Fallback::None if !written.lfs.is_empty() => Fallback::LfsAsBlobs,
                        Fallback::None | Fallback::LfsAsBlobs => {
                            attributes_refusal = Some(refusal);
                            Fallback::DropAttributes
                        }
                        Fallback::DropAttributes | Fallback::KeepHeadAttributes => {
                            Fallback::KeepHeadAttributes
                        }
                    };
                }
            }
        };
        if fallback == Fallback::KeepHeadAttributes {
            self.keep_head_attributes(&mut index, head_tree.as_ref())?;
        }
        let tree_id = index.write_tree_to(&self.repo)?;
        crate::failpoints::hit("commit.after_tree")?;
        if head.as_ref().is_some_and(|h| h.tree_id() == tree_id) {
            return Ok(Committed {
                commit: None,
                last_seq: last.seq,
                refused,
                lfs_as_blobs,
            });
        }
        let tree = self.repo.find_tree(tree_id)?;

        let first = &records[0];
        let author = match first
            .draft
            .operation
            .as_ref()
            .and_then(|op| patch_author(op.author.as_ref()))
        {
            Some(author) => author,
            None => {
                let author = identities.author(first.draft.caller)?;
                Signature::new(&author.name, &author.email, &time(last.draft.time_ms))?
            }
        };
        let committer = identities.committer();
        let committer = Signature::now(&committer.name, &committer.email)?;
        let message = message(records, context);
        let parents: Vec<&git2::Commit<'_>> = head.iter().collect();
        let commit = self
            .repo
            .commit(None, &author, &committer, &message, &tree, &parents)?;
        // Objects before the ref that names them (design §3.3).
        self.sync_object_dirs()?;
        crate::failpoints::hit("commit.after_object")?;

        let log = format!("agentfs: record through seq {}", last.seq);
        match &head {
            Some(h) => self.repo.reference_matching(
                &self.settings.branch_ref,
                commit,
                true,
                h.id(),
                &log,
            )?,
            None => self
                .repo
                .reference(&self.settings.branch_ref, commit, false, &log)?,
        };
        crate::failpoints::hit("commit.after_ref")?;
        self.sync_ref_dirs()?;
        crate::failpoints::hit("commit.after_sync")?;
        Ok(Committed {
            commit: Some(commit),
            last_seq: last.seq,
            refused,
            lfs_as_blobs,
        })
    }

    /// Applies `change` to `index`; with `lfs_as_blob`, an LFS write is
    /// stored as a blob.
    fn apply(
        &self,
        index: &mut Index,
        change: &Change,
        pending: &PendingStore,
        lfs: &ObjectCache,
        lfs_as_blob: bool,
    ) -> Result<(), HistoryError> {
        match change {
            Change::Remove { path } => remove_all(index, path)?,
            Change::Write {
                path,
                pending: id,
                executable,
                store,
            } => {
                let blob = match store {
                    Store::Lfs if lfs_as_blob => self.repo.blob_path(&pending.path(*id))?,
                    Store::Blob => self.repo.blob_path(&pending.path(*id))?,
                    Store::Lfs => {
                        let pointer = lfs
                            .insert_file(&pending.path(*id))
                            .and_then(|p| {
                                crate::failpoints::hit("lfs.after_cache_insert").map(|()| p)
                            })
                            .map_err(HistoryError::Lfs)?;
                        self.repo.blob(pointer.encode().as_bytes())?
                    }
                };
                let mode = if *executable {
                    MODE_EXECUTABLE
                } else {
                    MODE_FILE
                };
                make_room(index, path)?;
                index.add(&entry(path, blob, mode))?;
            }
            Change::Symlink { path, target } => {
                let blob = self.repo.blob(target.as_bytes())?;
                make_room(index, path)?;
                index.add(&entry(path, blob, MODE_SYMLINK))?;
            }
            Change::Move { from, to } => move_all(index, from, to)?,
        }
        Ok(())
    }

    /// Rebuilds the managed block of the root `.gitattributes` in `index`
    /// (design §7): one line for every path stored as LFS. `managed` is the
    /// head's block (agentfs wrote it), `written` what this commit wrote. A
    /// path stays managed while it is a regular file not since rewritten
    /// as a blob or symlink; the workload's own content is kept byte for
    /// byte, and any block it wrote is replaced.
    ///
    /// Returns why the block cannot be written, leaving `index` as it was:
    /// the root `.gitattributes` is a symlink or directory while there are
    /// paths to manage, or the composed file fails `checks`.
    fn update_managed_attributes(
        &self,
        index: &mut Index,
        managed: &BTreeSet<RepoPath>,
        written: &Written,
        checks: &Checks,
    ) -> Result<Option<Refusal>, HistoryError> {
        let is_file = |index: &Index, path: &RepoPath| {
            index
                .get_path(Path::new(path.as_str()), 0)
                .is_some_and(|e| matches!(e.mode, MODE_FILE | MODE_EXECUTABLE))
        };
        let paths: BTreeSet<RepoPath> = managed
            .iter()
            .filter(|p| !written.other.contains(*p))
            .chain(written.lfs.iter())
            .filter(|p| is_file(index, p))
            .cloned()
            .collect();
        let root = Path::new(attributes::ROOT_GITATTRIBUTES);
        let existing = index.get_path(root, 0);
        let (content, mode) = match &existing {
            Some(e) if matches!(e.mode, MODE_FILE | MODE_EXECUTABLE) => {
                (self.repo.find_blob(e.id)?.content().to_vec(), e.mode)
            }
            // Nothing to add, nothing to remove: the common case.
            Some(_) | None if paths.is_empty() => return Ok(None),
            // Not recorded since agentfs refuses it (design §3.4), but
            // history may hold one from before.
            Some(_) => {
                return Ok(Some(Refusal::SymlinkedSpecialFile {
                    file: SpecialFile::Gitattributes,
                }));
            }
            None => {
                let dir = format!("{}/", attributes::ROOT_GITATTRIBUTES);
                if index.iter().any(|e| e.path.starts_with(dir.as_bytes())) {
                    return Ok(Some(Refusal::SpecialFileNotBlob {
                        file: SpecialFile::Gitattributes,
                    }));
                }
                (Vec::new(), MODE_FILE)
            }
        };
        let (user, _) = attributes::split(&content);
        let had_block = user.len() != content.len();
        if paths.is_empty() && !had_block {
            return Ok(None);
        }
        if paths.is_empty() && user.is_empty() {
            if existing.is_some() {
                index.remove_path(root)?;
            }
            return Ok(None);
        }
        let composed = attributes::compose(&user, &paths);
        let path = root_attributes_path();
        if let Some(refusal) = checks.check_blob(&path, &composed) {
            return Ok(Some(refusal));
        }
        let blob = self.repo.blob(&composed)?;
        index.add(&entry(&path, blob, mode))?;
        Ok(None)
    }

    /// Puts the head's root `.gitattributes` back into `index` as it is,
    /// the last resort of [`History::commit`] when no rebuilt block passes
    /// the checks (for example after the limits were lowered).
    fn keep_head_attributes(
        &self,
        index: &mut Index,
        head_tree: Option<&git2::Tree<'_>>,
    ) -> Result<(), HistoryError> {
        let root = Path::new(attributes::ROOT_GITATTRIBUTES);
        let head_entry = match head_tree.map(|t| t.get_path(root)) {
            Some(Ok(e)) if matches!(e.filemode() as u32, MODE_FILE | MODE_EXECUTABLE) => Some(e),
            Some(Ok(_)) | None => None,
            Some(Err(e)) if e.code() == git2::ErrorCode::NotFound => None,
            Some(Err(e)) => return Err(e.into()),
        };
        match head_entry {
            Some(e) => {
                let path = root_attributes_path();
                make_room(index, &path)?;
                index.add(&entry(&path, e.id(), e.filemode() as u32))?;
            }
            None => {
                if index.get_path(root, 0).is_some() {
                    index.remove_path(root)?;
                }
            }
        }
        Ok(())
    }

    /// Makes a commit's objects durable before its ref moves (design
    /// §3.3): with fsync enabled, libgit2 fsyncs each loose object and its
    /// fan-out directory (`objects/xx`), but not `objects/` when it
    /// creates a fan-out directory there. Without this, a power cut after
    /// the ref update could leave the branch naming a commit whose
    /// objects have no durable directory entry.
    fn sync_object_dirs(&self) -> io::Result<()> {
        crate::failpoints::hit("commit.sync_objects")?;
        File::open(self.repo.path().join("objects"))?.sync_all()
    }

    /// After the ref update: libgit2 fsyncs the ref file and its
    /// directory, but not the directories above it that the first commit
    /// of a branch creates.
    fn sync_ref_dirs(&self) -> io::Result<()> {
        self.sync_dirs_of(&self.settings.branch_ref)
    }

    /// The directories above the ref `refname`, synced.
    fn sync_dirs_of(&self, refname: &str) -> io::Result<()> {
        let git_dir = self.repo.path();
        let mut dir = git_dir.join(refname);
        while dir.pop() && dir.starts_with(git_dir) {
            File::open(&dir)?.sync_all()?;
        }
        Ok(())
    }

    /// Removes the ref lock files a crash left: `HEAD.lock`,
    /// `packed-refs.lock` and every `refs/**/*.lock` (design §3.3). A ref
    /// update writes `<ref>.lock` and renames it over the ref; a process
    /// killed, or a power cut, in between leaves the lock, and libgit2
    /// then refuses every later update of that ref, so no commit could
    /// ever be made again. Only the holder of the state-area lock may call
    /// this (design §2.3): it is the one process that writes the history,
    /// so any lock it finds at open is stale. Returns what it removed.
    pub fn remove_stale_ref_locks(&self) -> io::Result<Vec<PathBuf>> {
        let git_dir = self.repo.path();
        let mut removed = Vec::new();
        for name in ["HEAD.lock", "packed-refs.lock"] {
            let path = git_dir.join(name);
            if std::fs::symlink_metadata(&path).is_ok_and(|m| m.is_file()) {
                std::fs::remove_file(&path)?;
                removed.push(path);
            }
        }
        let mut dirs = vec![git_dir.join("refs")];
        while let Some(dir) = dirs.pop() {
            let entries = match std::fs::read_dir(&dir) {
                Ok(entries) => entries,
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e),
            };
            for entry in entries {
                let entry = entry?;
                let kind = entry.file_type()?;
                let path = entry.path();
                if kind.is_dir() {
                    dirs.push(path);
                } else if kind.is_file() && path.extension().is_some_and(|e| e == "lock") {
                    std::fs::remove_file(&path)?;
                    removed.push(path);
                }
            }
        }
        if !removed.is_empty() {
            File::open(git_dir)?.sync_all()?;
        }
        removed.sort();
        Ok(removed)
    }
}

/// What one commit wrote, for the managed `.gitattributes` block: the
/// paths last written as LFS, and those last written as a blob or symlink.
#[derive(Default)]
struct Written {
    lfs: BTreeSet<RepoPath>,
    other: BTreeSet<RepoPath>,
}

impl Written {
    /// Notes `change`; with `lfs_as_blob`, an LFS write was stored as a
    /// blob.
    fn note(&mut self, change: &Change, lfs_as_blob: bool, managed: &BTreeSet<RepoPath>) {
        match change {
            Change::Write {
                path,
                store: Store::Lfs,
                ..
            } if !lfs_as_blob => {
                self.other.remove(path);
                self.lfs.insert(path.clone());
            }
            Change::Write { path, .. } | Change::Symlink { path, .. } => {
                self.lfs.remove(path);
                self.other.insert(path.clone());
            }
            Change::Remove { .. } => {}
            Change::Move { from, to } => self.note_move(from, to, managed),
        }
    }

    /// A move carries each LFS path beneath `from` to its place beneath `to`,
    /// managed line and all (a pointer without its line would check out as
    /// a plain file), and what was managed beneath `to` stops being.
    fn note_move(&mut self, from: &RepoPath, to: &RepoPath, managed: &BTreeSet<RepoPath>) {
        let at_or_under = |p: &RepoPath, dir: &RepoPath| {
            p == dir
                || p.as_str()
                    .strip_prefix(dir.as_str())
                    .is_some_and(|r| r.starts_with('/'))
        };
        let moved: Vec<RepoPath> = managed
            .iter()
            .filter(|p| !self.other.contains(*p))
            .chain(self.lfs.iter())
            .filter(|p| at_or_under(p, from))
            .filter_map(|p| reroot(p, from, to))
            // Left out of the index by `move_all`.
            .filter(|p| path_refusal(p).is_none())
            .collect();
        let gone: Vec<RepoPath> = managed
            .iter()
            .chain(self.lfs.iter())
            .chain(self.other.iter())
            .filter(|p| at_or_under(p, to) || at_or_under(p, from))
            .cloned()
            .collect();
        for p in gone {
            self.lfs.remove(&p);
            self.other.insert(p);
        }
        for p in moved {
            self.other.remove(&p);
            self.lfs.insert(p);
        }
    }
}

/// `path`, at or beneath `from`, at its place beneath `to`.
pub(crate) fn reroot(path: &RepoPath, from: &RepoPath, to: &RepoPath) -> Option<RepoPath> {
    let rest = path.as_str().strip_prefix(from.as_str())?;
    if rest.is_empty() {
        return Some(to.clone());
    }
    let rest = rest.strip_prefix('/')?;
    RepoPath::new(format!("{to}/{rest}")).ok()
}

/// Moves every entry at and beneath `from` to its place beneath `to`,
/// replacing whatever was at and beneath `to` (design §3.1): same blob,
/// same mode. An entry the path rules refuse at its new place
/// ([`crate::checks::path_refusal`]: beneath a `.gitmodules` or
/// `.gitattributes`, or too long) leaves history instead: recording it
/// there would have refused it, and git's fsck rejects such a tree. The
/// move's reconciliation reports it ([`crate::reconcile`]).
fn move_all(index: &mut Index, from: &RepoPath, to: &RepoPath) -> Result<(), git2::Error> {
    let prefix = format!("{from}/");
    let moved: Vec<IndexEntry> = index
        .iter()
        .filter(|e| e.path == from.as_str().as_bytes() || e.path.starts_with(prefix.as_bytes()))
        .collect();
    remove_all(index, from)?;
    remove_all(index, to)?;
    make_room(index, to)?;
    for mut e in moved {
        let rest = &e.path[from.as_str().len()..];
        let mut path = to.as_str().as_bytes().to_vec();
        path.extend_from_slice(rest);
        let refused = std::str::from_utf8(&path)
            .ok()
            .and_then(|p| RepoPath::new(p).ok())
            .is_none_or(|p| path_refusal(&p).is_some());
        if refused {
            continue;
        }
        e.path = path;
        index.add(&e)?;
    }
    Ok(())
}

/// How far [`History::commit`] has fallen back because the managed block
/// could not be written; each step includes the ones before.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Fallback {
    None,
    /// This commit's LFS writes are stored as blobs.
    LfsAsBlobs,
    /// Its writes at or beneath the root `.gitattributes` are dropped.
    DropAttributes,
    /// The head's root `.gitattributes` is kept as it is.
    KeepHeadAttributes,
}

/// A commit on the branch that applies a patch (design §12).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InboxCommit {
    /// The branch commit.
    pub commit: Oid,
    /// The patch it applied (`Agentfs-Patch`).
    pub patch: Oid,
}

impl History {
    /// Records that the patch of `op` (kind patch, with an inbox number)
    /// is applied when it changes nothing (design §12, the exception to
    /// §3.4's no empty commits): a commit with the head's tree and
    /// `Agentfs-Seq`, `Agentfs-Trigger: patch`, `Agentfs-Patch` and
    /// `Agentfs-Inbox`, authored like any patch commit.
    pub fn commit_patch_marker(
        &self,
        op: &Operation,
        identities: &IdentityRenderer,
        context: &Context,
    ) -> Result<Oid, HistoryError> {
        if self.head()?.is_none() {
            return Err(git2::Error::from_str(
                "the branch does not exist: nothing to apply a patch to",
            )
            .into());
        }
        let seq = self.head_seq()?.unwrap_or_default();
        let author = match patch_author(op.author.as_ref()) {
            Some(author) => author,
            None => {
                let author = identities.author(op.caller)?;
                Signature::now(&author.name, &author.email)?
            }
        };
        let committer = identities.committer();
        let committer = Signature::now(&committer.name, &committer.email)?;
        let mut message = format!("{}\n\n", op.summary);
        message.push_str(&format!("{SEQ_TRAILER}: {seq}\n"));
        message.push_str(&format!(
            "Agentfs-Trigger: {}\n",
            op.kind.trigger().as_str()
        ));
        message.push_str(&format!("{}: {}\n", op.kind.trailer(), op.source));
        if let Some(n) = op.inbox {
            message.push_str(&format!("{INBOX_TRAILER}: {n}\n"));
        }
        push_context_trailers(&mut message, context);
        message.push_str(&format!("Agentfs-Uid: {}\n", op.caller.uid));
        let log = format!("agentfs: patch marker through seq {seq}");
        self.marker(&message, &author, &committer, &log)
    }

    /// An empty commit on the branch with the head's tree and `message`
    /// (§3.4's exceptions: the patch marker, the checkpoint marker and a
    /// fork's finish commit), objects and refs synced. Fails if the branch
    /// does not exist.
    pub fn commit_marker(
        &self,
        message: &str,
        signature: &Signature<'_>,
    ) -> Result<Oid, HistoryError> {
        self.marker(message, signature, signature, "agentfs: marker commit")
    }

    /// Merges the checkpoint commit `change` back into the branch (design
    /// §19.2 item 11, [`crate::checkpoint`]): a marker with parents
    /// `[head, change]` and the head's tree, its message `message` then
    /// `Agentfs-Seq` (the head's), `Agentfs-Trigger: checkpoint` and
    /// `Agentfs-Checkpoint: <change>`. §3.4's marker exception, like the
    /// patch marker: it writes no file. First-parent scans (the inbox
    /// trailers, the sequence) pass it as any marker. Objects and refs
    /// synced; the branch moves only if it is still at the head read.
    pub fn commit_checkpoint_marker(
        &self,
        change: Oid,
        message: &str,
        author: &Signature<'_>,
        committer: &Signature<'_>,
    ) -> Result<Oid, HistoryError> {
        use crate::checkpoint::{CHECKPOINT_TRAILER, CHECKPOINT_TRIGGER};
        let seq = self.head_seq()?.unwrap_or_default();
        let message = format!(
            "{}\n\n{SEQ_TRAILER}: {seq}\nAgentfs-Trigger: {CHECKPOINT_TRIGGER}\n\
             {CHECKPOINT_TRAILER}: {change}\n",
            message.trim_end()
        );
        let change = self.repo.find_commit(change)?;
        let log = format!("agentfs: checkpoint marker through seq {seq}");
        self.marker_with(&message, author, committer, &log, Some(&change))
    }

    /// A commit of `tree` on `parents` at `refname`, outside the branch,
    /// by `(author, committer)`, moved only from `expected` (`None`:
    /// created, never replaced); objects and the ref synced. The change
    /// line's write ([`crate::checkpoint`]).
    pub(crate) fn commit_ref(
        &self,
        refname: &str,
        expected: Option<Oid>,
        tree: Oid,
        parents: &[Oid],
        message: &str,
        (author, committer): (&Signature<'_>, &Signature<'_>),
    ) -> Result<Oid, HistoryError> {
        let tree = self.repo.find_tree(tree)?;
        let parents = parents
            .iter()
            .map(|p| self.repo.find_commit(*p))
            .collect::<Result<Vec<_>, _>>()?;
        let parents: Vec<&git2::Commit<'_>> = parents.iter().collect();
        let commit = self
            .repo
            .commit(None, author, committer, message, &tree, &parents)?;
        self.sync_object_dirs()?;
        let log = "agentfs: checkpoint";
        match expected {
            Some(old) => self
                .repo
                .reference_matching(refname, commit, true, old, log)?,
            None => self.repo.reference(refname, commit, false, log)?,
        };
        self.sync_dirs_of(refname)?;
        Ok(commit)
    }

    fn marker(
        &self,
        message: &str,
        author: &Signature<'_>,
        committer: &Signature<'_>,
        log: &str,
    ) -> Result<Oid, HistoryError> {
        self.marker_with(message, author, committer, log, None)
    }

    /// [`History::marker`], with `merged` as a second parent.
    fn marker_with(
        &self,
        message: &str,
        author: &Signature<'_>,
        committer: &Signature<'_>,
        log: &str,
        merged: Option<&git2::Commit<'_>>,
    ) -> Result<Oid, HistoryError> {
        let head = self.head()?.ok_or_else(|| {
            git2::Error::from_str("the branch does not exist: there is nothing to mark")
        })?;
        let tree = head.tree()?;
        let mut parents = vec![&head];
        parents.extend(merged);
        let commit = self
            .repo
            .commit(None, author, committer, message, &tree, &parents)?;
        self.sync_object_dirs()?;
        self.repo
            .reference_matching(&self.settings.branch_ref, commit, true, head.id(), log)?;
        self.sync_ref_dirs()?;
        Ok(commit)
    }

    /// A commit of `tree` on `parent`, outside the branch, and `refname`
    /// created at it (design §11.5, a fork's change). Create-only: when
    /// `refname` already names a commit of `tree` on `parent`, that commit
    /// is returned and nothing is written; when it names anything else,
    /// this fails. Objects and the ref are synced.
    pub fn commit_detached(
        &self,
        refname: &str,
        tree: Oid,
        parent: Oid,
        message: &str,
        signature: &Signature<'_>,
    ) -> Result<Oid, HistoryError> {
        if let Ok(existing) = self.repo.find_reference(refname) {
            let commit = existing.peel_to_commit()?;
            if commit.tree_id() == tree && commit.parent_ids().eq([parent]) {
                return Ok(commit.id());
            }
            return Err(git2::Error::from_str(&format!(
                "{refname} already names {}, which is not a commit of {tree} on {parent}",
                commit.id()
            ))
            .into());
        }
        let tree = self.repo.find_tree(tree)?;
        let parent = self.repo.find_commit(parent)?;
        let commit = self
            .repo
            .commit(None, signature, signature, message, &tree, &[&parent])?;
        self.sync_object_dirs()?;
        self.repo
            .reference(refname, commit, false, "agentfs: detached commit")?;
        self.sync_dirs_of(refname)?;
        Ok(commit)
    }

    /// Every patch commit on the branch's first-parent history from the
    /// head back to `since` (exclusive; the whole history when `None` or
    /// when `since` is not on it), by inbox number (design §12). Only a
    /// commit whose last message paragraph carries `Agentfs-Trigger:
    /// patch`, `Agentfs-Patch` and `Agentfs-Inbox` counts.
    pub fn find_inbox_commits(
        &self,
        since: Option<Oid>,
    ) -> Result<std::collections::BTreeMap<u64, InboxCommit>, HistoryError> {
        let mut found = std::collections::BTreeMap::new();
        let Some(head) = self.head()? else {
            return Ok(found);
        };
        let mut walk = self.repo.revwalk()?;
        walk.push(head.id())?;
        walk.simplify_first_parent()?;
        if let Some(since) = since
            && self.repo.odb()?.exists(since)
            && (since == head.id() || self.repo.graph_descendant_of(head.id(), since)?)
        {
            walk.hide(since)?;
        }
        for oid in walk {
            let oid = oid?;
            let commit = self.repo.find_commit(oid)?;
            if let Some((n, patch)) = inbox_from_message(commit.message_raw().unwrap_or_default()) {
                // The newest commit for an inbox number wins.
                found.entry(n).or_insert(InboxCommit { commit: oid, patch });
            }
        }
        Ok(found)
    }
}

/// The inbox number and patch of a commit that applies a patch: read only
/// from the message's last paragraph, which agentfs writes itself, and only
/// with `Agentfs-Trigger: patch` there (design §12). A path in the change
/// list or a line of the patch's own message cannot forge it.
pub fn inbox_from_message(message: &str) -> Option<(u64, Oid)> {
    let trimmed = message.trim_end_matches('\n');
    let last = trimmed.rsplit("\n\n").next()?;
    let mut trigger = false;
    let mut inbox = None;
    let mut patch = None;
    let patch_prefix = format!("{}: ", OperationKind::Patch.trailer());
    let inbox_prefix = format!("{INBOX_TRAILER}: ");
    for line in last.lines() {
        if line == "Agentfs-Trigger: patch" {
            trigger = true;
        } else if let Some(n) = line.strip_prefix(&inbox_prefix) {
            inbox = n.trim().parse::<u64>().ok();
        } else if let Some(oid) = line.strip_prefix(&patch_prefix) {
            patch = Oid::from_str(oid.trim()).ok();
        }
    }
    match (trigger, inbox, patch) {
        (true, Some(n), Some(patch)) if n > 0 => Some((n, patch)),
        _ => None,
    }
}

/// The checkpoint commit a checkpoint marker merged back (design §19.2
/// item 11): read only from the message's last paragraph, which agentfs
/// writes itself, and only with `Agentfs-Trigger: checkpoint` there, like
/// [`inbox_from_message`].
pub fn checkpoint_from_message(message: &str) -> Option<Oid> {
    use crate::checkpoint::{CHECKPOINT_TRAILER, CHECKPOINT_TRIGGER};
    let trimmed = message.trim_end_matches('\n');
    let last = trimmed.rsplit("\n\n").next()?;
    let trigger_line = format!("Agentfs-Trigger: {CHECKPOINT_TRIGGER}");
    let prefix = format!("{CHECKPOINT_TRAILER}: ");
    let mut trigger = false;
    let mut change = None;
    for line in last.lines() {
        if line == trigger_line {
            trigger = true;
        } else if let Some(oid) = line.strip_prefix(&prefix) {
            change = Oid::from_str(oid.trim()).ok();
        }
    }
    change.filter(|_| trigger)
}

/// The finish record of the fork's finish commit `finish` (design §11.5):
/// read only from the message's last paragraph, which agentfs writes
/// itself, and only with `Agentfs-Trigger: finish` there. Every field is
/// checked against its grammar (names as volume names, full commit ids, a
/// decimal count) and each may appear once; anything else is `None`, so a
/// path or a line the workload wrote cannot forge one.
pub fn finish_from_message(message: &str, finish: Oid) -> Option<FinishRecord> {
    use crate::fork::finish as f;
    let trimmed = message.trim_end_matches('\n');
    let last = trimmed.rsplit("\n\n").next()?;
    let trigger_line = format!("Agentfs-Trigger: {}", f::FINISH_TRIGGER);
    let mut trigger = false;
    let mut fields: std::collections::BTreeMap<&str, &str> = std::collections::BTreeMap::new();
    for line in last.lines() {
        if line == trigger_line {
            trigger = true;
            continue;
        }
        let Some((key, value)) = line.split_once(": ") else {
            continue;
        };
        let known = [
            f::FORK_TRAILER,
            f::SOURCE_TRAILER,
            f::BASE_TRAILER,
            f::CHANGE_TRAILER,
            f::FORK_REMOTE_TRAILER,
            f::UNRECORDED_TRAILER,
        ];
        if known.contains(&key) && fields.insert(key, value).is_some() {
            return None;
        }
    }
    if !trigger {
        return None;
    }
    let name = |key: &str| {
        let value = *fields.get(key)?;
        crate::config::validate_name(key, value).ok()?;
        Some(value.to_owned())
    };
    let oid = |value: &str| {
        if crate::config::is_full_oid(value) {
            Oid::from_str(value).ok()
        } else {
            None
        }
    };
    let change = match fields.get(f::CHANGE_TRAILER) {
        None => None,
        Some(value) => Some(oid(value)?),
    };
    let unrecorded = *fields.get(f::UNRECORDED_TRAILER)?;
    if unrecorded.is_empty() || !unrecorded.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(FinishRecord {
        fork: name(f::FORK_TRAILER)?,
        source: name(f::SOURCE_TRAILER)?,
        base: oid(fields.get(f::BASE_TRAILER)?)?,
        finish,
        change,
        // A remote section's name, or the source's own history (F8 S3).
        remote: match fields.get(f::FORK_REMOTE_TRAILER) {
            Some(&value) if value == crate::fork::receive::SOURCE_HISTORY => value.to_owned(),
            _ => name(f::FORK_REMOTE_TRAILER)?,
        },
        unrecorded: unrecorded.parse().ok()?,
    })
}

/// The git signature of a patch author, if it makes one (a name or email
/// libgit2 refuses falls back to the default author).
fn patch_author(author: Option<&PatchAuthor>) -> Option<Signature<'static>> {
    let author = author?;
    Signature::new(
        &author.name,
        &author.email,
        &Time::new(author.time_s, author.offset_min),
    )
    .ok()
}

fn push_context_trailers(out: &mut String, context: &Context) {
    for (key, value) in [
        ("Agentfs-Namespace", &context.namespace),
        ("Agentfs-Pod", &context.pod),
        ("Agentfs-Pod-Uid", &context.pod_uid),
        ("Agentfs-Node", &context.node),
    ] {
        if !value.is_empty() {
            out.push_str(&format!("{key}: {value}\n"));
        }
    }
}

fn root_attributes_path() -> RepoPath {
    RepoPath::new(attributes::ROOT_GITATTRIBUTES).expect(".gitattributes is a valid path")
}

/// Removes `path` and everything beneath it.
fn remove_all(index: &mut Index, path: &RepoPath) -> Result<(), git2::Error> {
    let p = Path::new(path.as_str());
    if index.get_path(p, 0).is_some() {
        index.remove_path(p)?;
    }
    index.remove_dir(p, 0)
}

/// Clears anything that would conflict with a file at `path`: a directory
/// at `path`, or a file at any ancestor.
fn make_room(index: &mut Index, path: &RepoPath) -> Result<(), git2::Error> {
    index.remove_dir(Path::new(path.as_str()), 0)?;
    let mut prefix = String::new();
    let components: Vec<&str> = path.components().collect();
    for component in &components[..components.len() - 1] {
        if !prefix.is_empty() {
            prefix.push('/');
        }
        prefix.push_str(component);
        let p = Path::new(&prefix);
        if index.get_path(p, 0).is_some() {
            index.remove_path(p)?;
        }
    }
    Ok(())
}

fn entry(path: &RepoPath, id: Oid, mode: u32) -> IndexEntry {
    IndexEntry {
        ctime: IndexTime::new(0, 0),
        mtime: IndexTime::new(0, 0),
        dev: 0,
        ino: 0,
        mode,
        uid: 0,
        gid: 0,
        file_size: 0,
        id,
        flags: 0,
        flags_extended: 0,
        path: path.as_str().as_bytes().to_vec(),
    }
}

fn time(ms: u64) -> Time {
    Time::new((ms / 1000) as i64, 0)
}

fn message(records: &[Record], context: &Context) -> String {
    let changes: Vec<(&Record, &Change)> = records
        .iter()
        .flat_map(|r| r.draft.changes.iter().map(move |c| (r, c)))
        .collect();
    // The committer never groups an operation's records with others.
    let operation = records.first().and_then(|r| r.draft.operation.as_ref());
    let summary = match (operation, changes.as_slice()) {
        (Some(op), _) => op.summary.clone(),
        (None, [(r, Change::Move { from, to })]) => {
            format!("{}: {from} -> {to}", r.draft.trigger.as_str())
        }
        (None, [(r, c)]) => format!("{}: {}", r.draft.trigger.as_str(), c.path()),
        (None, _) => format!("{} changes", changes.len()),
    };
    let mut out = summary;
    out.push_str("\n\n");
    if changes.len() > 1 {
        for (_, change) in &changes {
            let line = match change {
                Change::Write { path, .. } => format!("write   {path}\n"),
                Change::Symlink { path, target } => format!("symlink {path} -> {target}\n"),
                Change::Remove { path } => format!("remove  {path}\n"),
                Change::Move { from, to } => format!("move    {from} -> {to}\n"),
            };
            out.push_str(&line);
        }
        out.push('\n');
    }

    let last = records.last().expect("non-empty");
    out.push_str(&format!("{SEQ_TRAILER}: {}\n", last.seq));
    let triggers: BTreeSet<&str> = records.iter().map(|r| r.draft.trigger.as_str()).collect();
    for trigger in triggers {
        out.push_str(&format!("Agentfs-Trigger: {trigger}\n"));
    }
    if let Some(op) = operation {
        out.push_str(&format!("{}: {}\n", op.kind.trailer(), op.source));
        if let Some(n) = op.inbox {
            out.push_str(&format!("{INBOX_TRAILER}: {n}\n"));
        }
    }
    push_context_trailers(&mut out, context);
    let uids: BTreeSet<u32> = records.iter().map(|r| r.draft.caller.uid).collect();
    for uid in uids {
        out.push_str(&format!("Agentfs-Uid: {uid}\n"));
    }
    // Pid 0 is unknown (a caller outside the pid namespace): not named.
    let pids: BTreeSet<u32> = records
        .iter()
        .map(|r| r.draft.caller.pid)
        .filter(|&pid| pid != 0)
        .collect();
    for pid in pids {
        out.push_str(&format!("Agentfs-Pid: {pid}\n"));
    }
    out
}

/// Reads the `Agentfs-Seq` trailer from a commit message.
/// [`History::set_includes`] on the repository at `git_dir`, without
/// loading its settings first, so a missing or bad include can be
/// replaced.
pub fn write_includes(git_dir: &Path, includes: &[PathBuf]) -> Result<(), HistoryError> {
    init_libgit2();
    let mut config = git2::Config::open(&git_dir.join("config"))?;
    match config.remove_multivar("include.path", ".*") {
        Ok(()) => {}
        Err(e) if e.code() == git2::ErrorCode::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    for path in includes {
        let Some(value) = path.to_str() else {
            return Err(git2::Error::from_str(&format!(
                "include path {} is not UTF-8",
                path.display()
            ))
            .into());
        };
        // A pattern no existing value matches appends a new entry.
        config.set_multivar("include.path", "^$", value)?;
    }
    Ok(())
}

pub fn seq_from_message(message: &str) -> Option<u64> {
    let prefix = format!("{SEQ_TRAILER}: ");
    message
        .lines()
        .rev()
        .find_map(|line| line.strip_prefix(&prefix))
        .and_then(|n| n.trim().parse().ok())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::Duration;

    use super::*;
    use crate::identity::Templates;
    use crate::lfs::Pointer;
    use crate::record::{Caller, Draft, Trigger};

    #[test]
    fn libgit2_home_directory_is_cleared() {
        init_libgit2();
        let mut buf = libgit2_sys::git_buf {
            ptr: std::ptr::null_mut(),
            reserved: 0,
            size: 0,
        };
        // SAFETY: GIT_OPT_GET_HOMEDIR fills a git_buf, disposed below.
        let rc = unsafe {
            libgit2_sys::git_libgit2_opts(
                libgit2_sys::GIT_OPT_GET_HOMEDIR as std::ffi::c_int,
                &mut buf as *mut libgit2_sys::git_buf,
            )
        };
        let size = buf.size;
        // SAFETY: the buffer came from libgit2.
        unsafe { libgit2_sys::git_buf_dispose(&mut buf) };
        assert_eq!(rc, 0);
        assert_eq!(size, 0, "libgit2 still has a home directory");
    }

    #[test]
    fn libgit2_server_timeouts_are_set() {
        init_libgit2();
        // SAFETY: reads of integer options.
        unsafe {
            assert_eq!(
                git2::opts::get_server_connect_timeout_in_milliseconds().unwrap(),
                SERVER_CONNECT_TIMEOUT_MS
            );
            assert_eq!(
                git2::opts::get_server_timeout_in_milliseconds().unwrap(),
                SERVER_IO_TIMEOUT_MS
            );
        }
    }

    struct Fixture {
        dir: tempfile::TempDir,
        history: History,
        pending: PendingStore,
        lfs: ObjectCache,
        checks: Checks,
        identities: IdentityRenderer,
        context: Context,
        seq: u64,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let history = History::init(&dir.path().join("git"), "vol", None).unwrap();
            let pending = PendingStore::open(dir.path().join("pending")).unwrap();
            let lfs = ObjectCache::open(dir.path().join("lfs")).unwrap();
            let context = Context {
                volume: "vol".into(),
                namespace: "agent".into(),
                pod: "pod-a".into(),
                pod_uid: "uid-a".into(),
                node: "n1".into(),
                replica: "pod-a".into(),
            };
            let identities = IdentityRenderer::new(Templates::default(), &context).unwrap();
            Self {
                dir,
                history,
                pending,
                lfs,
                checks: Checks::default(),
                identities,
                context,
                seq: 0,
            }
        }

        fn write(&self, path: &str, content: &[u8], executable: bool) -> Change {
            self.write_to(path, content, executable, Store::Blob)
        }

        fn write_lfs(&self, path: &str, content: &[u8]) -> Change {
            self.write_to(path, content, false, Store::Lfs)
        }

        fn write_to(&self, path: &str, content: &[u8], executable: bool, store: Store) -> Change {
            let src = self.dir.path().join("src");
            fs::write(&src, content).unwrap();
            let (id, _) = self.pending.freeze(&src).unwrap();
            Change::Write {
                path: RepoPath::new(path).unwrap(),
                pending: id,
                executable,
                store,
            }
        }

        fn record(&mut self, trigger: Trigger, changes: Vec<Change>) -> Record {
            let r = Record {
                seq: self.seq,
                draft: Draft {
                    trigger,
                    caller: Caller {
                        uid: 10000,
                        gid: 10000,
                        pid: 42,
                    },
                    time_ms: 1_700_000_000_000,
                    changes,
                    operation: None,
                    atomic: false,
                    origin: None,
                },
            };
            self.seq += 1;
            r
        }

        fn commit(&self, records: &[Record]) -> Committed {
            self.history
                .commit(
                    records,
                    &self.pending,
                    &self.lfs,
                    &self.checks,
                    &self.identities,
                    &self.context,
                )
                .unwrap()
        }

        fn read(&self, path: &str) -> Option<(Vec<u8>, i32)> {
            let head = self.history.head().unwrap()?;
            let entry = head.tree().unwrap().get_path(Path::new(path)).ok()?;
            let blob = self.history.repo().find_blob(entry.id()).ok()?;
            Some((blob.content().to_vec(), entry.filemode()))
        }
    }

    fn remove(path: &str) -> Change {
        Change::Remove {
            path: RepoPath::new(path).unwrap(),
        }
    }

    /// Objects are made durable before the ref moves, and the ref's
    /// directories after (design §3.3): the failpoints mark the steps.
    #[cfg(feature = "failpoints")]
    #[test]
    fn objects_are_synced_before_the_ref_moves() {
        let mut f = Fixture::new();
        let change = f.write("a", b"one", false);
        let r = f.record(Trigger::Close, vec![change]);
        f.commit(&[r]);
        let before = f.history.head().unwrap().unwrap().id();

        let guard = crate::failpoints::configure("").unwrap();
        let change = f.write("a", b"two", false);
        let r = f.record(Trigger::Close, vec![change]);
        f.commit(&[r]);
        assert_eq!(
            crate::failpoints::trace(),
            [
                "pending.after_freeze",
                "commit.after_tree",
                "commit.sync_objects",
                "commit.after_object",
                "commit.after_ref",
                "commit.after_sync"
            ]
        );
        drop(guard);

        // A failed object sync leaves the branch where it was.
        let head = f.history.head().unwrap().unwrap().id();
        assert_ne!(head, before);
        let _guard = crate::failpoints::configure("commit.sync_objects=error").unwrap();
        let change = f.write("a", b"three", false);
        let r = f.record(Trigger::Close, vec![change]);
        let err = f
            .history
            .commit(
                &[r],
                &f.pending,
                &f.lfs,
                &f.checks,
                &f.identities,
                &f.context,
            )
            .unwrap_err();
        assert!(
            err.to_string().contains("failpoint commit.sync_objects"),
            "{err}"
        );
        assert_eq!(f.history.head().unwrap().unwrap().id(), head);
        assert_eq!(f.read("a").unwrap().0, b"two");
    }

    #[test]
    fn first_commit_creates_the_branch_with_trailers() {
        let mut f = Fixture::new();
        let change = f.write("dir/a.txt", b"hello", false);
        let r = f.record(Trigger::Close, vec![change]);
        let c = f.commit(&[r]);

        assert!(c.commit.is_some());
        assert_eq!(f.read("dir/a.txt"), Some((b"hello".to_vec(), 0o100644)));
        assert_eq!(f.history.head_seq().unwrap(), Some(0));
        let head = f.history.head().unwrap().unwrap();
        let msg = head.message().unwrap();
        assert!(msg.starts_with("close: dir/a.txt\n"), "{msg}");
        assert!(msg.contains("Agentfs-Pod: pod-a\n"));
        assert!(msg.contains("Agentfs-Uid: 10000\n"));
        assert_eq!(head.committer().name().unwrap(), "pod-a");
        assert_eq!(head.author().name().unwrap(), "uid-10000");
        assert_eq!(
            f.history
                .repo()
                .find_reference("refs/heads/agentfs/vol")
                .unwrap()
                .target(),
            Some(head.id())
        );
    }

    #[test]
    fn executable_symlink_and_remove() {
        let mut f = Fixture::new();
        let w = f.write("bin/run", b"#!/bin/sh\n", true);
        let r1 = f.record(Trigger::Close, vec![w]);
        let r2 = f.record(
            Trigger::Symlink,
            vec![Change::Symlink {
                path: RepoPath::new("latest").unwrap(),
                target: "bin/run".into(),
            }],
        );
        f.commit(&[r1, r2]);
        assert_eq!(f.read("bin/run").unwrap().1, 0o100755);
        assert_eq!(f.read("latest"), Some((b"bin/run".to_vec(), 0o120000)));

        let r3 = f.record(Trigger::Rmdir, vec![remove("bin")]);
        f.commit(&[r3]);
        assert_eq!(f.read("bin/run"), None);
        assert!(f.read("latest").is_some());
    }

    #[test]
    fn rename_is_remove_plus_write_in_one_record() {
        let mut f = Fixture::new();
        let w = f.write("a", b"x", false);
        let r1 = f.record(Trigger::Close, vec![w]);
        f.commit(&[r1]);

        let w = f.write("b", b"x", false);
        let r2 = f.record(Trigger::Rename, vec![remove("a"), w]);
        f.commit(&[r2]);
        assert_eq!(f.read("a"), None);
        assert_eq!(f.read("b").unwrap().0, b"x");
    }

    #[test]
    fn file_replaced_by_directory_and_back() {
        let mut f = Fixture::new();
        let w = f.write("x", b"file", false);
        let r = f.record(Trigger::Close, vec![w]);
        f.commit(&[r]);

        let w = f.write("x/inner", b"nested", false);
        let r = f.record(Trigger::Close, vec![w]);
        f.commit(&[r]);
        assert_eq!(f.read("x/inner").unwrap().0, b"nested");

        let w = f.write("x", b"file again", false);
        let r = f.record(Trigger::Close, vec![w]);
        f.commit(&[r]);
        assert_eq!(f.read("x").unwrap().0, b"file again");
        assert_eq!(f.read("x/inner"), None);
    }

    #[test]
    fn unchanged_tree_makes_no_commit_and_replay_is_idempotent() {
        let mut f = Fixture::new();
        let w = f.write("a", b"same", false);
        let r = f.record(Trigger::Close, vec![w]);
        let first = f.commit(std::slice::from_ref(&r));

        // Replaying the same record (as after a crash) changes nothing.
        let again = f.commit(std::slice::from_ref(&r));
        assert_eq!(
            again,
            Committed {
                commit: None,
                last_seq: r.seq,
                refused: Vec::new(),
                lfs_as_blobs: 0,
            }
        );

        let w = f.write("a", b"same", false);
        let r2 = f.record(Trigger::Close, vec![w]);
        assert_eq!(f.commit(&[r2]).commit, None);
        assert_eq!(
            f.history.head().unwrap().unwrap().id(),
            first.commit.unwrap()
        );
    }

    #[test]
    fn grouped_records_list_their_changes() {
        let mut f = Fixture::new();
        let a = f.write("a", b"1", false);
        let b = f.write("b", b"2", false);
        let r1 = f.record(Trigger::Close, vec![a]);
        let r2 = f.record(Trigger::Close, vec![b]);
        f.commit(&[r1, r2]);
        let head = f.history.head().unwrap().unwrap();
        let msg = head.message().unwrap();
        assert!(
            msg.starts_with("2 changes\n\nwrite   a\nwrite   b\n"),
            "{msg}"
        );
        assert_eq!(seq_from_message(msg), Some(1));
    }

    #[test]
    fn init_libgit2_clears_config_search_paths() {
        let _f = Fixture::new();
        for level in [
            git2::ConfigLevel::System,
            git2::ConfigLevel::XDG,
            git2::ConfigLevel::Global,
            git2::ConfigLevel::ProgramData,
        ] {
            // SAFETY: reading the search path set once by init_libgit2.
            let path = unsafe { git2::opts::get_search_path(level) }.unwrap();
            assert!(path.as_bytes().is_empty(), "{level:?}: {path:?}");
        }
    }

    fn managed(paths: &[&str]) -> BTreeSet<RepoPath> {
        paths.iter().map(|p| RepoPath::new(*p).unwrap()).collect()
    }

    fn attributes_with(user: &[u8], paths: &[&str]) -> Vec<u8> {
        attributes::compose(user, &managed(paths))
    }

    #[test]
    fn lfs_writes_become_pointers_with_a_managed_attributes_line() {
        let mut f = Fixture::new();
        let content = b"large content".repeat(100);
        let big = f.write_lfs("big.bin", &content);
        let small = f.write("small.txt", b"small", false);
        let r = f.record(Trigger::Close, vec![big, small]);
        f.commit(&[r]);

        let (pointer, mode) = f.read("big.bin").unwrap();
        assert_eq!(mode, 0o100644);
        let pointer = Pointer::parse(&pointer).expect("a canonical pointer");
        assert_eq!(pointer.oid, crate::lfs::LfsOid::of(&content));
        assert_eq!(pointer.size, content.len() as u64);
        assert_eq!(fs::read(f.lfs.path(&pointer.oid)).unwrap(), content);
        assert_eq!(f.read("small.txt").unwrap().0, b"small");
        assert_eq!(
            f.read(".gitattributes").unwrap(),
            (attributes_with(b"", &["big.bin"]), 0o100644)
        );
        // The executable bit stays on the pointer.
        let exec = f.write_to("tool", &content, true, Store::Lfs);
        let r = f.record(Trigger::Close, vec![exec]);
        f.commit(&[r]);
        assert_eq!(f.read("tool").unwrap().1, 0o100755);
        assert_eq!(
            f.read(".gitattributes").unwrap().0,
            attributes_with(b"", &["big.bin", "tool"])
        );
    }

    #[test]
    fn the_workloads_gitattributes_is_kept_and_forged_blocks_replaced() {
        let mut f = Fixture::new();
        let forged = format!(
            "*.txt text\n{}\n/plain.bin filter=lfs diff=lfs merge=lfs -text\n{}\n",
            attributes::BEGIN,
            attributes::END
        );
        let user = f.write(".gitattributes", forged.as_bytes(), false);
        let plain = f.write("plain.bin", b"not a pointer", false);
        let r = f.record(Trigger::Close, vec![user, plain]);
        f.commit(&[r]);
        // No LFS path: the forged block goes, the user's line stays.
        assert_eq!(f.read(".gitattributes").unwrap().0, b"*.txt text\n");

        let lfs = f.write_lfs("a.bin", b"aaaa");
        let r = f.record(Trigger::Close, vec![lfs]);
        f.commit(&[r]);
        assert_eq!(
            f.read(".gitattributes").unwrap().0,
            attributes_with(b"*.txt text\n", &["a.bin"])
        );
        // The workload rewrites .gitattributes, with an unterminated BEGIN
        // line: its content is replaced, that line kept as its own, and the
        // managed block is not replaced.
        let rewritten = format!("*.md text\n{}\n/plain.bin x\n", attributes::BEGIN);
        let user = f.write(".gitattributes", rewritten.as_bytes(), false);
        let r = f.record(Trigger::Close, vec![user]);
        f.commit(&[r]);
        assert_eq!(
            f.read(".gitattributes").unwrap().0,
            attributes_with(rewritten.as_bytes(), &["a.bin"])
        );
        // Removing .gitattributes keeps the block alone.
        let r = f.record(Trigger::Unlink, vec![remove(".gitattributes")]);
        f.commit(&[r]);
        assert_eq!(
            f.read(".gitattributes").unwrap().0,
            attributes_with(b"", &["a.bin"])
        );
    }

    fn limited(bytes: u64) -> Checks {
        Checks::new(crate::checks::Limits {
            max_blob_size: Some(crate::checks::BlobLimit {
                bytes,
                remote: "r".into(),
            }),
            ..Default::default()
        })
    }

    fn is_pointer(f: &Fixture, path: &str) -> bool {
        Pointer::parse(&f.read(path).unwrap().0).is_some()
    }

    #[test]
    fn lfs_writes_are_blobs_while_the_root_gitattributes_is_a_directory() {
        let mut f = Fixture::new();
        // History from before the gitattributesBlob check holds a
        // directory named .gitattributes.
        let dir = f.write(".gitattributes/x", b"hi\n", false);
        let r = f.record(Trigger::Close, vec![dir]);
        f.commit(&[r]);
        let big = f.write_lfs("big.bin", b"0123456789abcdef");
        let r = f.record(Trigger::Close, vec![big]);
        let done = f.commit(&[r]);
        assert_eq!((done.lfs_as_blobs, done.refused.len()), (1, 0));
        // No pointer without its managed line: the content is a blob.
        assert_eq!(f.read("big.bin").unwrap().0, b"0123456789abcdef");
        assert_eq!(f.read(".gitattributes/x").unwrap().0, b"hi\n");
        // Over the blob limit, it is refused instead.
        f.checks = limited(8);
        let huge = f.write_lfs("huge.bin", b"0123456789abcdef!");
        let small = f.write("small.txt", b"ok", false);
        let r = f.record(Trigger::Close, vec![huge, small]);
        let done = f.commit(&[r]);
        assert!(done.commit.is_some());
        assert_eq!(done.lfs_as_blobs, 0);
        assert!(
            matches!(&done.refused[..], [(p, Refusal::BlobTooLarge { bytes: 17, .. })] if p.as_str() == "huge.bin"),
            "{:?}",
            done.refused
        );
        assert_eq!(f.read("huge.bin"), None);
        assert_eq!(f.read("small.txt").unwrap().0, b"ok");
        // Once the directory is gone, LFS writes are pointers again, with
        // their lines.
        f.checks = Checks::default();
        let r = f.record(Trigger::Unlink, vec![remove(".gitattributes")]);
        f.commit(&[r]);
        let again = f.write_lfs("big.bin", b"0123456789abcdef");
        let r = f.record(Trigger::Close, vec![again]);
        let done = f.commit(&[r]);
        assert_eq!(done.lfs_as_blobs, 0);
        assert!(is_pointer(&f, "big.bin"));
        assert_eq!(
            f.read(".gitattributes").unwrap().0,
            attributes_with(b"", &["big.bin"])
        );
    }

    #[test]
    fn a_too_large_composed_gitattributes_never_blocks_commits() {
        let mut f = Fixture::new();
        let block = attributes_with(b"", &["big.bin"]).len() as u64;
        // The workload's .gitattributes fits the limit, but not with a
        // block: the LFS write becomes a blob and the commit goes ahead.
        f.checks = limited(block + 10);
        let user = vec![b'#'; 40]
            .into_iter()
            .chain(*b"\n")
            .collect::<Vec<u8>>();
        let attrs = f.write(".gitattributes", &user, false);
        let big = f.write_lfs("big.bin", b"0123456789abcdef");
        let r = f.record(Trigger::Close, vec![attrs, big]);
        let done = f.commit(&[r]);
        assert!(done.commit.is_some());
        assert_eq!((done.lfs_as_blobs, done.refused.len()), (1, 0));
        assert_eq!(f.read(".gitattributes").unwrap().0, user);
        assert_eq!(f.read("big.bin").unwrap().0, b"0123456789abcdef");

        // With a managed path in history, a .gitattributes that leaves no
        // room for its block is dropped (refused at commit time); the
        // rest of the commit, LFS writes as blobs, goes ahead.
        let mut f = Fixture::new();
        let big = f.write_lfs("big.bin", b"0123456789abcdef");
        let r = f.record(Trigger::Close, vec![big]);
        f.commit(&[r]);
        let head_attrs = f.read(".gitattributes").unwrap().0;
        f.checks = limited(block + 10);
        let attrs = f.write(".gitattributes", &user, false);
        let other = f.write_lfs("other.bin", b"fedcba9876543210");
        let small = f.write("small.txt", b"ok", false);
        let r = f.record(Trigger::Close, vec![attrs, other, small]);
        let done = f.commit(&[r]);
        assert!(done.commit.is_some());
        assert_eq!(done.lfs_as_blobs, 1);
        assert!(
            matches!(&done.refused[..], [(p, Refusal::BlobTooLarge { .. })] if p.as_str() == ".gitattributes"),
            "{:?}",
            done.refused
        );
        assert_eq!(f.read(".gitattributes").unwrap().0, head_attrs);
        assert_eq!(f.read("other.bin").unwrap().0, b"fedcba9876543210");
        assert_eq!(f.read("small.txt").unwrap().0, b"ok");
        assert!(is_pointer(&f, "big.bin"));

        // The limit lowered below the head's own .gitattributes: it is
        // kept as it is, and commits still go ahead.
        f.checks = limited(16);
        let more = f.write("more.txt", b"ok", false);
        let r = f.record(Trigger::Close, vec![more]);
        let done = f.commit(&[r]);
        assert!(done.commit.is_some());
        assert!(done.refused.is_empty());
        assert_eq!(f.read(".gitattributes").unwrap().0, head_attrs);
        assert_eq!(f.read("more.txt").unwrap().0, b"ok");
        assert!(is_pointer(&f, "big.bin"));
    }

    #[test]
    fn managed_lines_go_when_their_paths_stop_being_lfs() {
        let mut f = Fixture::new();
        let changes = vec![
            f.write_lfs("rm.bin", b"1111"),
            f.write_lfs("shrunk.bin", b"2222"),
            f.write_lfs("dir/a.bin", b"3333"),
            f.write_lfs("dir/b.bin", b"4444"),
            f.write_lfs("linked.bin", b"5555"),
            f.write_lfs("stays.bin", b"6666"),
        ];
        let r = f.record(Trigger::Close, changes);
        f.commit(&[r]);
        assert_eq!(
            f.read(".gitattributes").unwrap().0,
            attributes_with(
                b"",
                &[
                    "dir/a.bin",
                    "dir/b.bin",
                    "linked.bin",
                    "rm.bin",
                    "shrunk.bin",
                    "stays.bin"
                ]
            )
        );
        let shrunk = f.write("shrunk.bin", b"2", false);
        let r = f.record(
            Trigger::Close,
            vec![
                remove("rm.bin"),
                shrunk,
                remove("dir"),
                Change::Symlink {
                    path: RepoPath::new("linked.bin").unwrap(),
                    target: "stays.bin".into(),
                },
            ],
        );
        f.commit(&[r]);
        assert_eq!(
            f.read(".gitattributes").unwrap().0,
            attributes_with(b"", &["stays.bin"])
        );
        assert_eq!(f.read("shrunk.bin").unwrap().0, b"2");
        // The last one goes: so does the file agentfs created.
        let r = f.record(Trigger::Unlink, vec![remove("stays.bin")]);
        f.commit(&[r]);
        assert_eq!(f.read(".gitattributes"), None);
    }

    fn mv(from: &str, to: &str) -> Change {
        Change::Move {
            from: RepoPath::new(from).unwrap(),
            to: RepoPath::new(to).unwrap(),
        }
    }

    #[test]
    fn a_move_leaves_out_what_the_path_rules_refuse_at_its_destination() {
        let mut f = Fixture::new();
        // Deep enough that a longer prefix takes it past MAX_PATH_BYTES.
        let deep = format!("{}/f", vec!["x".repeat(200); 20].join("/"));
        let changes = vec![
            f.write("d/f", b"1", false),
            f.write("d/sub/g", b"2", false),
            f.write("a/h", b"3", false),
            f.write_lfs("a/big.bin", b"4444"),
            f.write(&format!("l/{deep}"), b"5", false),
        ];
        let r = f.record(Trigger::Close, changes);
        f.commit(&[r]);
        let r = f.record(
            Trigger::Rename,
            vec![
                mv("d", ".gitmodules"),
                mv("a", "x/.gitattributes"),
                mv("l", &"y".repeat(200)),
            ],
        );
        f.commit(&[r]);
        for gone in [".gitmodules/f", ".gitmodules/sub/g", "x/.gitattributes/h"] {
            assert_eq!(f.read(gone), None, "{gone}");
        }
        assert_eq!(f.read(&format!("{}/{deep}", "y".repeat(200))), None);
        // Nothing of them is left anywhere, and no managed line follows the
        // LFS file that was left out.
        let head = f.history.head().unwrap().unwrap();
        let mut paths = Vec::new();
        head.tree()
            .unwrap()
            .walk(git2::TreeWalkMode::PreOrder, |dir, e| {
                if e.kind() == Some(git2::ObjectType::Blob) {
                    paths.push(format!("{dir}{}", e.name().unwrap()));
                }
                git2::TreeWalkResult::Ok
            })
            .unwrap();
        assert!(paths.is_empty(), "{paths:?}");
    }

    #[test]
    fn a_move_carries_the_tree_and_its_managed_lines() {
        let mut f = Fixture::new();
        let changes = vec![
            f.write_lfs("d/big.bin", b"1111"),
            f.write("d/e/small", b"s", true),
            f.write_lfs("x/old.bin", b"2222"),
            f.write("dx", b"sibling", false),
        ];
        let r = f.record(Trigger::Close, changes);
        f.commit(&[r]);
        let big = f.read("d/big.bin").unwrap();
        let r = f.record(Trigger::Rename, vec![mv("d", "x")]);
        let c = f.commit(std::slice::from_ref(&r));
        assert!(c.commit.is_some());
        assert_eq!(f.read("x/big.bin").unwrap(), big);
        assert_eq!(f.read("x/e/small").unwrap(), (b"s".to_vec(), 0o100755));
        assert_eq!(f.read("x/old.bin"), None, "the destination is replaced");
        assert_eq!(f.read("d/big.bin"), None);
        assert_eq!(f.read("dx").unwrap().0, b"sibling");
        assert_eq!(
            f.read(".gitattributes").unwrap().0,
            attributes_with(b"", &["x/big.bin"])
        );
        let msg = f
            .history
            .head()
            .unwrap()
            .unwrap()
            .message()
            .unwrap()
            .to_owned();
        assert!(msg.starts_with("rename: d -> x\n"), "{msg}");
        // Replayed after a crash: nothing changes.
        assert_eq!(f.commit(&[r]).commit, None);
        // Moved and changed in one commit; a move of nothing clears the
        // destination; a move into a new parent replaces a file there.
        let w = f.write("y/e/small", b"t", false);
        let r1 = f.record(Trigger::Rename, vec![mv("x", "y")]);
        let r2 = f.record(Trigger::Rename, vec![w, mv("none", "dx")]);
        let r3 = f.record(Trigger::Rename, vec![mv("y/e", "z/e")]);
        f.commit(&[r1, r2, r3]);
        assert_eq!(f.read("z/e/small").unwrap().0, b"t");
        assert_eq!(f.read("dx"), None);
        assert!(f.read("y/big.bin").is_some());
        assert_eq!(
            f.read(".gitattributes").unwrap().0,
            attributes_with(b"", &["y/big.bin"])
        );
    }

    #[test]
    fn lfs_commits_replay_idempotently() {
        let mut f = Fixture::new();
        let w = f.write_lfs("big", b"0123456789");
        let r = f.record(Trigger::Close, vec![w]);
        let first = f.commit(std::slice::from_ref(&r));
        assert!(first.commit.is_some());
        // As after a crash between the commit and the journal truncation.
        assert_eq!(f.commit(std::slice::from_ref(&r)).commit, None);
        // And between the cache insert and the commit: the object is reused.
        let w = f.write_lfs("again", b"0123456789");
        let r = f.record(Trigger::Close, vec![w]);
        f.commit(&[r]);
        assert_eq!(f.read("again").unwrap().0, f.read("big").unwrap().0);
    }
    #[test]
    fn settings_come_from_init() {
        let dir = tempfile::tempdir().unwrap();
        let h = History::init(&dir.path().join("git"), "v1", Some("custom/branch")).unwrap();
        assert_eq!(h.settings().branch_ref, "refs/heads/custom/branch");
        assert!(dir.path().join("git/info/attributes").exists());
    }

    #[test]
    fn set_includes_replaces_the_entries_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let git = dir.path().join("git");
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        std::fs::write(&a, "[agentfs]\n\tpushInterval = 7s\n").unwrap();
        std::fs::write(&b, "[remote \"origin\"]\n\turl = /srv/r.git\n").unwrap();
        let h = History::init(&git, "v", None).unwrap();
        let own = || std::fs::read_to_string(git.join("config")).unwrap();
        h.set_includes(&[a.clone(), b.clone()]).unwrap();
        let h = History::open(&git).unwrap();
        assert_eq!(h.settings().includes, [a.clone(), b.clone()]);
        assert_eq!(h.settings().push_interval, Duration::from_secs(7));
        assert_eq!(h.settings().remotes.pushed()[0].url, "/srv/r.git");
        let once = own();
        h.set_includes(&[a.clone(), b.clone()]).unwrap();
        assert_eq!(own(), once);
        // Replaced, not added to.
        h.set_includes(std::slice::from_ref(&b)).unwrap();
        let h = History::open(&git).unwrap();
        assert_eq!(h.settings().includes, std::slice::from_ref(&b));
        assert_eq!(h.settings().push_interval, Duration::from_secs(60));
        assert_eq!(own().matches("path = ").count(), 1, "{}", own());
        // And removed.
        h.set_includes(&[]).unwrap();
        let h = History::open(&git).unwrap();
        assert!(h.settings().includes.is_empty());
        assert!(h.settings().remotes.pushed().is_empty());
        assert_eq!(h.settings().volume, "v");
    }
}
