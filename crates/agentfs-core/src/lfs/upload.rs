//! Which LFS objects a push must upload first (design §7).
//!
//! Every commit not yet on the remote (`head ^ last_pushed`, or the whole
//! branch when the remote has nothing or its last pushed commit is
//! unknown) is diffed against its first parent, without rename detection.
//! An added or modified blob is an object to upload when its path is in
//! that commit's managed `.gitattributes` block (which agentfs wrote) and
//! it is a canonical pointer. Pointer text a workload wrote at a path that
//! is not managed is ordinary content, never mistaken for an object.

use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

use git2::{Delta, DiffOptions, Oid, Repository};

use super::LfsError;
use super::attributes::{self, ROOT_GITATTRIBUTES};
use super::pointer::{LfsOid, MAX_POINTER_SIZE, Pointer};
use crate::paths::RepoPath;

/// The objects the commits in `head ^ last_pushed` reference, each with
/// one path that references it, deduplicated and sorted by oid.
pub fn objects_to_upload(
    repo: &Repository,
    head: Oid,
    last_pushed: Option<Oid>,
) -> Result<Vec<(Pointer, RepoPath)>, LfsError> {
    walk(repo, head, last_pushed).map_err(|e| {
        LfsError::local(format!(
            "finding the LFS objects of the unpushed commits: {}",
            e.message()
        ))
    })
}

fn walk(
    repo: &Repository,
    head: Oid,
    last_pushed: Option<Oid>,
) -> Result<Vec<(Pointer, RepoPath)>, git2::Error> {
    let mut revwalk = repo.revwalk()?;
    revwalk.push(head)?;
    if let Some(last) = last_pushed
        && repo.odb()?.exists(last)
    {
        revwalk.hide(last)?;
    }
    // The managed set of each root .gitattributes blob, read once.
    let mut managed_by_blob: HashMap<Oid, BTreeSet<RepoPath>> = HashMap::new();
    let mut found: BTreeMap<LfsOid, (Pointer, RepoPath)> = BTreeMap::new();
    for oid in revwalk {
        let commit = repo.find_commit(oid?)?;
        let tree = commit.tree()?;
        let attrs = match tree.get_path(Path::new(ROOT_GITATTRIBUTES)) {
            Ok(entry) if matches!(entry.filemode(), 0o100644 | 0o100755) => entry.id(),
            Ok(_) => continue,
            Err(e) if e.code() == git2::ErrorCode::NotFound => continue,
            Err(e) => return Err(e),
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
            let blob = repo.find_blob(file.id())?;
            if blob.size() > MAX_POINTER_SIZE {
                continue;
            }
            if let Some(pointer) = Pointer::parse(blob.content()) {
                found.entry(pointer.oid).or_insert((pointer, path));
            }
        }
    }
    Ok(found.into_values().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lfs::attributes::compose;

    struct Repo {
        _dir: tempfile::TempDir,
        repo: Repository,
    }

    impl Repo {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let repo = Repository::init_bare(dir.path()).unwrap();
            Self { _dir: dir, repo }
        }

        /// Commits `files` (path, content) on top of `parent`, with a
        /// managed block for `lfs`.
        fn commit(&self, parent: Option<Oid>, files: &[(&str, Vec<u8>)], lfs: &[&str]) -> Oid {
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

    fn pointer(content: &[u8]) -> (Pointer, Vec<u8>) {
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
        let all = objects_to_upload(&r.repo, third, None).unwrap();
        let mut want = vec![a, b, c];
        want.sort();
        assert_eq!(all.iter().map(|(p, _)| *p).collect::<Vec<_>>(), want);
        // Only what the remote lacks.
        let new = objects_to_upload(&r.repo, third, Some(second)).unwrap();
        assert_eq!(new.len(), 1);
        assert_eq!(new[0].0, c);
        assert_eq!(new[0].1.as_str(), "c.bin");
        assert!(
            objects_to_upload(&r.repo, third, Some(third))
                .unwrap()
                .is_empty()
        );
        // An unknown last pushed commit means everything.
        let unknown = Oid::from_str("1111111111111111111111111111111111111111").unwrap();
        assert_eq!(
            objects_to_upload(&r.repo, third, Some(unknown))
                .unwrap()
                .len(),
            3
        );
    }

    #[test]
    fn unmanaged_or_unchanged_pointers_are_not_objects() {
        let r = Repo::new();
        let (a, a_text) = pointer(b"object a");
        // Managed nowhere: pointer text only.
        let first = r.commit(None, &[("a.bin", a_text.clone())], &[]);
        assert!(objects_to_upload(&r.repo, first, None).unwrap().is_empty());
        // Managed later without changing: not added or modified then.
        let second = r.commit(Some(first), &[], &["a.bin"]);
        assert!(
            objects_to_upload(&r.repo, second, Some(first))
                .unwrap()
                .is_empty()
        );
        let _ = a;
    }
}
