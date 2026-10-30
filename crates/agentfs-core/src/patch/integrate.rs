//! Merging a patch branch onto the branch head (design §12, §19.2 item
//! 11; bead agentfs-mia.4).
//!
//! A patch is a branch: a line of single-parent commits whose oldest
//! commit's parent is on the volume's branch (the head or an ancestor of
//! it), its **base**. A one-commit patch, today's only shape, is a branch
//! of one whose base is its parent. [`PatchBranch`] is what the queue
//! reads from the patch's tip; an [`Integrator`] merges it onto the head
//! and writes the merged tree's objects, nothing else: no ref, no file of
//! the backing tree, no journal record. The queue then lands the merged
//! tree exactly as it lands a patch today (the restore engine, one atomic
//! record, one commit carrying `Agentfs-Patch: <tip>` and `Agentfs-Inbox`).
//!
//! The one implementation in this bead, [`GitMergeTree`], runs the `git`
//! program (AGENTS.md principle 4, design §17.3):
//! `git merge-tree --write-tree --merge-base=<base> <head> <tip>` in the
//! state area's git directory, in the environment of [`git_cli::command`]
//! (an empty one, the repository's own config only), `-c core.fsync=objects`, so
//! the merged objects are durable before the record names them, and `-c
//! attr.tree=<the empty tree>`, so only the operator's
//! `$GIT_DIR/info/attributes` steers a merge (AGENTS.md principle 3; some
//! git versions, ubuntu-24.04's 2.43 among them, otherwise read a bare
//! repository's `HEAD:.gitattributes`; 2.47 does not). For a
//! one-commit patch that is the same three-way merge as libgit2's
//! cherry-pick against its parent, which the queue used before.
//!
//! Bead agentfs-mia.3 adds the rebase strategy, [`GitRebase`], and the one
//! place a volume's `agentfs.patchIntegrate` ([`IntegrateMode`]) becomes an
//! integrator, [`integrator`]. Both strategies merge with the same
//! `git merge-tree` run, so both honour the merge drivers the operator
//! names in `$GIT_DIR/info/attributes` (`merge=<name>`, the built-in
//! `union`, `text`, `binary`, or a `merge.<name>.driver` of the
//! repository's config: libgit2 cannot run those, design §17.3). A driver
//! that cannot run makes the merge not judged
//! ([`IntegrateError::DriverNotWorking`]), never a conflict: before the
//! merge, a configured driver whose program is not an executable file
//! ([`driver_program`]); during it, git's `failure to merge` (exit 128),
//! which is what git 2.43 and 2.47 say when a driver is killed or exits
//! above 128. A driver git can start that then exits 126 or 127 (its
//! shell found no program) reads to git as a conflict, and is one.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use git2::{FileMode, Oid, Repository};

use super::{BlockReason, MAX_PATCH_PATHS};
use crate::git_cli;
use crate::paths::RepoPath;

/// A patch as the queue reads it from its tip (see the module docs).
///
/// Built only by `PatchBranch::read` (bead agentfs-mia.4), which refuses
/// what the queue does not land: no merge base with the head
/// ([`BlockReason::ParentNotInHistory`]), a commit with more than one
/// parent between base and tip ([`BlockReason::MergeCommit`]), a root
/// commit or a gitlink or a path agentfs does not record in the diff
/// ([`BlockReason::Unsupported`]), more than
/// [`super::MAX_PATCH_PATHS`] paths ([`BlockReason::TooLarge`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchBranch {
    /// The commit the inbox ref names.
    pub tip: Oid,
    /// The merge base of the tip and the head the branch was read
    /// against: the last commit of the volume's branch the patch builds
    /// on.
    pub base: Oid,
    /// The patch's own commits, base exclusive, tip inclusive, oldest
    /// first; never empty.
    pub commits: Vec<Oid>,
    /// The paths the diff from `base` to `tip` changes, sorted, each once.
    pub paths: Vec<RepoPath>,
    /// The tip's last message paragraph has `Agentfs-Barrier: yes`
    /// ([`is_barrier`]; bead agentfs-mia.2).
    pub barrier: bool,
}

/// Why a patch branch is not one the queue lands: the reason, a message
/// for the status and the alarm, and the paths involved, if any.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub reason: BlockReason,
    pub message: String,
    pub paths: Vec<String>,
}

/// What merging a patch onto the head gave.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Integrated {
    /// A clean merge: the merged tree, its objects written and durable.
    Clean { tree: Oid },
    /// The merge conflicts in these paths (sorted, each once). Nothing a
    /// conflict wrote is used.
    Conflict { paths: Vec<String> },
}

