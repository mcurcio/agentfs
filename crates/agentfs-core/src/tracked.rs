//! The set of tracked paths: what history will contain once every durable
//! record is committed (design §4).
//!
//! This plays the part of git's index. Ignore rules and nested-repository
//! exclusion only apply to paths that are not tracked, and a path counts as
//! tracked from the moment its record is durable, not only once it reaches
//! the branch head. The set is rebuilt at open from the branch head plus the
//! uncommitted journal, then kept current by the recorder.
//!
//! It is a tree of path components, so a move ([`Change::Move`], a
//! directory rename) detaches one subtree and attaches it elsewhere: its
//! cost depends on the depth of the two paths, not on how many paths are
//! beneath them (design §3.1).

use std::collections::BTreeMap;
use std::sync::RwLock;

use crate::paths::RepoPath;
use crate::record::Change;

/// Tracked file and symlink paths. Safe to share across threads.
#[derive(Debug, Default)]
pub struct Tracked {
    root: RwLock<Node>,
}

/// One path component: a tracked file or symlink (`leaf`), or a directory
/// holding tracked paths (`children`), never both. Empty directories are
/// pruned, so a node with no children is a leaf.
#[derive(Debug, Default)]
struct Node {
    leaf: bool,
    children: BTreeMap<String, Node>,
}

impl Node {
    fn is_empty(&self) -> bool {
        !self.leaf && self.children.is_empty()
    }

    fn get(&self, path: &str) -> Option<&Node> {
        let mut node = self;
        for c in path.split('/') {
            node = node.children.get(c)?;
        }
        Some(node)
    }

    /// The node at `path`, made if missing; ancestors that were files stop
    /// being files.
    fn make(&mut self, path: &str) -> &mut Node {
        let mut node = self;
        for c in path.split('/') {
            node.leaf = false;
            node = node.children.entry(c.to_owned()).or_default();
        }
        node
    }

    /// Detaches and returns the subtree at `path`, pruning directories it
    /// leaves empty.
    fn take(&mut self, path: &str) -> Option<Node> {
        let (first, rest) = match path.split_once('/') {
            Some((first, rest)) => (first, Some(rest)),
            None => (path, None),
        };
        let taken = match rest {
            None => self.children.remove(first),
            Some(rest) => {
                let child = self.children.get_mut(first)?;
                let taken = child.take(rest);
                if child.is_empty() {
                    self.children.remove(first);
                }
                taken
            }
        };
        taken.filter(|n| !n.is_empty())
    }

    fn count(&self) -> usize {
        usize::from(self.leaf) + self.children.values().map(Node::count).sum::<usize>()
    }

    fn collect(&self, prefix: &str, out: &mut Vec<String>) {
        if self.leaf {
            out.push(prefix.to_owned());
        }
        for (name, child) in &self.children {
            let path = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            child.collect(&path, out);
        }
    }
}

impl Tracked {
    /// The blobs (files and symlinks) in `tree`, or an empty set.
    ///
    /// Names that are not UTF-8 cannot be [`RepoPath`]s and so can never be
    /// looked up; they are kept lossily.
    pub fn from_tree(tree: Option<&git2::Tree<'_>>) -> Result<Self, git2::Error> {
        let mut root = Node::default();
        if let Some(tree) = tree {
            tree.walk(git2::TreeWalkMode::PreOrder, |dir, entry| {
                if entry.kind() == Some(git2::ObjectType::Blob) {
                    let name = String::from_utf8_lossy(entry.name_bytes());
                    root.make(&format!("{dir}{name}")).leaf = true;
                }
                git2::TreeWalkResult::Ok
            })?;
        }
        Ok(Self {
            root: RwLock::new(root),
        })
    }

    /// True if `path` itself is a tracked file or symlink.
    pub fn contains(&self, path: &RepoPath) -> bool {
        self.read().get(path.as_str()).is_some_and(|n| n.leaf)
    }

    /// True if `path` is tracked, or is a directory holding tracked paths.
    pub fn contains_or_under(&self, path: &RepoPath) -> bool {
        self.read().get(path.as_str()).is_some()
    }

    /// Updates the set for a durable change, mirroring how history applies
    /// it (`history::apply`).
    pub fn apply(&self, change: &Change) {
        let mut root = self.write();
        match change {
            Change::Write { path, .. } | Change::Symlink { path, .. } => {
                let node = root.make(path.as_str());
                node.children.clear();
                node.leaf = true;
            }
            Change::Remove { path } => {
                root.take(path.as_str());
            }
            Change::Move { from, to } => {
                let moved = root.take(from.as_str());
                root.take(to.as_str());
                if let Some(moved) = moved {
                    *root.make(to.as_str()) = moved;
                }
            }
        }
    }

