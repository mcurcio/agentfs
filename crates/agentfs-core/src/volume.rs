//! A volume's state area, opened as a [`Recorder`] (used by the FUSE layer
//! on every change boundary) and a [`Committer`] (which turns records into
//! history in the background).
//!
//! Opening a volume is also crash recovery (design §3.3): records already in
//! history are dropped from the journal, the rest stay uncommitted, and
//! frozen copies nothing refers to are removed.

use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::history::{Committed, History, HistoryError};
use crate::identity::{Context, IdentityRenderer};
use crate::journal::{Journal, OpenReport};
use crate::paths::{PathError, RepoPath, must_not_be_symlink};
use crate::pending::{PendingId, PendingStore};
use crate::record::{Caller, Change, Draft, Record, Trigger};

/// Layout of a state area (design §2.2).
#[derive(Debug, Clone)]
pub struct StateDir {
    root: PathBuf,
}

impl StateDir {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn git(&self) -> PathBuf {
        self.root.join("git")
    }

    pub fn journal(&self) -> PathBuf {
        self.root.join("journal").join("journal")
    }

    pub fn pending(&self) -> PathBuf {
        self.root.join("pending")
    }
}

#[derive(Debug, thiserror::Error)]
pub enum VolumeError {
    #[error(transparent)]
    History(#[from] HistoryError),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Path(#[from] PathError),
    #[error("{0} must be a regular file, not a symlink")]
    SymlinkNotAllowed(RepoPath),
}

/// One path's new state, as the FUSE layer sees it.
#[derive(Debug, Clone)]
pub enum Input {
    /// `path` holds the content currently at `backing`.
    Write {
        path: RepoPath,
        backing: PathBuf,
        executable: bool,
    },
    Symlink {
        path: RepoPath,
        target: String,
    },
    Remove {
        path: RepoPath,
    },
}

/// What [`open`] found.
#[derive(Debug)]
pub struct Recovery {
    pub journal: OpenReport,
    /// Last sequence number already in history.
    pub committed_through: Option<u64>,
    /// Records recovered that still need committing.
    pub uncommitted: usize,
    /// Frozen copies removed because nothing referred to them.
    pub pruned_pending: usize,
}

/// Creates a new state area for `volume`.
pub fn init(state: &StateDir, volume: &str, branch: Option<&str>) -> Result<(), VolumeError> {
    History::init(&state.git(), volume, branch)?;
    std::fs::create_dir_all(state.journal().parent().expect("journal has a parent"))?;
    PendingStore::open(state.pending())?;
    Ok(())
}

/// Opens an existing state area.
pub fn open(
    state: &StateDir,
    context: Context,
) -> Result<(Recorder, Committer, Recovery), VolumeError> {
    let history = History::open(&state.git())?;
    let identities = IdentityRenderer::new(history.settings().identity.clone(), &context)
        .map_err(HistoryError::from)?;
    let committed_through = history.head_seq()?;

    let pending = Arc::new(PendingStore::open(state.pending())?);
    let hook_store = pending.clone();
    let (journal, report) = Journal::open(
        state.journal(),
        committed_through.map_or(0, |s| s + 1),
        Box::new(move || hook_store.sync_dir()),
    )?;
    if let Some(seq) = committed_through {
        journal.truncate_through(seq)?;
    }
    let uncommitted = journal.uncommitted();
    let keep: HashSet<PendingId> = uncommitted.iter().flat_map(Record::pending_ids).collect();
    let pruned_pending = pending.prune_except(&keep)?;
    pending.sync_dir()?;

    let journal = Arc::new(journal);
    let recovery = Recovery {
        journal: report,
        committed_through,
        uncommitted: uncommitted.len(),
        pruned_pending,
    };
    let recorder = Recorder {
        journal: journal.clone(),
        pending: pending.clone(),
    };
    let committer = Committer {
        history,
        journal,
        pending,
        identities,
        context,
    };
    Ok((recorder, committer, recovery))
}

/// Records change boundaries. Cheap to clone; safe to share across threads.
#[derive(Clone)]
pub struct Recorder {
    journal: Arc<Journal>,
    pending: Arc<PendingStore>,
}

impl Recorder {
    /// Durably records one change boundary, returning its sequence number
    /// once it is safe to let the triggering call return.
    pub fn record(
        &self,
        trigger: Trigger,
        caller: Caller,
        inputs: Vec<Input>,
    ) -> Result<u64, VolumeError> {
        let mut changes = Vec::with_capacity(inputs.len());
        let mut frozen = Vec::new();
        let result = (|| {
            for input in inputs {
                changes.push(match input {
                    Input::Write {
                        path,
                        backing,
                        executable,
                    } => {
                        let (id, _) = self.pending.freeze(&backing)?;
                        frozen.push(id);
                        Change::Write {
                            path,
                            pending: id,
                            executable,
                        }
                    }
                    Input::Symlink { path, target } => {
                        if must_not_be_symlink(&path) {
                            return Err(VolumeError::SymlinkNotAllowed(path));
                        }
                        Change::Symlink { path, target }
                    }
                    Input::Remove { path } => Change::Remove { path },
                });
            }
            let draft = Draft {
                trigger,
                caller,
                time_ms: now_ms(),
                changes,
            };
            Ok(self.journal.record(draft)?)
        })();
        if result.is_err() {
            // Copies frozen for a record that never became durable.
            let _ = self.pending.remove(frozen);
        }
        result
    }
}

/// Turns recorded changes into commits.
pub struct Committer {
    history: History,
    journal: Arc<Journal>,
    pending: Arc<PendingStore>,
    identities: IdentityRenderer,
    context: Context,
}

/// What one [`Committer::commit_pending`] pass did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct CommitReport {
    pub records: usize,
    pub commits: usize,
    pub last_seq: Option<u64>,
}

