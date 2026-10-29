//! Acceptance rules: whether a cleanly merged patch may land (design §12,
//! §19.2 item 11; bead agentfs-mia.2).
//!
//! The owner, 2026-10-16: "Not A, because that assumes that a clean merge
//! is an acceptable merge"; 2026-10-29: "Configurable with a sensible
//! default". An [`AcceptanceRule`] judges each patch after its merge onto
//! the head is clean and before anything is written. One rule per volume,
//! chosen by `agentfs.patchAccept` ([`AcceptMode`]); the factory in this
//! module (bead agentfs-mia.2) is the only place a mode becomes a rule:
//!
//! - [`AcceptMode::CleanMerge`] (the default): a clean merge is accepted;
//! - [`AcceptMode::Approval`]: accepted once
//!   `refs/agentfs/<volume>/approved/<n>` names the patch's tip
//!   (`agentfs patch approve`), else awaiting approval;
//! - [`AcceptMode::Command`]: the operator's `$GIT_DIR/hooks/patch-accept`,
//!   run without a shell through the command runner shared with fork
//!   delivery ([`crate::command`]), the candidate as JSON
//!   ([`CandidateJson`]) on stdin, within
//!   `agentfs.patchAcceptTimeout`: exit 0 accepts, 1 refuses (its first
//!   line of stderr is the message), 75 awaits; anything else, a signal, a
//!   timeout or a hook that cannot start is [`RuleNotWorking`].
//!
//! A rule that is not working leaves the patch not judged (set aside,
//! retried at the next run) and raises an alarm; it never accepts.

use std::path::{Path, PathBuf};
use std::time::Duration;

use git2::{ErrorCode, Oid, Repository};
use serde::{Deserialize, Serialize};

use super::approved_ref;
use super::integrate::PatchBranch;
use crate::command::{Finished, RunError, run_capturing_stderr};

/// The `patch-accept` hook, under the state area's git directory.
pub const ACCEPT_HOOK: &str = "hooks/patch-accept";

/// The exit status a hook uses for "not yet" (`EX_TEMPFAIL`).
const EXIT_TEMPFAIL: i32 = 75;

/// `agentfs.patchAccept`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AcceptMode {
    /// `cleanMerge`, the default.
    #[default]
    CleanMerge,
    /// `approval`.
    Approval,
    /// `command`: `$GIT_DIR/hooks/patch-accept`.
    Command,
}

/// What a rule is asked about: patch `inbox` of `volume`, whose merge onto
/// `head` is clean and gave the tree `merged`.
#[derive(Debug, Clone, Copy)]
pub struct Candidate<'a> {
    pub volume: &'a str,
    pub inbox: u64,
    pub patch: &'a PatchBranch,
    pub head: Oid,
    pub merged: Oid,
}

/// What an acceptance rule wants done with a patch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Acceptance {
    /// Land it.
    Accept,
    /// Do not land it: held, as `refused`, until replaced or skipped.
    Refuse { message: String },
    /// Not yet: set aside until the rule accepts it at a later run.
    Await { what: AwaitWhat },
}

/// What an awaiting patch waits for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AwaitWhat {
    /// `agentfs patch approve <n>` for this tip.
    Approval,
    /// The `patch-accept` hook exited 75.
    Command,
}

impl AwaitWhat {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Approval => "approval",
            Self::Command => "command",
        }
    }
}