/// Why a merge could not be judged at all (never a conflict).
#[derive(Debug, thiserror::Error)]
pub enum IntegrateError {
    /// The `git` program could not be started.
    #[error("running git: {0}")]
    Spawn(#[source] std::io::Error),
    /// `git` exited with a status other than clean or conflicted.
    #[error("`git {args}` failed ({status}): {stderr}")]
    Failed {
        args: String,
        status: String,
        stderr: String,
    },
    /// `git` succeeded but its output is not what this build reads.
    #[error("reading the output of `git {args}`: {detail}")]
    BadOutput { args: String, detail: String },
    /// A merge driver the operator configured could not run (not found,
    /// not executable, killed): the merge is not judged. `path` is empty
    /// when the path is not known (the check before the merge, and git's
    /// `failure to merge`, which names none).
    #[error("the merge driver {driver}{} could not run: {detail}", for_path(path))]
    DriverNotWorking {
        driver: String,
        path: String,
        detail: String,
    },
    #[error(transparent)]
    Git(#[from] git2::Error),
}

fn for_path(path: &str) -> String {
    if path.is_empty() {
        String::new()
    } else {
        format!(" for {path}")
    }
}

/// `agentfs.patchIntegrate`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum IntegrateMode {
    /// `merge`, the default: [`GitMergeTree`].
    #[default]
    Merge,
    /// `rebase`: [`GitRebase`].
    Rebase,
}

/// The one place a mode becomes an integrator, on the state area's git
/// directory `git_dir`.
pub fn integrator(mode: IntegrateMode, git_dir: &Path) -> Box<dyn Integrator> {
    match mode {
        IntegrateMode::Merge => Box::new(GitMergeTree::new(git_dir)),
        IntegrateMode::Rebase => Box::new(GitRebase::new(git_dir)),
    }
}

/// Merges a patch branch onto a branch head (see the module docs).
///
/// Invariants every implementation keeps: it writes objects only (never a
/// ref, the backing tree or the journal); it is deterministic for the same
/// head, patch and repository config, so a run after a crash merges to the
/// same tree; a conflict is [`Integrated::Conflict`], never an error.
pub trait Integrator {
    /// Merges `patch` onto `head` (a commit of the volume's branch, of
    /// which `patch.base` is the head or an ancestor).
    fn integrate(&self, head: Oid, patch: &PatchBranch) -> Result<Integrated, IntegrateError>;
}

/// The merge strategy: `git merge-tree --write-tree` against the patch's
/// base (see the module docs).
#[derive(Debug, Clone)]
pub struct GitMergeTree {
    /// The `git` program, looked up in [`git_cli::GIT_PATH`].
    git: PathBuf,
    /// The state area's git directory.
    git_dir: PathBuf,
}

/// Rebase: each commit of the patch replayed onto the head in order
/// (`git merge-tree --write-tree --merge-base=<c^> <cur> <c>`, `cur` a
/// throwaway `git commit-tree` of the previous result, never referenced);
/// the first commit that conflicts is the conflict (its paths). The result
/// tree lands as one record, like Merge.
#[derive(Debug, Clone)]
pub struct GitRebase {
    /// The `git` program, looked up in [`git_cli::GIT_PATH`].
    git: PathBuf,
    /// The state area's git directory.
    git_dir: PathBuf,
}

/// The arguments after `git`, before the merge's own.
///
/// `attr.tree` is the empty tree: some git versions (2.43, for one) read a
/// bare repository's attributes from `HEAD:.gitattributes` by default,
/// which is the workload's tree; only `$GIT_DIR/info/attributes` (the operator's,
/// AGENTS.md principle 3) may steer a merge. The command line wins over
/// the repository's config, so the override holds whatever git's default;
/// `the_heads_attributes_never_steer_a_merge` checks it on any git by
/// setting `attr.tree = HEAD` there.
pub const GIT_MERGE_TREE_ARGS: [&str; 6] = [
    "-c",
    "core.fsync=objects",
    "-c",
    "attr.tree=4b825dc642cb6eb9a060e54bf8d69288fbee4904",
    "merge-tree",
    "--write-tree",
];

/// The oldest git whose `merge-tree --write-tree` takes `--merge-base`.
const MIN_GIT: (u32, u32) = (2, 40);

impl PatchBranch {
    /// Reads the patch whose tip is `tip` against the branch head `head`:
    /// its base is the merge base of the two, and the tip's first-parent
    /// line back to the base must be single-parent commits. Refusals, in
    /// this order: the tip is not a commit, or a root commit
    /// ([`BlockReason::Unsupported`]); no merge base
    /// ([`BlockReason::ParentNotInHistory`]); the tip is the head or an
    /// ancestor of it (`Unsupported`); a commit with two or more parents
    /// ([`BlockReason::MergeCommit`]); more than [`MAX_PATCH_PATHS`] paths
    /// changed from base to tip ([`BlockReason::TooLarge`]); a gitlink or
    /// a path agentfs does not record (`Unsupported`). A repository that
    /// cannot be read is [`BlockReason::Failed`].
    pub fn read(repo: &Repository, head: Oid, tip: Oid) -> Result<PatchBranch, Refusal> {
        let Ok(tip_commit) = repo.find_commit(tip) else {
            return Err(refusal(
                BlockReason::Unsupported,
                format!("{tip} is not a commit"),
            ));
        };
        if tip_commit.parent_count() == 0 {
            return Err(root_commit());
        }
        let base = match repo.merge_base(head, tip) {
            Ok(base) => base,
            Err(e) if e.code() == git2::ErrorCode::NotFound => {
                return Err(refusal(
                    BlockReason::ParentNotInHistory,
                    format!("the patch's history does not meet the branch head {head}"),
                ));
            }
            Err(e) => return Err(failed(&e)),
        };
        if base == tip {
            return Err(refusal(
                BlockReason::Unsupported,
                format!("the patch's tip {tip} is already on the branch"),
            ));
        }
        let barrier = is_barrier(&tip_commit);
        let mut commits = Vec::new();
        let mut commit = tip_commit;
        while commit.id() != base {
            match commit.parent_count() {
                1 => {}
                0 => return Err(root_commit()),
                parents => {
                    return Err(refusal(
                        BlockReason::MergeCommit,
                        format!(
                            "{} in the patch has {parents} parents; a patch is a line of \
                             single-parent commits",
                            commit.id()
                        ),
                    ));
                }
            }
            commits.push(commit.id());
            commit = commit.parent(0).map_err(|e| failed(&e))?;
        }
        commits.reverse();
        let paths = diff_paths(repo, base, tip)?;
        Ok(PatchBranch {
            tip,
            base,
            commits,
            paths,
            barrier,
        })
    }
}

/// Whether `commit` makes its patch a barrier ([`message_is_barrier`]).
pub fn is_barrier(commit: &git2::Commit<'_>) -> bool {
    message_is_barrier(&String::from_utf8_lossy(commit.message_raw_bytes()))
}

/// Whether a patch tip's commit message makes it a barrier (design §12,
/// bead agentfs-mia.2): it has more than one paragraph and the last one
/// has a line `Agentfs-Barrier: yes` (the key in any case, as git reads
/// trailer keys).
pub fn message_is_barrier(message: &str) -> bool {
    let paragraphs: Vec<&str> = message
        .trim_end()
        .split("\n\n")
        .filter(|p| !p.trim().is_empty())
        .collect();
    if paragraphs.len() < 2 {
        return false;
    }
    paragraphs.last().is_some_and(|last| {
        last.lines().any(|line| {
            line.split_once(':').is_some_and(|(key, value)| {
                key.trim().eq_ignore_ascii_case(super::BARRIER_TRAILER) && value.trim() == "yes"
            })
        })
    })
}

fn refusal(reason: BlockReason, message: String) -> Refusal {
    Refusal {
        reason,
        message,
        paths: Vec::new(),
    }
}

fn root_commit() -> Refusal {
    refusal(
        BlockReason::Unsupported,
        "the patch is a root commit: it needs the branch commit it was written against as its \
         parent"
            .into(),
    )
}

fn failed(e: &git2::Error) -> Refusal {
    refusal(BlockReason::Failed, format!("reading the patch: {e}"))
}

/// The paths the diff from `base` to `tip` changes, or why the queue does
/// not land it: too many paths, a gitlink, or a path agentfs cannot write.
fn diff_paths(repo: &Repository, base: Oid, tip: Oid) -> Result<Vec<RepoPath>, Refusal> {
    let tree = |oid| repo.find_commit(oid).and_then(|c| c.tree());
    let before = tree(base).map_err(|e| failed(&e))?;
    let after = tree(tip).map_err(|e| failed(&e))?;
    let diff = repo
        .diff_tree_to_tree(Some(&before), Some(&after), None)
        .map_err(|e| failed(&e))?;
    let count = diff.deltas().len();
    if count > MAX_PATCH_PATHS {
        return Err(refusal(
            BlockReason::TooLarge,
            format!("the patch changes {count} paths; at most {MAX_PATCH_PATHS} are applied"),
        ));
    }
    let mut paths = BTreeSet::new();
    for delta in diff.deltas() {
        for file in [delta.old_file(), delta.new_file()] {
            let Some(path) = file.path_bytes() else {
                continue;
            };
            let path = String::from_utf8_lossy(path).into_owned();
            if file.mode() == FileMode::Commit {
                return Err(Refusal {
                    reason: BlockReason::Unsupported,
                    message: format!(
                        "{path} is a gitlink (submodule); agentfs never records those"
                    ),
                    paths: vec![path],
                });
            }
            match RepoPath::new(path.clone()) {
                Ok(p) => {
                    paths.insert(p);
                }
                Err(e) => {
                    return Err(Refusal {
                        reason: BlockReason::Unsupported,
                        message: format!("{path:?} is not a path agentfs records: {e}"),
                        paths: vec![path],
                    });
                }
            }
        }
    }
    Ok(paths.into_iter().collect())
}

impl GitMergeTree {
    /// `git` looked up in [`git_cli::GIT_PATH`]; `git_dir` the state
    /// area's git directory. With no `git` there, every merge fails to
    /// start ([`IntegrateError::Spawn`]).
    pub fn new(git_dir: impl Into<PathBuf>) -> Self {
        Self {
            git: git_cli::find_git().unwrap_or_else(|| PathBuf::from("git")),
            git_dir: git_dir.into(),
        }
    }
}

impl Integrator for GitMergeTree {
    /// The drivers checked ([`check_drivers`]), then one [`merge_tree`]
    /// of the tip onto the head against the patch's base. Refreshing the
    /// caller's object database is the caller's.
    fn integrate(&self, head: Oid, patch: &PatchBranch) -> Result<Integrated, IntegrateError> {
        let drivers = check_drivers(&self.git_dir)?;
        merge_tree(
            &self.git,
            &self.git_dir,
            &drivers,
            patch.base,
            head,
            patch.tip,
        )
    }
}

impl GitRebase {
    /// As [`GitMergeTree::new`].
    pub fn new(git_dir: impl Into<PathBuf>) -> Self {
        Self {
            git: git_cli::find_git().unwrap_or_else(|| PathBuf::from("git")),
            git_dir: git_dir.into(),
        }
    }

