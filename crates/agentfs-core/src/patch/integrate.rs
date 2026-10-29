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
//! Bead agentfs-mia.3 adds the rebase strategy as a second implementation.

use std::collections::BTreeSet;
use std::path::PathBuf;

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
    #[error(transparent)]
    Git(#[from] git2::Error),
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

/// The arguments after `git`, before the merge's own.
///
/// `attr.tree` is the empty tree: some git versions (2.43, for one) read a
/// bare repository's attributes from `HEAD:.gitattributes` by default,
/// which is the workload's tree; only `$GIT_DIR/info/attributes` (the operator's,
/// AGENTS.md principle 3) may steer a merge.
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
        })
    }
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

    /// The arguments after `git` that merge `patch` onto `head`.
    fn args(head: Oid, patch: &PatchBranch) -> Vec<String> {
        let mut args: Vec<String> = GIT_MERGE_TREE_ARGS.iter().map(|a| a.to_string()).collect();
        args.extend([
            "-z".to_owned(),
            "--name-only".to_owned(),
            "--no-messages".to_owned(),
            format!("--merge-base={}", patch.base),
            head.to_string(),
            patch.tip.to_string(),
        ]);
        args
    }

    /// `git --version` as (major, minor), when it can be read.
    fn version(&self) -> Option<(u32, u32)> {
        let out = git_cli::command(&self.git, &self.git_dir)
            .arg("--version")
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&out.stdout);
        let mut numbers = text.split_whitespace().nth(2)?.split('.');
        Some((numbers.next()?.parse().ok()?, numbers.next()?.parse().ok()?))
    }
}

impl Integrator for GitMergeTree {
    /// `git <GIT_MERGE_TREE_ARGS> -z --name-only --no-messages
    /// --merge-base=<base> <head> <tip>`: exit 0 is a clean merge (the
    /// first NUL-terminated field is the tree), exit 1 a conflict (the
    /// tree, then the conflicted paths up to an empty field); anything else
    /// is [`IntegrateError::Failed`], output it cannot read
    /// [`IntegrateError::BadOutput`]. Refreshing the caller's object
    /// database is the caller's.
    fn integrate(&self, head: Oid, patch: &PatchBranch) -> Result<Integrated, IntegrateError> {
        let args = Self::args(head, patch);
        let out = git_cli::command(&self.git, &self.git_dir)
            .args(&args)
            .output()
            .map_err(IntegrateError::Spawn)?;
        let shown = args.join(" ");
        let conflicted = match out.status.code() {
            Some(0) => false,
            Some(1) => true,
            _ => {
                let mut stderr = String::from_utf8_lossy(&out.stderr).trim().to_owned();
                if let Some((major, minor)) = self.version()
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
