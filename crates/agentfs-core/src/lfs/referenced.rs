//! Which LFS objects commits reference (design §7), for pushes
//! ([`super::upload`]), `agentfs lfs fetch` and `agentfs lfs prune`.
//!
//! Only agentfs's own pointers count: a blob at a path in the commit's
//! managed `.gitattributes` block (which agentfs wrote) that is a
//! canonical pointer. Pointer text a workload wrote at a path that is not
//! managed is ordinary content, never mistaken for an object.
//!
//! - [`in_history`]: the objects every commit of `head ^ hide` adds or
//!   modifies, each diffed against its first parent without rename
//!   detection. Over a whole branch, that is every object any of its
//!   trees references.
//! - [`in_tree`]: the objects one commit's tree references.

use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

use git2::{Delta, DiffOptions, Oid, Repository};

use super::LfsError;
use super::attributes::{self, ROOT_GITATTRIBUTES};
use super::pointer::{LfsOid, MAX_POINTER_SIZE, Pointer};
use crate::paths::RepoPath;

/// The objects the commits in `head ^ hide` add or modify (all of
/// `head`'s history when `hide` is `None` or unknown here), each with one
/// path that references it, deduplicated and sorted by oid.
pub fn in_history(
    repo: &Repository,
    head: Oid,
    hide: Option<Oid>,
) -> Result<Vec<(Pointer, RepoPath)>, LfsError> {
    walk(repo, head, hide).map_err(|e| {
        LfsError::local(format!(
            "finding the LFS objects of the commits: {}",
            e.message()
        ))
    })
}

/// The objects `commit`'s tree references at its managed paths, each with
/// one path, deduplicated and sorted by oid.
pub fn in_tree(repo: &Repository, commit: Oid) -> Result<Vec<(Pointer, RepoPath)>, LfsError> {
    tree_objects(repo, commit).map_err(|e| {
        LfsError::local(format!(
            "finding the LFS objects of commit {commit}: {}",
            e.message()
        ))
    })
}