    /// A throwaway commit of `tree` on `parent`, for the next step's
    /// `<cur>` (merge-tree merges commits): a fixed identity and date, so
    /// a run after a crash writes the same object. Nothing references it.
    fn step_commit(&self, tree: Oid, parent: Oid) -> Result<Oid, IntegrateError> {
        let args = [
            "-c".to_owned(),
            "core.fsync=objects".to_owned(),
            "commit-tree".to_owned(),
            tree.to_string(),
            "-p".to_owned(),
            parent.to_string(),
            "-m".to_owned(),
            "agentfs rebase step".to_owned(),
        ];
        let out = git_cli::command(&self.git, &self.git_dir)
            .args(&args)
            .envs([
                ("GIT_AUTHOR_NAME", "agentfs"),
                ("GIT_AUTHOR_EMAIL", "agentfs@localhost"),
                ("GIT_AUTHOR_DATE", "@0 +0000"),
                ("GIT_COMMITTER_NAME", "agentfs"),
                ("GIT_COMMITTER_EMAIL", "agentfs@localhost"),
                ("GIT_COMMITTER_DATE", "@0 +0000"),
            ])
            .output()
            .map_err(IntegrateError::Spawn)?;
        let shown = args.join(" ");
        if !out.status.success() {
            return Err(IntegrateError::Failed {
                args: shown,
                status: out.status.to_string(),
                stderr: String::from_utf8_lossy(&out.stderr).trim().to_owned(),
            });
        }
        let text = String::from_utf8_lossy(&out.stdout);
        Oid::from_str(text.trim()).map_err(|_| IntegrateError::BadOutput {
            args: shown,
            detail: format!("{:?} is not a commit id", text.trim()),
        })
    }
}

impl Integrator for GitRebase {
    /// The drivers checked ([`check_drivers`]), then one [`merge_tree`]
    /// per commit of the patch, oldest first, each against its parent,
    /// onto the head and then onto a step commit of the previous result:
    /// the last result, or the first conflict.
    fn integrate(&self, head: Oid, patch: &PatchBranch) -> Result<Integrated, IntegrateError> {
        let drivers = check_drivers(&self.git_dir)?;
        let mut cur = head;
        let mut parent = patch.base;
        let mut last = None;
        for commit in &patch.commits {
            if let Some(tree) = last {
                cur = self.step_commit(tree, cur)?;
            }
            match merge_tree(&self.git, &self.git_dir, &drivers, parent, cur, *commit)? {
                Integrated::Clean { tree } => last = Some(tree),
                conflict => return Ok(conflict),
            }
            parent = *commit;
        }
        last.map(|tree| Integrated::Clean { tree })
            .ok_or_else(|| IntegrateError::BadOutput {
                args: "merge-tree".into(),
                detail: "the patch has no commits".into(),
            })
    }
}

/// The merge drivers of the repository's config (`merge.<name>.driver`,
/// the file `$GIT_DIR/config` and what it includes: what git reads in the
/// environment of [`git_cli::command`]), as (name, command), in config
/// order. Fails, [`IntegrateError::DriverNotWorking`], at the first whose
/// program is not an executable file ([`driver_program`]).
fn check_drivers(git_dir: &Path) -> Result<Vec<(String, String)>, IntegrateError> {
    let config = git2::Config::open(&git_dir.join("config"))?;
    let mut drivers = Vec::new();
    let mut entries = config.entries(Some(r"^merge\..*\.driver$"))?;
    while let Some(entry) = entries.next() {
        let entry = entry?;
        let (Ok(key), Ok(command)) = (entry.name(), entry.value()) else {
            continue;
        };
        let Some(name) = key
            .strip_prefix("merge.")
            .and_then(|k| k.strip_suffix(".driver"))
        else {
            continue;
        };
        drivers.push((name.to_owned(), command.to_owned()));
    }
    for (name, command) in &drivers {
        if let Err(detail) = driver_program(command, git_dir) {
            return Err(IntegrateError::DriverNotWorking {
                driver: name.clone(),
                path: String::new(),
                detail,
            });
        }
    }
    Ok(drivers)
}

/// The program a driver's command runs: its first word, a path when it
/// has a `/` (relative ones from `git_dir`, where git runs), else looked
/// up in [`git_cli::GIT_PATH`]; it must be an executable file. A shell
/// word that is no file (a builtin, an assignment) does not pass.
pub fn driver_program(command: &str, git_dir: &Path) -> Result<PathBuf, String> {
    use std::os::unix::fs::PermissionsExt as _;
    let Some(word) = command.split_whitespace().next() else {
        return Err("its command is empty".into());
    };
    let executable = |path: &Path| {
        std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    };
    if word.contains('/') {
        let path = git_dir.join(word);
        return if executable(&path) {
            Ok(path)
        } else {
            Err(format!("{word} is not an executable file"))
        };
    }
    git_cli::GIT_PATH
        .split(':')
        .map(|dir| Path::new(dir).join(word))
        .find(|path| executable(path))
        .ok_or_else(|| format!("{word} is not a program in {}", git_cli::GIT_PATH))
}

/// The arguments after `git` that merge `theirs` onto `ours` against
/// `base`.
fn merge_args(base: Oid, ours: Oid, theirs: Oid) -> Vec<String> {
    let mut args: Vec<String> = GIT_MERGE_TREE_ARGS.iter().map(|a| a.to_string()).collect();
    args.extend([
        "-z".to_owned(),
        "--name-only".to_owned(),
        "--no-messages".to_owned(),
        format!("--merge-base={base}"),
        ours.to_string(),
        theirs.to_string(),
    ]);
    args
}

/// `git --version` as (major, minor), when it can be read.
fn git_version(git: &Path, git_dir: &Path) -> Option<(u32, u32)> {
    let out = git_cli::command(git, git_dir)
        .arg("--version")
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut numbers = text.split_whitespace().nth(2)?.split('.');
    Some((numbers.next()?.parse().ok()?, numbers.next()?.parse().ok()?))
}

/// git's message when a merge driver is killed or exits above 128 (2.43
/// and 2.47 alike, from `merge-tree`'s die with exit 128; 2.43 first
/// prints `error: failed to execute internal merge`).
const FAILURE_TO_MERGE: &str = "fatal: failure to merge";

/// `git <GIT_MERGE_TREE_ARGS> -z --name-only --no-messages
/// --merge-base=<base> <ours> <theirs>`: exit 0 is a clean merge (the
/// first NUL-terminated field is the tree), exit 1 a conflict (the tree,
/// then the conflicted paths up to an empty field); exit 128 with git's
/// [`FAILURE_TO_MERGE`] while `drivers` is not empty is
/// [`IntegrateError::DriverNotWorking`] (the drivers named, no path);
/// anything else is [`IntegrateError::Failed`], output it cannot read
/// [`IntegrateError::BadOutput`].
fn merge_tree(
    git: &Path,
    git_dir: &Path,
    drivers: &[(String, String)],
    base: Oid,
    ours: Oid,
    theirs: Oid,
) -> Result<Integrated, IntegrateError> {
    let args = merge_args(base, ours, theirs);
    let out = git_cli::command(git, git_dir)
        .args(&args)
        .output()
        .map_err(IntegrateError::Spawn)?;
    let shown = args.join(" ");
    let conflicted = match out.status.code() {
        Some(0) => false,
        Some(1) => true,
        code => {
            let mut stderr = String::from_utf8_lossy(&out.stderr).trim().to_owned();
            if code == Some(128) && stderr.contains(FAILURE_TO_MERGE) && !drivers.is_empty() {
                let names: Vec<&str> = drivers.iter().map(|(n, _)| n.as_str()).collect();
                return Err(IntegrateError::DriverNotWorking {
                    driver: names.join(", "),
                    path: String::new(),
                    detail: stderr,
                });
            }
            if let Some((major, minor)) = git_version(git, git_dir)
                && (major, minor) < MIN_GIT
            {
                stderr.push_str(&format!(
                    " (this is git {major}.{minor}; merging a patch needs git {}.{} or later)",
                    MIN_GIT.0, MIN_GIT.1
                ));
            }
            return Err(IntegrateError::Failed {
                args: shown,
                status: out.status.to_string(),
                stderr,
            });
        }
    };
    let bad = |detail: String| IntegrateError::BadOutput {
        args: shown.clone(),
        detail,
    };
    let stdout =
        String::from_utf8(out.stdout).map_err(|_| bad("the output is not UTF-8".into()))?;
    let mut fields = stdout.split('\0');
    let first = fields.next().unwrap_or_default();
    let tree =
        Oid::from_str(first.trim()).map_err(|_| bad(format!("{first:?} is not a tree id")))?;
    if !conflicted {
        return Ok(Integrated::Clean { tree });
    }
    let paths: BTreeSet<String> = fields
        .take_while(|f| !f.is_empty())
        .map(str::to_owned)
        .collect();
    Ok(Integrated::Conflict {
        paths: paths.into_iter().collect(),
    })
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::Path;

    use git2::Signature;

    use super::*;

    fn have_git() -> bool {
        let found = git_cli::find_git().is_some();
        if !found {
            eprintln!("skipping: git is not installed");
        }
        found
    }

    /// A bare repository and a commit helper: `files` written on `parent`
    /// (`None` removes).
    struct Repo {
        dir: tempfile::TempDir,
        repo: Repository,
    }

    impl Repo {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let repo = Repository::init_bare(dir.path()).unwrap();
            Self { dir, repo }
        }

        fn commit(&self, parents: &[Oid], files: &[(&str, Option<&str>)], message: &str) -> Oid {
            let repo = &self.repo;
            let mut index = git2::Index::new().unwrap();
            if let Some(first) = parents.first() {
                index
                    .read_tree(&repo.find_commit(*first).unwrap().tree().unwrap())
                    .unwrap();
            }
            for (path, content) in files {
                match content {
                    Some(content) => {
                        let id = repo.blob(content.as_bytes()).unwrap();
                        index.add(&entry(path, id, 0o100644)).unwrap();
                    }
                    None => index.remove_path(Path::new(path)).unwrap(),
                }
            }
            let tree = repo.find_tree(index.write_tree_to(repo).unwrap()).unwrap();
            let sig = Signature::now("Ada", "ada@example.com").unwrap();
            let parents: Vec<git2::Commit<'_>> = parents
                .iter()
                .map(|p| repo.find_commit(*p).unwrap())
                .collect();
            let parents: Vec<&git2::Commit<'_>> = parents.iter().collect();
            repo.commit(None, &sig, &sig, message, &tree, &parents)
                .unwrap()
        }

        fn merge(&self) -> GitMergeTree {
            GitMergeTree::new(self.dir.path())
        }

        fn content(&self, tree: Oid, path: &str) -> Option<String> {
            let tree = self.repo.find_tree(tree).ok()?;
            let entry = tree.get_path(Path::new(path)).ok()?;
            let blob = self.repo.find_blob(entry.id()).ok()?;
            Some(String::from_utf8_lossy(blob.content()).into_owned())
        }
    }

