use agentfs_core::tree_git::{RefUpdate, TreeEntry, TreeGit};
use git2::Oid;
use std::process::Command;

fn update(name: &str, new: Option<Oid>, old: Option<Oid>) -> RefUpdate {
    RefUpdate {
        refname: name.into(),
        new,
        expected_old: old,
    }
}

#[test]
fn objects_references_and_private_checkout() {
    let temp = tempfile::tempdir().unwrap();
    let repo = TreeGit::init(temp.path().join("repo.git")).unwrap();
    let blob = repo.write_blob(b"binary\0\xff\n").unwrap();
    assert_eq!(repo.read_blob(blob).unwrap(), b"binary\0\xff\n");
    let files = repo
        .write_tree(&[TreeEntry {
            name: b"tab\tline\n\xff".to_vec(),
            mode: 0o100755,
            oid: blob,
        }])
        .unwrap();
    let metadata = repo
        .write_tree(&[TreeEntry {
            name: b"state".to_vec(),
            mode: 0o100644,
            oid: blob,
        }])
        .unwrap();
    let envelope = repo
        .write_tree(&[
            TreeEntry {
                name: b"files".to_vec(),
                mode: 0o40000,
                oid: files,
            },
            TreeEntry {
                name: b"metadata".to_vec(),
                mode: 0o40000,
                oid: metadata,
            },
        ])
        .unwrap();
    assert_eq!(repo.list_tree(files).unwrap()[0].name, b"tab\tline\n\xff");
    let first = repo.write_commit(envelope, &[], "first\n").unwrap();
    let second = repo.write_commit(envelope, &[first], "second\n").unwrap();
    let merged = repo
        .write_commit(envelope, &[first, second], "merge\n")
        .unwrap();
    let read = repo.read_commit(merged).unwrap();
    assert_eq!(read.tree, envelope);
    assert_eq!(read.parents, vec![first, second]);
    assert_eq!(read.message, b"merge\n");
    assert!(repo.is_ancestor(first, merged).unwrap());
    assert!(!repo.is_ancestor(merged, first).unwrap());
    assert_eq!(repo.head("refs/heads/a").unwrap(), None);
    repo.ref_transaction(&[
        update("refs/heads/a", Some(first), None),
        update("refs/queue/1", Some(first), None),
    ])
    .unwrap();
    assert!(
        repo.ref_transaction(&[
            update("refs/heads/a", Some(second), Some(first)),
            update("refs/queue/1", Some(second), Some(second))
        ])
        .is_err()
    );
    assert_eq!(repo.head("refs/heads/a").unwrap(), Some(first));
    assert_eq!(repo.head("refs/queue/1").unwrap(), Some(first));
    repo.ref_transaction(&[
        update("refs/heads/a", Some(merged), Some(first)),
        update("refs/queue/1", None, Some(first)),
        update("refs/queue/2", Some(second), None),
    ])
    .unwrap();
    assert_eq!(
        repo.list_refs("refs/queue/").unwrap(),
        vec![("refs/queue/2".into(), second)]
    );
    let checkout = temp.path().join("cache");
    std::fs::create_dir(&checkout).unwrap();
    std::fs::write(checkout.join("stale"), b"stale").unwrap();
    repo.checkout_tree(envelope, &checkout).unwrap();
    assert!(!checkout.join("stale").exists());
    assert_eq!(
        std::fs::read(checkout.join("metadata/state")).unwrap(),
        b"binary\0\xff\n"
    );
    drop(repo);
    assert_eq!(
        TreeGit::open(temp.path().join("repo.git"))
            .unwrap()
            .head("refs/heads/a")
            .unwrap(),
        Some(merged)
    );
}

#[test]
fn migrates_existing_refs_without_losing_history() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("old.git");
    assert!(
        Command::new("git")
            .args(["init", "--bare", "--ref-format=files"])
            .arg(&path)
            .status()
            .unwrap()
            .success()
    );
    let repo = git2::Repository::open_bare(&path).unwrap();
    let tree = repo.treebuilder(None).unwrap().write().unwrap();
    let sig = git2::Signature::now("test", "test@example.com").unwrap();
    let commit = repo
        .commit(
            Some("refs/heads/existing"),
            &sig,
            &sig,
            "existing",
            &repo.find_tree(tree).unwrap(),
            &[],
        )
        .unwrap();
    drop(repo);
    let migrated = TreeGit::open(&path).unwrap();
    assert_eq!(migrated.head("refs/heads/existing").unwrap(), Some(commit));
    assert_eq!(migrated.read_commit(commit).unwrap().tree, tree);
}
