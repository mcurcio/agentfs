//! A mirror: a remote copy of the root's history, outside the tree, which
//! Replication pushes in its own step (`docs/target-architecture/domains/replication.md`).
//! It reads tree history's moves ([`TreeMoved`]) and keeps how far it has read:
//! it is due once a move it has not read moved the root or named a collision
//! ref. The model's git is a bare repository in its own temporary directory.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use crate::calls::TreeId;
use crate::git::git;
use crate::world::{TreeMoved, TreeNames};

static MIRRORS: AtomicU32 = AtomicU32::new(0);

/// One remote copy of the root's branch and every collision ref.
pub struct Mirror {
    /// Its temporary directory, removed on drop.
    dir: PathBuf,
    /// The bare repository.
    repo: PathBuf,
    /// The root's branch, as the trees name it.
    root: String,
    /// How many moves it has read.
    read: usize,
}

impl Mirror {
    /// An empty bare repository in its own temporary directory, its branch the
    /// root's.
    pub fn new(names: &TreeNames) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "agentfs-model-mirror-{}-{}",
            std::process::id(),
            MIRRORS.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let repo = dir.join("origin.git");
        std::fs::create_dir_all(&repo).expect("create the mirror");
        git(
            &repo,
            &["init", "-q", "--bare", "-b", &names.name(TreeId::ROOT)],
        );
        Self {
            dir,
            repo,
            root: names.branch(TreeId::ROOT),
            read: 0,
        }
    }

    /// The bare repository.
    pub fn repo(&self) -> &Path {
        &self.repo
    }

    /// A move it has not read moved the root or names a collision ref.
    pub fn due(&self, moves: &[TreeMoved]) -> bool {
        moves[self.read..]
            .iter()
            .any(|m| m.tree == TreeId::ROOT || !m.collisions.is_empty())
    }

    /// One atomic push from `from`, never forced: the root's branch, whose
    /// history holds every take's decisions, and every collision ref the unread
    /// moves name (owner, 2026-11-03: "yes, \"losing\" patches should still be
    /// part of the history. i want to know who changed what when"). Then all of
    /// `moves` count as read. The root only moves forward and collision refs
    /// never move. Panics on a git failure, as the model's `git` does.
    pub fn push(&mut self, from: &Path, moves: &[TreeMoved]) {
        let to = self.repo.to_string_lossy().into_owned();
        let mut args: Vec<String> = ["push", "-q", "--atomic", &to]
            .into_iter()
            .map(str::to_string)
            .collect();
        args.push(format!("{0}:{0}", self.root));
        for m in &moves[self.read..] {
            args.extend(m.collisions.iter().map(|r| format!("{r}:{r}")));
        }
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        git(from, &args);
        self.read = moves.len();
    }
}

impl Drop for Mirror {
    fn drop(&mut self) {
        if std::env::var_os("AGENTFS_MODEL_KEEP").is_none() {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}
