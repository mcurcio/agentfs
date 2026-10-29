//! Checkpoints: one change-line commit per intent (design §3.4, §19.2
//! item 11).
//!
//! The volume's branch records every change (§3); the **change line**,
//! `refs/agentfs/<volume>/change-line`, is a coarser history beside it:
//! one commit per checkpoint, which a caller cuts when an intent is done
//! (`agentfs checkpoint -m MESSAGE`, or the control socket's
//! `checkpoint`).
//!
//! A checkpoint commit C has the branch head's tree; its first parent is
//! the previous change-line tip (none for the first, unless the volume is
//! a fork whose source had a change line at create:
//! [`BASE_CHANGE_LINE_KEY`]), and its further parents are the tips
//! (`applied/<n>`) of the patches landed on the branch since that tip,
//! by inbox number. Its author is the caller (§9), its committer the
//! replica, its message the caller's, then `Agentfs-Trigger: checkpoint`
//! and `Agentfs-Branch-Head: <head>`.
//!
//! C is then merged back: the branch gets a marker with parents
//! `[head, C]` ([`History::commit_checkpoint_marker`]), so every
//! checkpoint stays reachable from the branch, and its first-parent
//! history (inbox trailers, the sequence) is unchanged. The change-line
//! ref moves before the marker: a crash between the two leaves a tip not
//! merged back, which the next cut merges first.
//!
//! A remote with `agentfsPush = changeLine` pushes the change line to its
//! branch ref instead of the branch ([`crate::config::PushSource`]).

use git2::{ErrorCode, Oid, Repository};

use crate::history::HistoryError;
use crate::record::Caller;
use crate::volume::Committer;

/// `agentfs.fork.baseChangeLine`: a fork's source's change-line tip at
/// create (fork identity: written by fork create, never by an include).
pub const BASE_CHANGE_LINE_KEY: &str = "agentfs.fork.baseChangeLine";
/// The trigger of a checkpoint commit and its marker.
pub const CHECKPOINT_TRIGGER: &str = "checkpoint";
/// The marker's trailer naming the checkpoint commit it merged back.
pub const CHECKPOINT_TRAILER: &str = "Agentfs-Checkpoint";
/// The checkpoint commit's trailer naming the branch head it was cut at.
pub const BRANCH_HEAD_TRAILER: &str = "Agentfs-Branch-Head";
/// The message of a checkpoint cut without one.
pub const DEFAULT_MESSAGE: &str = "checkpoint";

/// `refs/agentfs/<volume>/change-line`.
pub fn change_line_ref(volume: &str) -> String {
    format!("refs/agentfs/{volume}/change-line")
}

/// What to cut.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointRequest {
    /// The checkpoint's message; [`DEFAULT_MESSAGE`] when `None` or blank.
    pub message: Option<String>,
    /// Who asked: the checkpoint's author (§9).
    pub caller: Caller,
}

/// What [`cut`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckpointOutcome {
    /// A checkpoint commit `change` on the change line, merged back into
    /// the branch by `marker`; `patches` are the inbox numbers whose tips
    /// it merged.
    Cut {
        change: Oid,
        marker: Oid,
        patches: Vec<u64>,
    },
    /// The head's tree is the change-line tip's and no patch landed since:
    /// no commit.
    NothingNew,
}