/// Why a rule could not judge a patch (the patch is not judged, and an
/// alarm is raised).
#[derive(Debug, thiserror::Error)]
pub enum RuleNotWorking {
    #[error("the acceptance rule {rule} could not start: {detail}")]
    NotStarted { rule: &'static str, detail: String },
    #[error("the acceptance rule {rule} failed: {detail}")]
    Crashed { rule: &'static str, detail: String },
    #[error("the acceptance rule {rule} was killed after {after:?}")]
    TimedOut { rule: &'static str, after: Duration },
    /// The approval rule could not read the volume's refs.
    #[error("the acceptance rule could not read the repository: {0}")]
    Unreadable(#[from] git2::Error),
}

/// Judges whether a cleanly merged patch may land.
///
/// Invariants: a rule never writes a ref, a file or the journal; it is
/// asked only about clean merges; an error is never an acceptance.
pub trait AcceptanceRule {
    /// The rule's name in status and alarms (`cleanMerge`, `approval`,
    /// `command`).
    fn name(&self) -> &'static str;

    /// Judges `candidate`; `repo` is the volume's history, read only.
    fn judge(
        &self,
        repo: &Repository,
        candidate: &Candidate<'_>,
    ) -> Result<Acceptance, RuleNotWorking>;
}

/// [`AcceptMode::CleanMerge`]: always [`Acceptance::Accept`].
#[derive(Debug, Clone, Copy, Default)]
pub struct CleanMerge;

/// [`AcceptMode::Approval`]: accepted when
/// `refs/agentfs/<volume>/approved/<n>` names the candidate's tip.
#[derive(Debug, Clone, Copy, Default)]
pub struct ApprovalRule;

/// [`AcceptMode::Command`]: the `patch-accept` hook.
#[derive(Debug, Clone)]
pub struct CommandRule {
    /// `$GIT_DIR/hooks/patch-accept`.
    hook: PathBuf,
    /// `agentfs.patchAcceptTimeout`.
    timeout: Duration,
}

/// The JSON on the hook's stdin (schema 1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CandidateJson {
    /// 1.
    pub schema: u32,
    pub volume: String,
    pub inbox: u64,
    /// The patch's tip, as `inbox/<n>` names it.
    pub tip: String,
    /// The last commit of the volume's branch it builds on.
    pub base: String,
    /// The branch head it was merged onto.
    pub head: String,
    /// The tree the clean merge gave.
    pub merged_tree: String,
    /// Its commits, oldest first.
    pub commits: Vec<String>,
    /// The paths it changes, sorted.
    pub paths: Vec<String>,
    pub barrier: bool,
}

impl CandidateJson {
    pub fn new(candidate: &Candidate<'_>) -> Self {
        let patch = candidate.patch;
        Self {
            schema: 1,
            volume: candidate.volume.to_owned(),
            inbox: candidate.inbox,
            tip: patch.tip.to_string(),
            base: patch.base.to_string(),
            head: candidate.head.to_string(),
            merged_tree: candidate.merged.to_string(),
            commits: patch.commits.iter().map(Oid::to_string).collect(),
            paths: patch.paths.iter().map(|p| p.as_str().to_owned()).collect(),
            barrier: patch.barrier,
        }
    }
}

/// The one place a mode becomes a rule: `git_dir` is the state area's git
/// directory (for the hook), `timeout` `agentfs.patchAcceptTimeout`.
pub fn rule(mode: AcceptMode, git_dir: &Path, timeout: Duration) -> Box<dyn AcceptanceRule> {
    match mode {
        AcceptMode::CleanMerge => Box::new(CleanMerge),
        AcceptMode::Approval => Box::new(ApprovalRule),
        AcceptMode::Command => Box::new(CommandRule::new(git_dir.join(ACCEPT_HOOK), timeout)),
    }
}

impl AcceptanceRule for CleanMerge {
    fn name(&self) -> &'static str {
        "cleanMerge"
    }

    fn judge(&self, _: &Repository, _: &Candidate<'_>) -> Result<Acceptance, RuleNotWorking> {
        Ok(Acceptance::Accept)
    }
}

impl AcceptanceRule for ApprovalRule {
    fn name(&self) -> &'static str {
        "approval"
    }

    /// Accepted only when the approval names this very tip: a replaced
    /// patch needs approving again.
    fn judge(
        &self,
        repo: &Repository,
        candidate: &Candidate<'_>,
    ) -> Result<Acceptance, RuleNotWorking> {
        let awaiting = Acceptance::Await {
            what: AwaitWhat::Approval,
        };
        match repo.refname_to_id(&approved_ref(candidate.volume, candidate.inbox)) {
            Ok(oid) if oid == candidate.patch.tip => Ok(Acceptance::Accept),
            Ok(_) => Ok(awaiting),
            Err(e) if e.code() == ErrorCode::NotFound => Ok(awaiting),
            Err(e) => Err(RuleNotWorking::Unreadable(e)),
        }
    }
}

impl CommandRule {
    pub fn new(hook: PathBuf, timeout: Duration) -> Self {
        Self { hook, timeout }
    }
}

impl AcceptanceRule for CommandRule {
    fn name(&self) -> &'static str {
        "command"
    }