    /// Runs `f`, again while a just-written script is busy (`ETXTBSY`: a
    /// process forked by another test still holds it open for writing).
    fn retry_busy(
        f: impl Fn() -> Result<Integrated, IntegrateError>,
    ) -> Result<Integrated, IntegrateError> {
        for _ in 0..50 {
            match f() {
                Err(IntegrateError::Spawn(e)) if e.raw_os_error() == Some(26) => {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                other => return other,
            }
        }
        f()
    }

    /// An executable script `name` in `dir`.
    fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn entry(path: &str, id: Oid, mode: u32) -> git2::IndexEntry {
        git2::IndexEntry {
            ctime: git2::IndexTime::new(0, 0),
            mtime: git2::IndexTime::new(0, 0),
            dev: 0,
            ino: 0,
            mode,
            uid: 0,
            gid: 0,
            file_size: 0,
            id,
            flags: 0,
            flags_extended: 0,
            path: path.as_bytes().to_vec(),
        }
    }

    #[test]
    fn a_branch_is_read_back_to_its_base() {
        let r = Repo::new();
        let base = r.commit(&[], &[("f", Some("0\n"))], "seed");
        let head = r.commit(&[base], &[("g", Some("g\n"))], "on the branch");
        let a = r.commit(&[base], &[("a", Some("a\n"))], "a");
        let b = r.commit(&[a], &[("b", Some("b\n")), ("f", None)], "b");
        let branch = PatchBranch::read(&r.repo, head, b).unwrap();
        assert_eq!(branch.tip, b);
        assert_eq!(branch.base, base);
        assert_eq!(branch.commits, [a, b]);
        let paths: Vec<&str> = branch.paths.iter().map(RepoPath::as_str).collect();
        assert_eq!(paths, ["a", "b", "f"]);
        assert!(!branch.barrier);
        let barrier = r.commit(&[b], &[("c", Some("c\n"))], "c\n\nAgentfs-Barrier: yes\n");
        assert!(PatchBranch::read(&r.repo, head, barrier).unwrap().barrier);
        // Only in the last paragraph, and not as the subject.
        for message in [
            "Agentfs-Barrier: yes\n",
            "c\n\nAgentfs-Barrier: yes\n\nmore\n",
            "c\n\nAgentfs-Barrier: no\n",
        ] {
            let tip = r.commit(&[b], &[("c", Some("c\n"))], message);
            assert!(
                !PatchBranch::read(&r.repo, head, tip).unwrap().barrier,
                "{message:?}"
            );
        }
    }

    #[test]
    fn what_is_not_a_patch_is_refused() {
        let r = Repo::new();
        let base = r.commit(&[], &[("f", Some("0\n"))], "seed");
        let head = r.commit(&[base], &[("g", Some("g\n"))], "on the branch");
        let reason = |tip| PatchBranch::read(&r.repo, head, tip).unwrap_err();
        let blob = r.repo.blob(b"x").unwrap();
        let not = reason(blob);
        assert_eq!(not.reason, BlockReason::Unsupported);
        assert_eq!(not.message, format!("{blob} is not a commit"));
        assert_eq!(reason(base).reason, BlockReason::Unsupported);
        let on = reason(head);
        assert_eq!(on.reason, BlockReason::Unsupported);
        assert!(
            on.message.contains("already on the branch"),
            "{}",
            on.message
        );
        let foreign_root = r.commit(&[], &[("x", Some("x\n"))], "root");
        assert_eq!(reason(foreign_root).reason, BlockReason::Unsupported);
        let foreign = r.commit(&[foreign_root], &[("y", Some("y\n"))], "foreign");
        let refused = reason(foreign);
        assert_eq!(refused.reason, BlockReason::ParentNotInHistory);
        assert!(
            refused.message.contains(&head.to_string()),
            "{}",
            refused.message
        );
        // A merge inside the branch, not only at its tip.
        let side = r.commit(&[base], &[("s", Some("s\n"))], "side");
        let a = r.commit(&[base], &[("a", Some("a\n"))], "a");
        let merge = r.commit(&[a, side], &[("s", Some("s\n"))], "merge");
        let on_top = r.commit(&[merge], &[("t", Some("t\n"))], "on top");
        let refused = reason(on_top);
        assert_eq!(refused.reason, BlockReason::MergeCommit);
        assert_eq!(
            refused.message,
            format!(
                "{merge} in the patch has 2 parents; a patch is a line of single-parent commits"
            )
        );
    }

    #[test]
    fn a_clean_merge_is_against_the_base() {
        if !have_git() {
            return;
        }
        let r = Repo::new();
        let base = r.commit(&[], &[("f", Some("0\n")), ("g", Some("0\n"))], "seed");
        let head = r.commit(&[base], &[("g", Some("head\n"))], "on the branch");
        let a = r.commit(&[base], &[("a", Some("a\n"))], "a");
        let b = r.commit(&[a], &[("f", Some("patched\n"))], "b");
        let branch = PatchBranch::read(&r.repo, head, b).unwrap();
        let Integrated::Clean { tree } = r.merge().integrate(head, &branch).unwrap() else {
            panic!("not clean");
        };
        r.repo.odb().unwrap().refresh().unwrap();
        assert_eq!(r.content(tree, "a").as_deref(), Some("a\n"));
        assert_eq!(r.content(tree, "f").as_deref(), Some("patched\n"));
        assert_eq!(r.content(tree, "g").as_deref(), Some("head\n"));
    }

    #[test]
    fn a_conflict_lists_its_paths() {
        if !have_git() {
            return;
        }
        let r = Repo::new();
        let base = r.commit(&[], &[("f", Some("0\n")), ("h", Some("0\n"))], "seed");
        let head = r.commit(
            &[base],
            &[("f", Some("head\n")), ("h", Some("head\n"))],
            "on the branch",
        );
        let a = r.commit(&[base], &[("h", Some("patch\n"))], "a");
        let b = r.commit(&[a], &[("f", Some("patch\n")), ("ok", Some("x\n"))], "b");
        let branch = PatchBranch::read(&r.repo, head, b).unwrap();
        assert_eq!(
            r.merge().integrate(head, &branch).unwrap(),
            Integrated::Conflict {
                paths: vec!["f".into(), "h".into()]
            }
        );
    }

    /// The merge's objects are durable and only the operator's attributes
    /// steer it: the argv says so (P4; durability cannot be tested in
    /// process), and git runs in the environment of `git_cli`.
    #[test]
    fn git_runs_durable_in_an_environment_of_its_own() {
        let r = Repo::new();
        let dir = tempfile::tempdir().unwrap();
        let git = script(
            dir.path(),
            "git",
            &format!(
                "printf '%s\\n' \"$@\" > {0}/args\nenv > {0}/env\nprintf \
                 '4b825dc642cb6eb9a060e54bf8d69288fbee4904\\0'",
                dir.path().display()
            ),
        );
        let merge = GitMergeTree {
            git,
            git_dir: r.dir.path().to_owned(),
        };
        let base = r.commit(&[], &[("f", Some("0\n"))], "seed");
        let head = r.commit(&[base], &[("g", Some("g\n"))], "head");
        let tip = r.commit(&[base], &[("a", Some("a\n"))], "a");
        let branch = PatchBranch::read(&r.repo, head, tip).unwrap();
        assert_eq!(
            retry_busy(|| merge.integrate(head, &branch)).unwrap(),
            Integrated::Clean {
                tree: Oid::from_str("4b825dc642cb6eb9a060e54bf8d69288fbee4904").unwrap()
            }
        );
        let args = std::fs::read_to_string(dir.path().join("args")).unwrap();
        let args: Vec<&str> = args.lines().collect();
        let expected: Vec<String> = [
            "-c",
            "core.fsync=objects",
            "-c",
            "attr.tree=4b825dc642cb6eb9a060e54bf8d69288fbee4904",
            "merge-tree",
            "--write-tree",
            "-z",
            "--name-only",
            "--no-messages",
        ]
        .iter()
        .map(|s| s.to_string())
        .chain([
            format!("--merge-base={base}"),
            head.to_string(),
            tip.to_string(),
        ])
        .collect();
        assert_eq!(args, expected);
        let env = std::fs::read_to_string(dir.path().join("env")).unwrap();
        assert!(
            env.contains(&format!("PATH={}\n", git_cli::GIT_PATH)),
            "{env}"
        );
        assert!(env.contains("GIT_CONFIG_GLOBAL=/dev/null\n"), "{env}");
        assert!(!env.contains("GIT_ATTR_SOURCE"), "{env}");
    }

    impl Repo {
        fn rebase(&self) -> GitRebase {
            GitRebase::new(self.dir.path())
        }

        /// `merge.<name>.driver = command` and `info/attributes` holding
        /// `attributes`.
        fn driver(&self, name: &str, command: &str, attributes: &str) {
            let mut config = git2::Config::open(&self.dir.path().join("config")).unwrap();
            config
                .set_str(&format!("merge.{name}.driver"), command)
                .unwrap();
            let info = self.dir.path().join("info");
            std::fs::create_dir_all(&info).unwrap();
            std::fs::write(info.join("attributes"), attributes).unwrap();
        }

        /// A patch whose first commit edits `f` against the head's edit
        /// and whose second puts `f` back and adds `g`.
        fn edit_and_revert(&self) -> (Oid, PatchBranch) {
            let base = self.commit(&[], &[("f", Some("0\n"))], "seed");
            let head = self.commit(&[base], &[("f", Some("head\n"))], "on the branch");
            let a = self.commit(&[base], &[("f", Some("patch\n"))], "edit f");
            let b = self.commit(
                &[a],
                &[("f", Some("0\n")), ("g", Some("g\n"))],
                "revert, add g",
            );
            (head, PatchBranch::read(&self.repo, head, b).unwrap())
        }

        /// A one-commit patch editing `f.txt` against the head's edit.
        fn both_edit(&self) -> (Oid, PatchBranch) {
            let base = self.commit(&[], &[("f.txt", Some("0\n"))], "seed");
            let head = self.commit(&[base], &[("f.txt", Some("head\n"))], "on the branch");
            let tip = self.commit(&[base], &[("f.txt", Some("patch\n"))], "edit f");
            (head, PatchBranch::read(&self.repo, head, tip).unwrap())
        }
    }

    /// Acceptance 1 in the core: a first commit that conflicts with the
    /// head and a second that reverts it merge cleanly as one diff, but
    /// not commit by commit.
    #[test]
    fn rebase_replays_commit_by_commit() {
        if !have_git() {
            return;
        }
        let r = Repo::new();
        let (head, branch) = r.edit_and_revert();
        let Integrated::Clean { tree } = r.merge().integrate(head, &branch).unwrap() else {
            panic!("merge is not clean");
        };
        r.repo.odb().unwrap().refresh().unwrap();
        assert_eq!(r.content(tree, "f").as_deref(), Some("head\n"));
        assert_eq!(
            r.rebase().integrate(head, &branch).unwrap(),
            Integrated::Conflict {
                paths: vec!["f".into()]
            }
        );
        let by_mode = |mode| {
            integrator(mode, r.dir.path())
                .integrate(head, &branch)
                .unwrap()
        };
        assert!(matches!(
            by_mode(IntegrateMode::Merge),
            Integrated::Clean { .. }
        ));
        assert!(matches!(
            by_mode(IntegrateMode::Rebase),
            Integrated::Conflict { .. }
        ));
        assert_eq!(IntegrateMode::default(), IntegrateMode::Merge);
    }

    /// A clean rebase gives the tree a merge gives, every commit replayed
    /// onto the last (the second commit edits what the first added).
    #[test]
    fn a_clean_rebase_replays_every_commit() {
        if !have_git() {
            return;
        }
        let r = Repo::new();
        let base = r.commit(&[], &[("f", Some("0\n")), ("g", Some("0\n"))], "seed");
        let head = r.commit(&[base], &[("g", Some("head\n"))], "on the branch");
        let a = r.commit(&[base], &[("a", Some("a\n"))], "add a");
        let b = r.commit(&[a], &[("a", Some("a, again\n"))], "edit a");
        let c = r.commit(&[b], &[("f", Some("patched\n"))], "edit f");
        let branch = PatchBranch::read(&r.repo, head, c).unwrap();
        let Integrated::Clean { tree } = r.rebase().integrate(head, &branch).unwrap() else {
            panic!("not clean");
        };
        assert_eq!(
            r.merge().integrate(head, &branch).unwrap(),
            Integrated::Clean { tree }
        );
        r.repo.odb().unwrap().refresh().unwrap();
        assert_eq!(r.content(tree, "a").as_deref(), Some("a, again\n"));
        assert_eq!(r.content(tree, "f").as_deref(), Some("patched\n"));
        assert_eq!(r.content(tree, "g").as_deref(), Some("head\n"));
        // A run after a crash merges to the same tree.
        assert_eq!(
            r.rebase().integrate(head, &branch).unwrap(),
            Integrated::Clean { tree }
        );
    }

    /// The operator's drivers run in both strategies; one that cannot
    /// run is not a conflict but [`IntegrateError::DriverNotWorking`]:
    /// missing, not executable, or killed (git's own wording, checked
    /// against the git installed here); one that exits 1 is a conflict.
    #[test]
    fn merge_drivers_run_or_are_not_working() {
        if !have_git() {
            return;
        }
        let scripts = tempfile::tempdir().unwrap();
        let write = |name: &str, body: &str| {
            let path = scripts.path().join(name);
            std::fs::write(&path, format!("{body}\n")).unwrap();
            path.display().to_string()
        };
        // Run with `sh <script>`: no exec of a file just written (ETXTBSY).
        let ours = write("ours.sh", "exit 0");
        let conflict = write("conflict.sh", "exit 1");
        let killed = write("killed.sh", "kill -9 $$");
        let noexec = write("noexec", "exit 0");
        for rebase in [false, true] {
            let run = |command: &str| {
                let r = Repo::new();
                r.driver("d", command, "*.txt merge=d\n");
                let (head, branch) = r.both_edit();
                let result = if rebase {
                    r.rebase().integrate(head, &branch)
                } else {
                    r.merge().integrate(head, &branch)
                };
                r.repo.odb().unwrap().refresh().unwrap();
                (r, result)
            };
            let (r, clean) = run(&format!("sh {ours} %O %A %B"));
            let Ok(Integrated::Clean { tree }) = clean else {
                panic!("{clean:?}");
            };
            assert_eq!(r.content(tree, "f.txt").as_deref(), Some("head\n"));
            assert_eq!(
                run(&format!("sh {conflict} %O %A %B")).1.unwrap(),
                Integrated::Conflict {
                    paths: vec!["f.txt".into()]
                }
            );
            for command in [
                "agentfs-no-such-driver %O %A %B".to_owned(),
                format!("{noexec} %O %A %B"),
                String::new(),
                format!("sh {killed} %O %A %B"),
            ] {
                match run(&command).1 {
                    Err(e @ IntegrateError::DriverNotWorking { .. }) => {
                        if let IntegrateError::DriverNotWorking { driver, path, .. } = &e {
                            assert_eq!(driver, "d");
                            assert_eq!(path, "");
                        }
                        assert!(
                            e.to_string()
                                .starts_with("the merge driver d could not run: "),
                            "{e}"
                        );
                    }
                    other => panic!("{command:?} (rebase {rebase}): {other:?}"),
                }
            }
        }
    }

    /// The workload's own `.gitattributes` never steers a merge, on any
    /// git: the repository's config sets `attr.tree = HEAD`, so every git
    /// reads the head's attributes (as 2.43 does by default in a bare
    /// repository and 2.47 does not), and the head asks for
    /// `merge=union`, which would merge both edits cleanly. The empty-tree
    /// override in [`GIT_MERGE_TREE_ARGS`] wins over the config: both
    /// strategies see the conflict.
    #[test]
    fn the_heads_attributes_never_steer_a_merge() {
        if !have_git() {
            return;
        }
        let r = Repo::new();
        let mut config = git2::Config::open(&r.dir.path().join("config")).unwrap();
        config.set_str("attr.tree", "HEAD").unwrap();
        let base = r.commit(
            &[],
            &[
                ("f.txt", Some("0\n")),
                (".gitattributes", Some("f.txt merge=union\n")),
            ],
            "seed",
        );
        let head = r.commit(&[base], &[("f.txt", Some("head\n"))], "on the branch");
        let tip = r.commit(&[base], &[("f.txt", Some("patch\n"))], "edit f");
        r.repo
            .reference("refs/heads/main", head, true, "test")
            .unwrap();
        r.repo.set_head("refs/heads/main").unwrap();
        let branch = PatchBranch::read(&r.repo, head, tip).unwrap();
        let conflict = Integrated::Conflict {
            paths: vec!["f.txt".into()],
        };
        assert_eq!(r.merge().integrate(head, &branch).unwrap(), conflict);
        assert_eq!(r.rebase().integrate(head, &branch).unwrap(), conflict);
    }

    #[test]
    fn a_driver_program_is_an_executable_file() {
        let dir = tempfile::tempdir().unwrap();
        let tool = script(dir.path(), "tool", "exit 0");
        std::fs::write(dir.path().join("plain"), "").unwrap();
        assert_eq!(
            driver_program(&format!("{} %O", tool.display()), Path::new("/")),
            Ok(tool.clone())
        );
        assert_eq!(
            driver_program("./tool %A", dir.path()),
            Ok(dir.path().join("./tool"))
        );
        assert!(
            driver_program("sh -c 'x'", dir.path())
                .unwrap()
                .ends_with("sh")
        );
        let err = driver_program("plain %A", dir.path()).unwrap_err();
        assert!(err.contains("plain is not a program in"), "{err}");
        let err = driver_program("./plain", dir.path()).unwrap_err();
        assert_eq!(err, "./plain is not an executable file");
        assert_eq!(
            driver_program("  ", dir.path()).unwrap_err(),
            "its command is empty"
        );
    }

    #[test]
    fn a_failing_or_unreadable_git_is_an_error() {
        let r = Repo::new();
        let base = r.commit(&[], &[("f", Some("0\n"))], "seed");
        let head = r.commit(&[base], &[("g", Some("g\n"))], "head");
        let tip = r.commit(&[base], &[("a", Some("a\n"))], "a");
        let branch = PatchBranch::read(&r.repo, head, tip).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let fake = |name: &str, body: &str| {
            let merge = GitMergeTree {
                git: script(dir.path(), name, body),
                git_dir: r.dir.path().to_owned(),
            };
            retry_busy(|| merge.integrate(head, &branch))
        };
        // An old git: the message names the version.
        let old = fake(
            "old-git",
            "if [ \"$1\" = --version ]; then echo 'git version 2.39.5'; exit 0; fi\n\
             echo \"error: unknown option\" >&2; exit 129",
        );
        match old {
            Err(IntegrateError::Failed { stderr, .. }) => {
                assert!(stderr.contains("unknown option"), "{stderr}");
                assert!(stderr.contains("git 2.39"), "{stderr}");
                assert!(stderr.contains("2.40 or later"), "{stderr}");
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            fake("odd-git", "echo nonsense; exit 0"),
            Err(IntegrateError::BadOutput { .. })
        ));
        let missing = GitMergeTree {
            git: dir.path().join("nope"),
            git_dir: r.dir.path().to_owned(),
        };
        assert!(matches!(
            missing.integrate(head, &branch),
            Err(IntegrateError::Spawn(_))
        ));
    }
}