impl Committer {
    pub fn history(&self) -> &History {
        &self.history
    }

    /// Records durable but not yet in history.
    pub fn backlog(&self) -> usize {
        self.journal.uncommitted().len()
    }

    /// Commits every durable record, grouping records whose times fall
    /// within the configured commit window into one commit.
    pub fn commit_pending(&self) -> Result<CommitReport, VolumeError> {
        let records = self.journal.uncommitted();
        let window_ms = self.history.settings().commit_window.as_millis() as u64;
        let mut report = CommitReport::default();
        let mut result = Ok(());
        for group in group_by_window(&records, window_ms) {
            match self
                .history
                .commit(group, &self.pending, &self.identities, &self.context)
            {
                Ok(Committed { commit, last_seq }) => {
                    report.records += group.len();
                    report.commits += usize::from(commit.is_some());
                    report.last_seq = Some(last_seq);
                }
                Err(e) => {
                    result = Err(e.into());
                    break;
                }
            }
        }
        if let Some(seq) = report.last_seq {
            self.journal.truncate_through(seq)?;
            let done = records
                .iter()
                .take_while(|r| r.seq <= seq)
                .flat_map(Record::pending_ids);
            self.pending.remove(done)?;
            self.pending.sync_dir()?;
        }
        result.map(|()| report)
    }
}