#[derive(Debug, thiserror::Error)]
pub enum CheckpointError {
    #[error("the volume's branch does not exist yet: nothing to checkpoint")]
    NoBranch,
    #[error(transparent)]
    History(#[from] HistoryError),
    #[error(transparent)]
    Git(#[from] git2::Error),
}

/// Cuts a checkpoint on `committer`'s history. Precondition: the caller
/// is the branch's writer (the commit thread, or the lock holder after
/// commit_pending).
pub fn cut(
    committer: &Committer,
    request: &CheckpointRequest,
) -> Result<CheckpointOutcome, CheckpointError> {
    let history = committer.history();
    let repo = history.repo();
    let volume = &history.settings().volume;
    let line_ref = change_line_ref(volume);
    let head = history.head()?.ok_or(CheckpointError::NoBranch)?.id();
    let message = request
        .message
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .unwrap_or(DEFAULT_MESSAGE)
        .to_owned();
    let author = committer.author_signature(request.caller)?;
    let committer_sig = committer.committer_signature()?;

    let tip = find(repo, &line_ref)?;
    // A tip whose marker a crash prevented is merged back first.
    let mut recovered = None;
    if let Some(tip) = tip
        && !merged(repo, head, tip)?
    {
        let subject = subject(repo, tip)?;
        recovered =
            Some(history.commit_checkpoint_marker(tip, &subject, &author, &committer_sig)?);
    }
    let head = history.head()?.ok_or(CheckpointError::NoBranch)?;
    let first = match tip {
        Some(tip) => Some(tip),
        None => history.settings().fork_base_change_line,
    };
    let since = match first {
        Some(first) => branch_head_of(repo, first)?,
        None => None,
    };
    let landed = history.find_inbox_commits(since)?;
    let mut patches = Vec::with_capacity(landed.len());
    let mut parents: Vec<Oid> = first.into_iter().collect();
    for (n, commit) in &landed {
        let applied = crate::patch::applied_ref(volume, *n);
        let patch_tip = find(repo, &applied)?.unwrap_or(commit.patch);
        if !parents.contains(&patch_tip) {
            parents.push(patch_tip);
            patches.push(*n);
        }
    }
    if let Some(first) = first
        && patches.is_empty()
        && repo.find_commit(first)?.tree_id() == head.tree_id()
    {
        return Ok(match (recovered, tip) {
            (Some(marker), Some(change)) => CheckpointOutcome::Cut {
                change,
                marker,
                patches: Vec::new(),
            },
            _ => CheckpointOutcome::NothingNew,
        });
    }
    let change_message = format!(
        "{message}\n\nAgentfs-Trigger: {CHECKPOINT_TRIGGER}\n{BRANCH_HEAD_TRAILER}: {}\n",
        head.id()
    );
    let change = history.commit_ref(
        &line_ref,
        tip,
        head.tree_id(),
        &parents,
        &change_message,
        (&author, &committer_sig),
    )?;
    let marker = history.commit_checkpoint_marker(change, &message, &author, &committer_sig)?;
    Ok(CheckpointOutcome::Cut {
        change,
        marker,
        patches,
    })
}

/// The commit `refname` names, if it exists.
fn find(repo: &Repository, refname: &str) -> Result<Option<Oid>, git2::Error> {
    match repo.refname_to_id(refname) {
        Ok(oid) => Ok(Some(oid)),
        Err(e) if e.code() == ErrorCode::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Whether `tip` is `head` or an ancestor of it.
fn merged(repo: &Repository, head: Oid, tip: Oid) -> Result<bool, git2::Error> {
    Ok(head == tip || repo.graph_descendant_of(head, tip)?)
}

/// The first line of `commit`'s message.
fn subject(repo: &Repository, commit: Oid) -> Result<String, git2::Error> {
    let commit = repo.find_commit(commit)?;
    Ok(commit
        .message_raw()
        .ok()
        .and_then(|m| m.lines().next())
        .unwrap_or(DEFAULT_MESSAGE)
        .to_owned())
}

/// The `Agentfs-Branch-Head` a checkpoint commit was cut at: where the
/// scan for the patches landed since it starts. Read only from the last
/// paragraph with `Agentfs-Trigger: checkpoint`, like the inbox trailer
/// ([`crate::history::INBOX_TRAILER`]); `None` (the whole history) when absent.
fn branch_head_of(repo: &Repository, change: Oid) -> Result<Option<Oid>, git2::Error> {
    let commit = repo.find_commit(change)?;
    let message = commit.message_raw().unwrap_or_default();
    let last = message.trim_end_matches('\n').rsplit("\n\n").next();
    let Some(last) = last else { return Ok(None) };
    let trigger = format!("Agentfs-Trigger: {CHECKPOINT_TRIGGER}");
    if !last.lines().any(|l| l == trigger) {
        return Ok(None);
    }
    let prefix = format!("{BRANCH_HEAD_TRAILER}: ");
    Ok(last
        .lines()
        .find_map(|l| l.strip_prefix(&prefix))
        .and_then(|oid| Oid::from_str(oid.trim()).ok()))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::sync::Arc;

    use git2::Repository;

    use super::*;
    use crate::events::NullSink;
    use crate::history::{checkpoint_from_message, inbox_from_message, seq_from_message};
    use crate::identity::Context;
    use crate::paths::RepoPath;
    use crate::record::{Operation, OperationKind, Trigger};
    use crate::telemetry::Telemetry;
    use crate::volume::{self, Input, Recorder, StateDir};

    const CALLER: Caller = Caller {
        uid: 7,
        gid: 7,
        pid: 7,
    };

    struct Fixture {
        dir: tempfile::TempDir,
        state: StateDir,
        rec: Recorder,
        com: Committer,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let state = StateDir::new(dir.path().join("state"));
            volume::init(&state, "vol", None).unwrap();
            fs::create_dir_all(dir.path().join("tree")).unwrap();
            let (rec, com, _) = open(dir.path(), &state);
            Self {
                dir,
                state,
                rec,
                com,
            }
        }

        fn record(&self, name: &str, content: &str) {
            let backing = self.dir.path().join("tree").join(name);
            fs::write(&backing, content).unwrap();
            self.rec
                .record(
                    Trigger::Close,
                    CALLER,
                    vec![Input::Write {
                        path: RepoPath::new(name).unwrap(),
                        backing,
                        executable: false,
                    }],
                )
                .unwrap();
            self.com.commit_pending().unwrap();
        }

        fn repo(&self) -> Repository {
            Repository::open_bare(self.state.git()).unwrap()
        }

        fn head(&self) -> Oid {
            self.repo().refname_to_id("refs/heads/agentfs/vol").unwrap()
        }

        fn cut(&self, message: &str) -> CheckpointOutcome {
            cut(
                &self.com,
                &CheckpointRequest {
                    message: Some(message.into()),
                    caller: CALLER,
                },
            )
            .unwrap()
        }

        /// A patch landed as the queue lands one that changes nothing:
        /// its commit on top of the head, `applied/<n>` at it, and the
        /// inbox marker on the branch.
        fn land_patch(&self, n: u64) -> Oid {
            let repo = self.repo();
            let head = repo.find_commit(self.head()).unwrap();
            let sig = git2::Signature::now("producer", "p@example.com").unwrap();
            let tip = repo
                .commit(
                    None,
                    &sig,
                    &sig,
                    &format!("patch {n}"),
                    &head.tree().unwrap(),
                    &[&head],
                )
                .unwrap();
            repo.reference(&crate::patch::applied_ref("vol", n), tip, true, "test")
                .unwrap();
            self.com
                .commit_patch_marker(&Operation {
                    id: n,
                    kind: OperationKind::Patch,
                    source: tip.to_string(),
                    summary: format!("patch {n}"),
                    caller: CALLER,
                    inbox: Some(n),
                    author: None,
                })
                .unwrap();
            tip
        }
    }

    fn open(dir: &Path, state: &StateDir) -> (Recorder, Committer, ()) {
        let context = Context {
            volume: "vol".into(),
            replica: "r".into(),
            pod_uid: "u".into(),
            ..Context::default()
        };
        let (rec, com, _) = volume::open(
            state,
            &dir.join("tree"),
            context,
            Arc::new(NullSink),
            &Telemetry::noop(),
        )
        .unwrap();
        (rec, com, ())
    }

    #[test]
    fn a_checkpoint_is_one_commit_merged_back() {
        let f = Fixture::new();
        f.record("a", "1");
        f.record("b", "2");
        f.record("c", "3");
        let head = f.head();
        let seq = f.com.history().head_seq().unwrap();
        let CheckpointOutcome::Cut {
            change,
            marker,
            patches,
        } = f.cut("intent A")
        else {
            panic!("nothing cut");
        };
        assert!(patches.is_empty());
        let repo = f.repo();
        assert_eq!(repo.refname_to_id(&change_line_ref("vol")).unwrap(), change);
        let c = repo.find_commit(change).unwrap();
        assert_eq!(c.parent_count(), 0, "the first checkpoint has no parent");
        assert_eq!(c.tree_id(), repo.find_commit(head).unwrap().tree_id());
        let message = c.message().unwrap();
        assert!(message.starts_with("intent A\n\n"), "{message}");
        assert!(message.contains(&format!("{BRANCH_HEAD_TRAILER}: {head}")));
        assert_eq!(c.committer().name().ok(), Some("r"), "the replica commits");
        assert_eq!(c.author().name().ok(), Some("uid-7"), "the caller authors");

        let m = repo.find_commit(marker).unwrap();
        assert_eq!(f.head(), marker);
        assert_eq!(m.parent_ids().collect::<Vec<_>>(), [head, change]);
        assert_eq!(m.tree_id(), c.tree_id());
        let text = m.message().unwrap();
        assert_eq!(checkpoint_from_message(text), Some(change));
        assert_eq!(seq_from_message(text), seq);
        assert_eq!(f.com.history().head_seq().unwrap(), seq);
        assert_eq!(inbox_from_message(text), None);

        // The branch commits on top of the marker as before.
        f.record("d", "4");
        let next = repo.find_commit(f.head()).unwrap();
        assert_eq!(next.parent_ids().collect::<Vec<_>>(), [marker]);
    }

    #[test]
    fn nothing_new_cuts_nothing() {
        let f = Fixture::new();
        f.record("a", "1");
        assert!(matches!(f.cut("one"), CheckpointOutcome::Cut { .. }));
        let head = f.head();
        assert_eq!(f.cut("two"), CheckpointOutcome::NothingNew);
        assert_eq!(f.head(), head);
        // A change and its undo leave the tree as it was: nothing new.
        f.record("a", "2");
        f.record("a", "1");
        assert_eq!(f.cut("three"), CheckpointOutcome::NothingNew);
    }

    #[test]
    fn no_branch_is_refused() {
        let f = Fixture::new();
        let err = cut(
            &f.com,
            &CheckpointRequest {
                message: None,
                caller: CALLER,
            },
        )
        .unwrap_err();
        assert!(matches!(err, CheckpointError::NoBranch), "{err}");
    }

    #[test]
    fn patches_landed_since_become_parents() {
        let f = Fixture::new();
        f.record("a", "1");
        let CheckpointOutcome::Cut { change: first, .. } = f.cut("first") else {
            panic!()
        };
        let p1 = f.land_patch(1);
        // A patch that changed nothing is still new.
        let CheckpointOutcome::Cut {
            change, patches, ..
        } = f.cut("second")
        else {
            panic!("the landed patch was not cut");
        };
        assert_eq!(patches, [1]);
        let repo = f.repo();
        let c = repo.find_commit(change).unwrap();
        assert_eq!(c.parent_ids().collect::<Vec<_>>(), [first, p1]);
        // The default message.
        assert_eq!(
            cut(
                &f.com,
                &CheckpointRequest {
                    message: Some("  ".into()),
                    caller: CALLER,
                },
            )
            .unwrap(),
            CheckpointOutcome::NothingNew
        );
        f.record("b", "2");
        let CheckpointOutcome::Cut { change: third, .. } = cut(
            &f.com,
            &CheckpointRequest {
                message: None,
                caller: CALLER,
            },
        )
        .unwrap() else {
            panic!()
        };
        let t = repo.find_commit(third).unwrap();
        assert_eq!(t.parent_ids().collect::<Vec<_>>(), [change]);
        assert!(t.message().unwrap().starts_with("checkpoint\n\n"));
    }

    #[test]
    fn a_tip_a_crash_left_unmerged_is_merged_first() {
        let f = Fixture::new();
        f.record("a", "1");
        let CheckpointOutcome::Cut { change, marker, .. } = f.cut("first") else {
            panic!()
        };
        // As if the marker was never written.
        let repo = f.repo();
        let before = repo.find_commit(marker).unwrap().parent_id(0).unwrap();
        repo.reference("refs/heads/agentfs/vol", before, true, "test")
            .unwrap();
        let CheckpointOutcome::Cut {
            change: again,
            marker: merged,
            ..
        } = f.cut("again")
        else {
            panic!("the unmerged tip was not merged back");
        };
        assert_eq!(again, change);
        assert_eq!(
            repo.find_commit(merged)
                .unwrap()
                .parent_ids()
                .collect::<Vec<_>>(),
            [before, change]
        );
        assert_eq!(f.cut("once more"), CheckpointOutcome::NothingNew);
    }
}
