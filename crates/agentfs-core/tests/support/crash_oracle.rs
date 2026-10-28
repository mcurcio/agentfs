//! The crash harnesses' oracle (design §18 test 3): after a kill or an
//! injected failure at a failpoint, and the recovery that follows, nothing
//! promised is lost and nothing is committed twice.
//!
//! A workload is a sequence of states of the tree (a [`Model`]), one per
//! record. Its harness writes a promise log line after each call that
//! returned success. The checks:
//!
//! - **Nothing lost**: the head's tree is the state of the last promise,
//!   or (after a kill) the state of the one step in flight, which landed
//!   whole or not at all.
//! - **Nothing duplicated**: the first-parent history's trees are a
//!   strictly increasing subsequence of the model's states (commits may
//!   group records, never repeat or reorder them), no commit is empty,
//!   and `Agentfs-Seq` strictly increases.
//! - **Clean**: no journal record past the head, nothing left in
//!   `pending/`, `git fsck --full --strict` passes, every LFS pointer's
//!   object is in the cache, and a remote that was pushed after recovery
//!   holds exactly the head.
//!
//! Shared by `crates/agentfs-core/tests` and (through `#[path]`)
//! `crates/agentfs/tests`.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use git2::{ObjectType, Oid, Repository, TreeWalkMode, TreeWalkResult};

/// A tree: path → content (LFS content resolved through the cache).
pub type Tree = BTreeMap<String, Vec<u8>>;

/// The states a workload passes through: `states[0]` is the empty tree,
/// `states[k]` the tree after its `k`th record.
#[derive(Debug, Clone, Default)]
pub struct Model {
    pub states: Vec<Tree>,
}

impl Model {
    pub fn new() -> Self {
        Self {
            states: vec![Tree::new()],
        }
    }

    /// The state after one more record: the last state changed by `f`.
    pub fn step(&mut self, f: impl FnOnce(&mut Tree)) {
        let mut next = self.states.last().cloned().unwrap_or_default();
        f(&mut next);
        self.states.push(next);
    }

    pub fn last(&self) -> usize {
        self.states.len() - 1
    }
}

/// How the run under test ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Killed: the step after the last promise may or may not have landed.
    Kill,
    /// Failed and carried on: every step landed.
    Error,
}

/// An append-only, fsynced log of promises: one line per call that
/// returned success, written by the process under test.
pub struct PromiseLog {
    path: PathBuf,
}

impl PromiseLog {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Appends `line` durably.
    pub fn promise(&self, line: &str) {
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .expect("opening the promise log");
        writeln!(f, "{line}").expect("writing the promise log");
        f.sync_all().expect("syncing the promise log");
    }