/// The blob of a tree's root `.gitattributes`, if it has one.
fn root_attributes(tree: &git2::Tree<'_>) -> Result<Option<Oid>, git2::Error> {
    match tree.get_path(Path::new(ROOT_GITATTRIBUTES)) {
        Ok(entry) if matches!(entry.filemode(), 0o100644 | 0o100755) => Ok(Some(entry.id())),
        Ok(_) => Ok(None),
        Err(e) if e.code() == git2::ErrorCode::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// The pointer in the blob `id`, if it is a canonical one.
fn pointer_in(repo: &Repository, id: Oid) -> Result<Option<Pointer>, git2::Error> {
    let blob = repo.find_blob(id)?;
    if blob.size() > MAX_POINTER_SIZE {
        return Ok(None);
    }
    Ok(Pointer::parse(blob.content()))
}

fn tree_objects(repo: &Repository, commit: Oid) -> Result<Vec<(Pointer, RepoPath)>, git2::Error> {
    let tree = repo.find_commit(commit)?.tree()?;
    let Some(attrs) = root_attributes(&tree)? else {
        return Ok(Vec::new());
    };
    let managed = attributes::split(repo.find_blob(attrs)?.content()).1;
    let mut found: BTreeMap<LfsOid, (Pointer, RepoPath)> = BTreeMap::new();
    for path in managed {
        let entry = match tree.get_path(Path::new(path.as_str())) {
            Ok(entry) => entry,
            Err(e) if e.code() == git2::ErrorCode::NotFound => continue,
            Err(e) => return Err(e),
        };
        if !matches!(entry.filemode(), 0o100644 | 0o100755) {
            continue;
        }
        if let Some(pointer) = pointer_in(repo, entry.id())? {
            found.entry(pointer.oid).or_insert((pointer, path));
        }
    }
    Ok(found.into_values().collect())
}

fn walk(
    repo: &Repository,
    head: Oid,
    hide: Option<Oid>,
) -> Result<Vec<(Pointer, RepoPath)>, git2::Error> {
    let mut revwalk = repo.revwalk()?;
    revwalk.push(head)?;
    if let Some(hide) = hide
        && repo.odb()?.exists(hide)
    {
        revwalk.hide(hide)?;
    }
    // The managed set of each root .gitattributes blob, read once.
    let mut managed_by_blob: HashMap<Oid, BTreeSet<RepoPath>> = HashMap::new();
    let mut found: BTreeMap<LfsOid, (Pointer, RepoPath)> = BTreeMap::new();
    for oid in revwalk {
        let commit = repo.find_commit(oid?)?;
        let tree = commit.tree()?;
        let Some(attrs) = root_attributes(&tree)? else {
            continue;
        };
        let managed = match managed_by_blob.entry(attrs) {
            Entry::Occupied(e) => e.into_mut(),
            Entry::Vacant(e) => e.insert(attributes::split(repo.find_blob(attrs)?.content()).1),
        };
        if managed.is_empty() {
            continue;
        }
        let parent = match commit.parent_count() {
            0 => None,
            _ => Some(commit.parent(0)?.tree()?),
        };
        let mut options = DiffOptions::new();
        options.skip_binary_check(true);
        let diff = repo.diff_tree_to_tree(parent.as_ref(), Some(&tree), Some(&mut options))?;
        for delta in diff.deltas() {
            if !matches!(delta.status(), Delta::Added | Delta::Modified) {
                continue;
            }
            let file = delta.new_file();
            let Some(path) = file
                .path_bytes()
                .and_then(|b| std::str::from_utf8(b).ok())
                .and_then(|p| RepoPath::new(p).ok())
            else {
                continue;
            };
            if !managed.contains(&path) {
                continue;
            }
            if let Some(pointer) = pointer_in(repo, file.id())? {
                found.entry(pointer.oid).or_insert((pointer, path));
            }
        }
    }
    Ok(found.into_values().collect())
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::lfs::attributes::compose;

    pub(crate) struct Repo {
        _dir: tempfile::TempDir,
        pub(crate) repo: Repository,
    }

    impl Repo {
        pub(crate) fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let repo = Repository::init_bare(dir.path()).unwrap();
            Self { _dir: dir, repo }
        }

        /// Commits `files` (path, content) on top of `parent`, with a
        /// managed block for `lfs`.
        pub(crate) fn commit(
            &self,
            parent: Option<Oid>,
            files: &[(&str, Vec<u8>)],
            lfs: &[&str],
        ) -> Oid {
            let mut builder = git2::build::TreeUpdateBuilder::new();
            let base = match parent {
                Some(p) => self.repo.find_commit(p).unwrap().tree().unwrap(),
                None => self
                    .repo
                    .find_tree(self.repo.treebuilder(None).unwrap().write().unwrap())
                    .unwrap(),
            };
            for (path, content) in files {
                let blob = self.repo.blob(content).unwrap();
                builder.upsert(*path, blob, git2::FileMode::Blob);
            }
            let managed: BTreeSet<RepoPath> =
                lfs.iter().map(|p| RepoPath::new(*p).unwrap()).collect();
            let attrs = self.repo.blob(&compose(b"", &managed)).unwrap();
            builder.upsert(ROOT_GITATTRIBUTES, attrs, git2::FileMode::Blob);
            let tree = builder.create_updated(&self.repo, &base).unwrap();
            let tree = self.repo.find_tree(tree).unwrap();
            let sig = git2::Signature::now("t", "t@example.com").unwrap();
            let parents: Vec<git2::Commit<'_>> = parent
                .map(|p| self.repo.find_commit(p).unwrap())
                .into_iter()
                .collect();
            let parents: Vec<&git2::Commit<'_>> = parents.iter().collect();
            self.repo
                .commit(None, &sig, &sig, "c", &tree, &parents)
                .unwrap()
        }
    }

    pub(crate) fn pointer(content: &[u8]) -> (Pointer, Vec<u8>) {
        let p = Pointer::of_reader(content).unwrap();
        (p, p.encode().into_bytes())
    }

    #[test]
    fn unpushed_commits_are_walked_and_objects_deduplicated() {
        let r = Repo::new();
        let (a, a_text) = pointer(b"object a");
        let (b, b_text) = pointer(b"object b");
        let (c, c_text) = pointer(b"object c");
        let first = r.commit(
            None,
            &[("a.bin", a_text.clone()), ("copy.bin", a_text.clone())],
            &["a.bin", "copy.bin"],
        );
        let second = r.commit(
            Some(first),
            &[("b.bin", b_text), ("plain.txt", b"hello".to_vec())],
            &["a.bin", "copy.bin", "b.bin"],
        );
        // Pointer text the workload wrote, at a path that is not managed.
        let third = r.commit(
            Some(second),
            &[("c.bin", c_text.clone()), ("fake.txt", c_text)],
            &["a.bin", "copy.bin", "b.bin", "c.bin"],
        );
        let all = in_history(&r.repo, third, None).unwrap();
        let mut want = vec![a, b, c];
        want.sort();
        assert_eq!(all.iter().map(|(p, _)| *p).collect::<Vec<_>>(), want);
        // Only what the remote lacks.
        let new = in_history(&r.repo, third, Some(second)).unwrap();
        assert_eq!(new.len(), 1);
        assert_eq!(new[0].0, c);
        assert_eq!(new[0].1.as_str(), "c.bin");
        assert!(in_history(&r.repo, third, Some(third)).unwrap().is_empty());
        // An unknown last pushed commit means everything.
        let unknown = Oid::from_str("1111111111111111111111111111111111111111").unwrap();
        assert_eq!(in_history(&r.repo, third, Some(unknown)).unwrap().len(), 3);
    }

    #[test]
    fn unmanaged_or_unchanged_pointers_are_not_objects() {
        let r = Repo::new();
        let (a, a_text) = pointer(b"object a");
        // Managed nowhere: pointer text only.
        let first = r.commit(None, &[("a.bin", a_text.clone())], &[]);
        assert!(in_history(&r.repo, first, None).unwrap().is_empty());
        // Managed later without changing: not added or modified then.
        let second = r.commit(Some(first), &[], &["a.bin"]);
        assert!(in_history(&r.repo, second, Some(first)).unwrap().is_empty());
        let _ = a;
    }
}

#[cfg(test)]
mod tree_tests {
    use super::tests::{Repo, pointer};
    use super::*;

    #[test]
    fn a_tree_lists_its_managed_pointers_only() {
        let r = Repo::new();
        let (a, a_text) = pointer(b"object a");
        let (b, b_text) = pointer(b"object b");
        let (c, c_text) = pointer(b"object c");
        let first = r.commit(
            None,
            &[("a.bin", a_text.clone()), ("copy.bin", a_text)],
            &["a.bin", "copy.bin"],
        );
        // b replaces nothing; c is pointer text at an unmanaged path; a
        // managed path that is not a pointer is skipped.
        let second = r.commit(
            Some(first),
            &[
                ("b.bin", b_text),
                ("fake.txt", c_text),
                ("plain.bin", b"plain".to_vec()),
            ],
            &["a.bin", "b.bin", "plain.bin", "gone.bin"],
        );
        let got: Vec<Pointer> = in_tree(&r.repo, second)
            .unwrap()
            .into_iter()
            .map(|(p, _)| p)
            .collect();
        let mut want = vec![a, b];
        want.sort();
        assert_eq!(got, want);
        let first_only: Vec<Pointer> = in_tree(&r.repo, first)
            .unwrap()
            .into_iter()
            .map(|(p, _)| p)
            .collect();
        assert_eq!(first_only, [a]);
        let _ = c;
        // The walk over the whole branch sees every object any tree has.
        assert_eq!(in_history(&r.repo, second, None).unwrap().len(), 2);
    }
}