    /// Every tracked path at or beneath `path`, sorted (O(paths beneath)).
    pub fn under(&self, path: &RepoPath) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(node) = self.read().get(path.as_str()) {
            node.collect(path.as_str(), &mut out);
        }
        out
    }

    /// How many paths are tracked (O(paths)).
    pub fn len(&self) -> usize {
        self.read().count()
    }

    pub fn is_empty(&self) -> bool {
        self.read().is_empty()
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, Node> {
        self.root.read().expect("tracked set lock poisoned")
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, Node> {
        self.root.write().expect("tracked set lock poisoned")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pending::PendingId;

    fn p(s: &str) -> RepoPath {
        RepoPath::new(s).unwrap()
    }

    fn write(s: &str) -> Change {
        Change::Write {
            path: p(s),
            pending: PendingId(0),
            executable: false,
            store: Default::default(),
        }
    }

    fn all(t: &Tracked) -> Vec<String> {
        let mut out = Vec::new();
        t.read().collect("", &mut out);
        out.sort();
        out
    }

    fn sample() -> (tempfile::TempDir, Tracked) {
        let dir = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init_bare(dir.path()).unwrap();
        let blob = repo.blob(b"x").unwrap();
        let mut sub = repo.treebuilder(None).unwrap();
        sub.insert("b", blob, 0o100644).unwrap();
        let sub = sub.write().unwrap();
        let mut root = repo.treebuilder(None).unwrap();
        root.insert("a", blob, 0o100644).unwrap();
        root.insert("d", sub, 0o040000).unwrap();
        root.insert("link", blob, 0o120000).unwrap();
        let tree = repo.find_tree(root.write().unwrap()).unwrap();
        let tracked = Tracked::from_tree(Some(&tree)).unwrap();
        (dir, tracked)
    }

    #[test]
    fn from_tree_lists_files_and_symlinks() {
        let (_d, t) = sample();
        assert_eq!(all(&t), ["a", "d/b", "link"]);
        assert!(Tracked::from_tree(None).unwrap().is_empty());
    }

    #[test]
    fn membership() {
        let (_d, t) = sample();
        t.apply(&write("d-x"));
        t.apply(&write("db"));
        assert!(t.contains(&p("a")));
        assert!(t.contains(&p("d/b")));
        assert!(!t.contains(&p("d")));
        assert!(t.contains_or_under(&p("d")));
        assert!(t.contains_or_under(&p("d/b")));
        assert!(!t.contains_or_under(&p("d/c")));
        assert!(!t.contains_or_under(&p("x")));
        // Siblings that share a prefix are not "under" d.
        t.apply(&Change::Remove { path: p("d/b") });
        assert!(!t.contains_or_under(&p("d")));
        assert!(t.contains(&p("d-x")) && t.contains(&p("db")));
    }

    #[test]
    fn apply_mirrors_history() {
        let (_d, t) = sample();
        // A file replaces a directory.
        t.apply(&write("d"));
        assert_eq!(all(&t), ["a", "d", "link"]);
        // A directory replaces a file.
        t.apply(&write("a/x"));
        assert_eq!(all(&t), ["a/x", "d", "link"]);
        t.apply(&Change::Symlink {
            path: p("s/l"),
            target: "t".into(),
        });
        assert!(t.contains(&p("s/l")));
        // Removing a directory removes its subtree only.
        t.apply(&write("a/y/z"));
        t.apply(&write("ab"));
        t.apply(&Change::Remove { path: p("a") });
        assert_eq!(all(&t), ["ab", "d", "link", "s/l"]);
        assert_eq!(t.len(), 4);
    }

    fn mv(from: &str, to: &str) -> Change {
        Change::Move {
            from: p(from),
            to: p(to),
        }
    }

    #[test]
    fn a_move_carries_the_subtree_and_replaces_the_destination() {
        let (_d, t) = sample();
        t.apply(&write("d/e/f"));
        t.apply(&write("x/old"));
        t.apply(&mv("d", "x"));
        assert_eq!(all(&t), ["a", "link", "x/b", "x/e/f"]);
        assert!(!t.contains_or_under(&p("d")));
        assert_eq!(t.under(&p("x")), ["x/b", "x/e/f"]);
        // Into a new parent; an ancestor that was a file stops being one.
        t.apply(&mv("x/e", "a/e"));
        assert_eq!(all(&t), ["a/e/f", "link", "x/b"]);
        // A move of nothing clears the destination.
        t.apply(&mv("nothing", "x"));
        assert_eq!(all(&t), ["a/e/f", "link"]);
        // A file moves too.
        t.apply(&mv("link", "l2"));
        assert_eq!(all(&t), ["a/e/f", "l2"]);
        assert_eq!(t.len(), 2);
    }
}