    /// Runs the hook (see the module docs).
    fn judge(
        &self,
        _: &Repository,
        candidate: &Candidate<'_>,
    ) -> Result<Acceptance, RuleNotWorking> {
        let rule = self.name();
        let stdin = serde_json::to_vec(&CandidateJson::new(candidate)).map_err(|e| {
            RuleNotWorking::NotStarted {
                rule,
                detail: e.to_string(),
            }
        })?;
        let argv = [self.hook.to_string_lossy().into_owned()];
        let hook = self.hook.display();
        let (finished, stderr) = match run_capturing_stderr(&argv, &stdin, self.timeout) {
            Ok(done) => done,
            Err(RunError::NotStarted(e)) => {
                return Err(RuleNotWorking::NotStarted {
                    rule,
                    detail: format!("{hook}: {e}"),
                });
            }
            Err(RunError::TimedOut(after)) => return Err(RuleNotWorking::TimedOut { rule, after }),
            Err(RunError::Wait(e)) => {
                return Err(RuleNotWorking::Crashed {
                    rule,
                    detail: format!("waiting for {hook}: {e}"),
                });
            }
        };
        let first_line = String::from_utf8_lossy(&stderr)
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .map(str::to_owned);
        match finished {
            Finished::Code(0) => Ok(Acceptance::Accept),
            Finished::Code(1) => Ok(Acceptance::Refuse {
                message: first_line.unwrap_or_else(|| "refused by patch-accept".to_owned()),
            }),
            Finished::Code(EXIT_TEMPFAIL) => Ok(Acceptance::Await {
                what: AwaitWhat::Command,
            }),
            Finished::Code(code) => Err(RuleNotWorking::Crashed {
                rule,
                detail: match first_line {
                    Some(line) => format!("{hook} exited with {code}: {line}"),
                    None => format!("{hook} exited with {code}"),
                },
            }),
            Finished::Signal(signal) => Err(RuleNotWorking::Crashed {
                rule,
                detail: format!("{hook} was killed by signal {signal}"),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use git2::Signature;

    use super::*;
    use crate::paths::RepoPath;

    /// A repository with two commits and a patch branch of the second.
    fn fixture() -> (tempfile::TempDir, Repository, PatchBranch) {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init_bare(dir.path()).unwrap();
        let sig = Signature::now("Ada", "ada@example.com").unwrap();
        let tree = repo
            .find_tree(repo.treebuilder(None).unwrap().write().unwrap())
            .unwrap();
        let base = repo.commit(None, &sig, &sig, "base", &tree, &[]).unwrap();
        let parent = repo.find_commit(base).unwrap();
        let tip = repo
            .commit(None, &sig, &sig, "tip", &tree, &[&parent])
            .unwrap();
        drop(parent);
        drop(tree);
        let patch = PatchBranch {
            tip,
            base,
            commits: vec![tip],
            paths: vec![RepoPath::new("a.txt").unwrap()],
            barrier: false,
        };
        (dir, repo, patch)
    }

    fn candidate(patch: &PatchBranch) -> Candidate<'_> {
        Candidate {
            volume: "vol",
            inbox: 2,
            patch,
            head: patch.base,
            merged: patch.base,
        }
    }

    #[test]
    fn a_clean_merge_is_accepted() {
        let (_d, repo, patch) = fixture();
        let rule = rule(
            AcceptMode::CleanMerge,
            Path::new("/nowhere"),
            Duration::ZERO,
        );
        assert_eq!(rule.name(), "cleanMerge");
        assert_eq!(
            rule.judge(&repo, &candidate(&patch)).unwrap(),
            Acceptance::Accept
        );
    }

    /// P4: the approval must name this very tip.
    #[test]
    fn approval_names_the_tip() {
        let (_d, repo, patch) = fixture();
        let rule = rule(AcceptMode::Approval, Path::new("/nowhere"), Duration::ZERO);
        assert_eq!(rule.name(), "approval");
        let awaiting = Acceptance::Await {
            what: AwaitWhat::Approval,
        };
        assert_eq!(rule.judge(&repo, &candidate(&patch)).unwrap(), awaiting);
        let name = approved_ref("vol", 2);
        repo.reference(&name, patch.base, true, "another tip")
            .unwrap();
        assert_eq!(rule.judge(&repo, &candidate(&patch)).unwrap(), awaiting);
        repo.reference(&name, patch.tip, true, "this tip").unwrap();
        assert_eq!(
            rule.judge(&repo, &candidate(&patch)).unwrap(),
            Acceptance::Accept
        );
    }

    #[test]
    fn the_hook_input_is_schema_1() {
        let (_d, _repo, patch) = fixture();
        let json = serde_json::to_value(CandidateJson::new(&candidate(&patch))).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "schema": 1,
                "volume": "vol",
                "inbox": 2,
                "tip": patch.tip.to_string(),
                "base": patch.base.to_string(),
                "head": patch.base.to_string(),
                "merged_tree": patch.base.to_string(),
                "commits": [patch.tip.to_string()],
                "paths": ["a.txt"],
                "barrier": false,
            })
        );
    }

    /// A hook that cannot start is a rule not working, never an
    /// acceptance.
    #[test]
    fn a_missing_hook_does_not_judge() {
        let (dir, repo, patch) = fixture();
        let rule = rule(AcceptMode::Command, dir.path(), Duration::from_secs(5));
        assert_eq!(rule.name(), "command");
        assert!(matches!(
            rule.judge(&repo, &candidate(&patch)),
            Err(RuleNotWorking::NotStarted {
                rule: "command",
                ..
            })
        ));
    }
}
