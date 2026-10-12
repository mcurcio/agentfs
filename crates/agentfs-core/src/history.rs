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
use std::path::Path;
use std::sync::Once;

use git2::{Index, IndexEntry, IndexTime, Oid, Repository, Signature, Time};

use crate::config::{ConfigError, Settings};
use crate::identity::{Context, IdentityError, IdentityRenderer};
use crate::paths::RepoPath;
use crate::pending::PendingStore;
use crate::record::{Change, Record};

pub const SEQ_TRAILER: &str = "Agentfs-Seq";

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
}

/// Process-wide libgit2 setup, done once before any repository is opened:
///
/// - objects and refs are fsynced (libgit2 does not by default; design
///   §3.3);
/// - the system, XDG, global and ProgramData config search paths are
///   cleared, so no config outside the history repository's own file can
///   reach a volume: not `url.<base>.insteadOf` redirecting a push, not
///   `http.*`, not `core.excludesFile` (design §4.4, §11.1);
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
    pub fn commit(
        &self,
        records: &[Record],
        pending: &PendingStore,
        identities: &IdentityRenderer,
        context: &Context,
    ) -> Result<Committed, HistoryError> {
        let last = records.last().expect("commit needs at least one record");
        let head = self.head()?;
        let mut index = Index::new()?;
        if let Some(head) = &head {
            index.read_tree(&head.tree()?)?;
        }
        for record in records {
            for change in &record.draft.changes {
                self.apply(&mut index, change, pending)?;
            }
        }
        let tree_id = index.write_tree_to(&self.repo)?;
        if head.as_ref().is_some_and(|h| h.tree_id() == tree_id) {
            return Ok(Committed {
                commit: None,
                last_seq: last.seq,
            });
        }
        let tree = self.repo.find_tree(tree_id)?;

        let first = &records[0];
        let author = identities.author(first.draft.caller)?;
        let author = Signature::new(&author.name, &author.email, &time(last.draft.time_ms))?;
        let committer = identities.committer();
        let committer = Signature::now(&committer.name, &committer.email)?;
        let message = message(records, context);
        let parents: Vec<&git2::Commit<'_>> = head.iter().collect();
        let commit = self
            .repo
            .commit(None, &author, &committer, &message, &tree, &parents)?;

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
        self.sync_dirs()?;
        Ok(Committed {
            commit: Some(commit),
            last_seq: last.seq,
        })
    }

    fn apply(
        &self,
        index: &mut Index,
        change: &Change,
        pending: &PendingStore,
    ) -> Result<(), HistoryError> {
        match change {
            Change::Remove { path } => remove_all(index, path)?,
            Change::Write {
                path,
                pending: id,
                executable,
            } => {
                let blob = self.repo.blob_path(&pending.path(*id))?;
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
        }
        Ok(())
    }

    /// libgit2 fsyncs object files and refs once enabled, but not newly
    /// created directories; fsync the ones a commit can create.
    fn sync_dirs(&self) -> io::Result<()> {
        let git_dir = self.repo.path();
        File::open(git_dir.join("objects"))?.sync_all()?;
        let mut dir = git_dir.join(&self.settings.branch_ref);
        while dir.pop() && dir.starts_with(git_dir) {
            File::open(&dir)?.sync_all()?;
        }
        Ok(())
    }
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
    }
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

    use super::*;
    use crate::identity::Templates;
    use crate::record::{Caller, Draft, Trigger};

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
        identities: IdentityRenderer,
        context: Context,
        seq: u64,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let history = History::init(&dir.path().join("git"), "vol", None).unwrap();
            let pending = PendingStore::open(dir.path().join("pending")).unwrap();
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
                identities,
                context,
                seq: 0,
            }
        }

        fn write(&self, path: &str, content: &[u8], executable: bool) -> Change {
            let src = self.dir.path().join("src");
            fs::write(&src, content).unwrap();
            let (id, _) = self.pending.freeze(&src).unwrap();
            Change::Write {
                path: RepoPath::new(path).unwrap(),
                pending: id,
                executable,
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
                },
            };
            self.seq += 1;
            r
        }

        fn commit(&self, records: &[Record]) -> Committed {
            self.history
                .commit(records, &self.pending, &self.identities, &self.context)
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
                last_seq: r.seq
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

    #[test]
    fn settings_come_from_init() {
        let dir = tempfile::tempdir().unwrap();
        let h = History::init(&dir.path().join("git"), "v1", Some("custom/branch")).unwrap();
        assert_eq!(h.settings().branch_ref, "refs/heads/custom/branch");
        assert!(dir.path().join("git/info/attributes").exists());
    }
}
