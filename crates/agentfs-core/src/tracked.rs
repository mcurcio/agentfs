//! The set of tracked paths: what history will contain once every durable
//! record is committed (design §4).
//!
//! This plays the part of git's index. Ignore rules and nested-repository
//! exclusion only apply to paths that are not tracked, and a path counts as
//! tracked from the moment its record is durable, not only once it reaches
//! the branch head. The set is rebuilt at open from the branch head plus the
//! uncommitted journal, then kept current by the recorder.

use std::collections::BTreeSet;
use std::ops::Bound;
use std::sync::RwLock;

use crate::paths::RepoPath;
use crate::record::Change;

/// Tracked file and symlink paths. Safe to share across threads.
#[derive(Debug, Default)]
pub struct Tracked {
    paths: RwLock<BTreeSet<String>>,
}

impl Tracked {
    /// The blobs (files and symlinks) in `tree`, or an empty set.
    ///
    /// Names that are not UTF-8 cannot be [`RepoPath`]s and so can never be
    /// looked up; they are kept lossily.
    pub fn from_tree(tree: Option<&git2::Tree<'_>>) -> Result<Self, git2::Error> {
        let mut paths = BTreeSet::new();
        if let Some(tree) = tree {
            tree.walk(git2::TreeWalkMode::PreOrder, |root, entry| {
                if entry.kind() == Some(git2::ObjectType::Blob) {
                    let name = String::from_utf8_lossy(entry.name_bytes());
                    paths.insert(format!("{root}{name}"));
                }
                git2::TreeWalkResult::Ok
            })?;
        }
        Ok(Self {
            paths: RwLock::new(paths),
        })
    }

    /// True if `path` itself is a tracked file or symlink.
    pub fn contains(&self, path: &RepoPath) -> bool {
        self.read().contains(path.as_str())
    }

    /// True if `path` is tracked, or is a directory holding tracked paths.
    pub fn contains_or_under(&self, path: &RepoPath) -> bool {
        let paths = self.read();
        paths.contains(path.as_str()) || has_under(&paths, path.as_str())
    }

    /// Updates the set for a durable change, mirroring how history applies
    /// it (`history::apply`).
    pub fn apply(&self, change: &Change) {
        let mut paths = self.write();
        match change {
            Change::Write { path, .. } | Change::Symlink { path, .. } => {
                remove_under(&mut paths, path.as_str());
                let mut prefix = String::new();
                let components: Vec<&str> = path.components().collect();
                for component in &components[..components.len() - 1] {
                    if !prefix.is_empty() {
                        prefix.push('/');
                    }
                    prefix.push_str(component);
                    paths.remove(&prefix);
                }
                paths.insert(path.as_str().to_owned());
            }
            Change::Remove { path } => {
                paths.remove(path.as_str());
                remove_under(&mut paths, path.as_str());
            }
        }
    }

    pub fn len(&self) -> usize {
        self.read().len()
    }

    pub fn is_empty(&self) -> bool {
        self.read().is_empty()
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, BTreeSet<String>> {
        self.paths.read().expect("tracked set lock poisoned")
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, BTreeSet<String>> {
        self.paths.write().expect("tracked set lock poisoned")
    }
}

/// The paths strictly beneath directory `dir`.
fn under<'a>(paths: &'a BTreeSet<String>, dir: &str) -> impl Iterator<Item = &'a String> + 'a {
    let prefix = format!("{dir}/");
    paths
        .range::<String, _>((Bound::Included(prefix.clone()), Bound::Unbounded))
        .take_while(move |p| p.starts_with(&prefix))
}

fn has_under(paths: &BTreeSet<String>, dir: &str) -> bool {
    under(paths, dir).next().is_some()
}

fn remove_under(paths: &mut BTreeSet<String>, dir: &str) {
    let doomed: Vec<String> = under(paths, dir).cloned().collect();
    for p in doomed {
        paths.remove(&p);
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
        }
    }

    fn all(t: &Tracked) -> Vec<String> {
        t.read().iter().cloned().collect()
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
}