    pub fn lines(&self) -> Vec<String> {
        fs::read_to_string(&self.path)
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// The highest `R <step>` (a record returned) promise, 0 for none.
    pub fn last_record(&self) -> usize {
        self.numbers("R").max().unwrap_or(0)
    }

    /// The highest `C <step>` (a commit pass returned) promise.
    pub fn last_commit(&self) -> Option<usize> {
        self.numbers("C").max()
    }

    /// Every `U <remote> <oid>` (a push returned success) promise.
    pub fn pushed(&self) -> Vec<(String, Oid)> {
        self.lines()
            .iter()
            .filter_map(|l| {
                let mut parts = l.split(' ');
                (parts.next() == Some("U")).then_some(())?;
                let remote = parts.next()?.to_owned();
                let oid = Oid::from_str(parts.next()?).ok()?;
                Some((remote, oid))
            })
            .collect()
    }

    fn numbers(&self, tag: &str) -> impl Iterator<Item = usize> {
        let tag = format!("{tag} ");
        self.lines()
            .into_iter()
            .filter_map(move |l| l.strip_prefix(&tag).and_then(|n| n.trim().parse().ok()))
    }
}

/// The tree of `commit`, with LFS pointers resolved from `lfs_objects`
/// (`<state>/lfs/objects`) when given.
pub fn tree_of(repo: &Repository, commit: Oid, lfs_objects: Option<&Path>) -> Tree {
    let tree = repo.find_commit(commit).unwrap().tree().unwrap();
    let mut out = Tree::new();
    tree.walk(TreeWalkMode::PreOrder, |dir, entry| {
        if entry.kind() == Some(ObjectType::Blob) {
            let path = format!("{dir}{}", entry.name().unwrap());
            if path == ".gitattributes" {
                // The managed LFS block (design §7), not workload content.
                return TreeWalkResult::Ok;
            }
            let blob = repo.find_blob(entry.id()).unwrap();
            let content = blob.content().to_vec();
            let content = match (lfs_objects, lfs_pointer(&content)) {
                (Some(objects), Some(oid)) => {
                    let object = objects.join(&oid[0..2]).join(&oid[2..4]).join(&oid);
                    fs::read(&object).unwrap_or_else(|e| {
                        panic!("{path}: LFS object {oid} is not in the cache: {e}")
                    })
                }
                _ => content,
            };
            out.insert(path, content);
        }
        TreeWalkResult::Ok
    })
    .unwrap();
    out
}

/// The oid of an LFS pointer blob, if `content` is one.
pub fn lfs_pointer(content: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(content).ok()?;
    if !text.starts_with("version https://git-lfs.github.com/spec/v1\n") {
        return None;
    }
    text.lines()
        .find_map(|l| l.strip_prefix("oid sha256:"))
        .map(str::to_owned)
}

/// Every LFS oid the trees of `commits` point to.
pub fn lfs_oids(repo: &Repository, commits: &[Oid]) -> Vec<String> {
    let mut out = Vec::new();
    for c in commits {
        let tree = repo.find_commit(*c).unwrap().tree().unwrap();
        tree.walk(TreeWalkMode::PreOrder, |_, entry| {
            if entry.kind() == Some(ObjectType::Blob)
                && let Some(oid) = lfs_pointer(repo.find_blob(entry.id()).unwrap().content())
                && !out.contains(&oid)
            {
                out.push(oid);
            }
            TreeWalkResult::Ok
        })
        .unwrap();
    }
    out
}

/// The branch's first-parent history, oldest first.
pub fn first_parent(repo: &Repository, branch_ref: &str) -> Vec<Oid> {
    let Ok(head) = repo.refname_to_id(branch_ref) else {
        return Vec::new();
    };
    let mut out = vec![head];
    let mut c = repo.find_commit(head).unwrap();
    while c.parent_count() > 0 {
        c = c.parent(0).unwrap();
        out.push(c.id());
    }
    out.reverse();
    out
}

fn seq_of(repo: &Repository, commit: Oid) -> Option<u64> {
    let c = repo.find_commit(commit).unwrap();
    agentfs_core::history::seq_from_message(c.message_raw().unwrap_or_default())
}

/// The model index of each commit on the branch, after checking that
/// nothing is duplicated: each commit's tree is a model state later than
/// the previous commit's, no commit is empty, and `Agentfs-Seq` strictly
/// increases. Errors describe the first violation.
///
/// One exception: a root commit may hold the empty tree, state 0. With no
/// branch yet, records that leave the tree empty still commit, so the
/// head carries their `Agentfs-Seq` (design §3.3): a failed write dropped
/// because its copy is gone, alone in its commit window, makes one.
pub fn check_history(
    git_dir: &Path,
    branch_ref: &str,
    model: &Model,
    lfs_objects: Option<&Path>,
) -> Result<Vec<usize>, String> {
    let repo = Repository::open_bare(git_dir).map_err(|e| e.to_string())?;
    let commits = first_parent(&repo, branch_ref);
    let mut indices = Vec::new();
    let mut next = 1;
    let mut last_seq: Option<u64> = None;
    let mut last_tree: Option<Oid> = None;
    for c in &commits {
        let commit = repo.find_commit(*c).unwrap();
        if last_tree == Some(commit.tree_id()) {
            return Err(format!("commit {c} is empty: its tree is its parent's"));
        }
        last_tree = Some(commit.tree_id());
        let seq = seq_of(&repo, *c).ok_or_else(|| format!("commit {c} has no Agentfs-Seq"))?;
        if last_seq.is_some_and(|l| seq <= l) {
            return Err(format!(
                "commit {c}: Agentfs-Seq {seq} does not increase (previous {last_seq:?})"
            ));
        }
        last_seq = Some(seq);
        let tree = tree_of(&repo, *c, lfs_objects);
        if commit.parent_count() == 0 && tree == model.states[0] {
            indices.push(0);
            continue;
        }
        let found = (next..model.states.len()).find(|i| model.states[*i] == tree);
        match found {
            Some(i) => {
                indices.push(i);
                next = i + 1;
            }
            None => {
                let earlier = (0..next).find(|i| model.states[*i] == tree);
                return Err(match earlier {
                    Some(i) => format!(
                        "commit {c} repeats or reorders state {i} (after state {})",
                        next - 1
                    ),
                    None => format!(
                        "commit {c} has a tree that is no state of the workload: {:?}",
                        tree.keys().collect::<Vec<_>>()
                    ),
                });
            }
        }
    }
    Ok(indices)
}

/// Nothing lost: the head is the state of the last promise (`promised`,
/// a model index), or after a kill the next one.
pub fn check_promised(indices: &[usize], promised: usize, mode: Mode) -> Result<(), String> {
    let head = indices.last().copied().unwrap_or(0);
    let ok = match mode {
        Mode::Kill => head == promised || head == promised + 1,
        Mode::Error => head == promised,
    };
    if ok {
        Ok(())
    } else {
        Err(format!(
            "the head is state {head}, but state {promised} was promised ({mode:?})"
        ))
    }
}

/// Runs `git fsck --full --strict` on the repository at `dir`.
pub fn fsck(dir: &Path) -> Result<(), String> {
    let out = Command::new("git")
        .args(["fsck", "--full", "--strict", "--no-progress"])
        .env("GIT_DIR", dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("HOME", dir)
        .output()
        .map_err(|e| format!("running git fsck: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "git fsck {}: {}{}",
            dir.display(),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ))
    }
}

/// The state area is clean after recovery: no journal record left (all
/// in history), nothing in `pending/`, the history passes fsck, and every
/// LFS pointer of the branch has its object in the cache.
pub fn check_clean(state: &Path, branch_ref: &str) -> Result<(), String> {
    let (records, torn) = agentfs_core::journal::read_records(&state.join("journal/journal"))
        .map_err(|e| format!("reading the journal: {e}"))?;
    if !records.is_empty() || torn != 0 {
        return Err(format!(
            "the journal still holds {} record(s) and {torn} torn byte(s) after recovery",
            records.len()
        ));
    }
    let left: Vec<String> = fs::read_dir(state.join("pending"))
        .map_err(|e| e.to_string())?
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    if !left.is_empty() {
        return Err(format!("pending/ still holds {left:?} after recovery"));
    }
    let git = state.join("git");
    fsck(&git)?;
    let repo = Repository::open_bare(&git).map_err(|e| e.to_string())?;
    let commits = first_parent(&repo, branch_ref);
    let objects = state.join("lfs/objects");
    for oid in lfs_oids(&repo, &commits) {
        let object = objects.join(&oid[0..2]).join(&oid[2..4]).join(&oid);
        if !object.is_file() {
            return Err(format!("LFS object {oid} is missing from the cache"));
        }
    }
    Ok(())
}

/// A remote pushed after recovery: its branch is `head`, it has no
/// `refs/agentfs/<volume>/diverged/*`, it passes fsck, and (for a local
/// remote) it holds every LFS object the branch points to.
pub fn check_remote(
    remote: &Path,
    branch_ref: &str,
    volume: &str,
    head: Oid,
    lfs: bool,
) -> Result<(), String> {
    let repo = Repository::open_bare(remote).map_err(|e| e.to_string())?;
    let theirs = repo
        .refname_to_id(branch_ref)
        .map_err(|e| format!("{}: {branch_ref}: {e}", remote.display()))?;
    if theirs != head {
        return Err(format!(
            "{}: the branch is {theirs}, the head is {head}",
            remote.display()
        ));
    }
    let diverged: Vec<String> = repo
        .references_glob(&format!("refs/agentfs/{volume}/diverged/*"))
        .unwrap()
        .filter_map(|r| r.ok()?.name().ok().map(str::to_owned))
        .collect();
    if !diverged.is_empty() {
        return Err(format!("{}: diverged refs {diverged:?}", remote.display()));
    }
    fsck(remote)?;
    if lfs {
        let commits = first_parent(&repo, branch_ref);
        for oid in lfs_oids(&repo, &commits) {
            let object = remote
                .join("lfs/objects")
                .join(&oid[0..2])
                .join(&oid[2..4])
                .join(&oid);
            if !object.is_file() {
                return Err(format!("{}: LFS object {oid} is missing", remote.display()));
            }
        }
    }
    Ok(())
}

/// Every head a push promised is the head or an ancestor of it: no push
/// is ever undone.
pub fn check_pushed(git_dir: &Path, head: Oid, pushed: &[(String, Oid)]) -> Result<(), String> {
    let repo = Repository::open_bare(git_dir).map_err(|e| e.to_string())?;
    for (remote, oid) in pushed {
        let ok = *oid == head || repo.graph_descendant_of(head, *oid).unwrap_or(false);
        if !ok {
            return Err(format!(
                "{oid}, pushed to {remote}, is not in the history of the head {head}"
            ));
        }
    }
    Ok(())
}