/// Splits records into runs where each record is within `window_ms` of the
/// run's first record.
fn group_by_window(records: &[Record], window_ms: u64) -> Vec<&[Record]> {
    let mut groups = Vec::new();
    let mut start = 0;
    for i in 1..records.len() {
        let first = records[start].draft.time_ms;
        if records[i].draft.time_ms.saturating_sub(first) > window_ms {
            groups.push(&records[start..i]);
            start = i;
        }
    }
    if start < records.len() {
        groups.push(&records[start..]);
    }
    groups
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    const CALLER: Caller = Caller {
        uid: 10000,
        gid: 10000,
        pid: 1,
    };

    fn context() -> Context {
        Context {
            volume: "vol".into(),
            replica: "pod-0".into(),
            pod_uid: "u".into(),
            ..Context::default()
        }
    }

    fn setup() -> (tempfile::TempDir, StateDir) {
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(dir.path().join("state"));
        init(&state, "vol", None).unwrap();
        (dir, state)
    }

    fn write(dir: &Path, name: &str, content: &str) -> Input {
        let backing = dir.join(name.replace('/', "_"));
        fs::write(&backing, content).unwrap();
        Input::Write {
            path: RepoPath::new(name).unwrap(),
            backing,
            executable: false,
        }
    }

    fn tree_paths(c: &Committer) -> Vec<String> {
        let head = c.history().head().unwrap().unwrap();
        let mut out = Vec::new();
        head.tree()
            .unwrap()
            .walk(git2::TreeWalkMode::PreOrder, |root, e| {
                if e.kind() == Some(git2::ObjectType::Blob) {
                    out.push(format!("{root}{}", e.name().unwrap()));
                }
                git2::TreeWalkResult::Ok
            })
            .unwrap();
        out
    }

    #[test]
    fn record_then_commit() {
        let (dir, state) = setup();
        let (rec, com, recovery) = open(&state, context()).unwrap();
        assert_eq!(recovery.committed_through, None);

        rec.record(Trigger::Close, CALLER, vec![write(dir.path(), "a", "1")])
            .unwrap();
        rec.record(Trigger::Close, CALLER, vec![write(dir.path(), "d/b", "2")])
            .unwrap();
        assert_eq!(com.backlog(), 2);

        let report = com.commit_pending().unwrap();
        assert_eq!(report.records, 2);
        assert_eq!(report.last_seq, Some(1));
        assert_eq!(com.backlog(), 0);
        assert_eq!(tree_paths(&com), ["a", "d/b"]);
        assert_eq!(fs::read_dir(state.pending()).unwrap().count(), 0);
    }

    #[test]
    fn content_is_what_it_was_at_the_boundary() {
        let (dir, state) = setup();
        let (rec, com, _) = open(&state, context()).unwrap();
        let input = write(dir.path(), "a", "at close");
        rec.record(Trigger::Close, CALLER, vec![input]).unwrap();
        fs::write(dir.path().join("a"), "changed later").unwrap();

        com.commit_pending().unwrap();
        let head = com.history().head().unwrap().unwrap();
        let blob = head.tree().unwrap().get_name("a").unwrap().id();
        let content = com.history().repo().find_blob(blob).unwrap();
        assert_eq!(content.content(), b"at close");
    }

    #[test]
    fn recorded_but_uncommitted_survives_a_crash() {
        let (dir, state) = setup();
        {
            let (rec, com, _) = open(&state, context()).unwrap();
            rec.record(Trigger::Close, CALLER, vec![write(dir.path(), "a", "1")])
                .unwrap();
            com.commit_pending().unwrap();
            rec.record(Trigger::Close, CALLER, vec![write(dir.path(), "b", "2")])
                .unwrap();
            // Crash: the second record is durable but never committed.
        }
        let (rec, com, recovery) = open(&state, context()).unwrap();
        assert_eq!(recovery.committed_through, Some(0));
        assert_eq!(recovery.uncommitted, 1);

        com.commit_pending().unwrap();
        assert_eq!(tree_paths(&com), ["a", "b"]);
        // New records continue after the recovered ones.
        let seq = rec
            .record(Trigger::Close, CALLER, vec![write(dir.path(), "c", "3")])
            .unwrap();
        assert_eq!(seq, 2);
    }

    #[test]
    fn crash_after_commit_before_truncate_does_not_duplicate() {
        let (dir, state) = setup();
        {
            let (rec, com, _) = open(&state, context()).unwrap();
            rec.record(Trigger::Close, CALLER, vec![write(dir.path(), "a", "1")])
                .unwrap();
            // Commit without letting the committer truncate: simulates a crash
            // between the ref update and journal truncation.
            let records = com.journal.uncommitted();
            com.history
                .commit(&records, &com.pending, &com.identities, &com.context)
                .unwrap();
        }
        let (_, com, recovery) = open(&state, context()).unwrap();
        assert_eq!(recovery.committed_through, Some(0));
        assert_eq!(recovery.uncommitted, 0);
        assert_eq!(recovery.pruned_pending, 1);
        assert_eq!(com.commit_pending().unwrap(), CommitReport::default());
        let head = com.history().head().unwrap().unwrap();
        assert_eq!(head.parent_count(), 0, "no duplicate commit");
    }

    #[test]
    fn failed_record_leaves_no_frozen_copy() {
        let (dir, state) = setup();
        let (rec, _, _) = open(&state, context()).unwrap();
        let err = rec.record(
            Trigger::Close,
            CALLER,
            vec![
                write(dir.path(), "a", "1"),
                Input::Symlink {
                    path: RepoPath::new(".gitmodules").unwrap(),
                    target: "x".into(),
                },
            ],
        );
        assert!(matches!(err, Err(VolumeError::SymlinkNotAllowed(_))));
        assert_eq!(fs::read_dir(state.pending()).unwrap().count(), 0);
    }

    #[test]
    fn grouping_by_window() {
        let rec = |seq, time_ms| Record {
            seq,
            draft: Draft {
                trigger: Trigger::Close,
                caller: CALLER,
                time_ms,
                changes: vec![],
            },
        };
        let records = [
            rec(0, 0),
            rec(1, 150),
            rec(2, 200),
            rec(3, 201),
            rec(4, 900),
        ];
        let groups: Vec<Vec<u64>> = group_by_window(&records, 200)
            .into_iter()
            .map(|g| g.iter().map(|r| r.seq).collect())
            .collect();
        assert_eq!(groups, [vec![0, 1, 2], vec![3], vec![4]]);
        assert!(group_by_window(&[], 200).is_empty());
    }
}
