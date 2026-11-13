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

fn commit(repo: &TreeGit) -> Oid {
    let tree = repo.write_tree(&[]).unwrap();
    repo.write_commit(tree, &[], "initial\n").unwrap()
}

fn git(path: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .arg("--git-dir")
        .arg(path)
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn a_tree_is_the_one_git_itself_would_make() {
    use std::io::Write;
    use std::process::Stdio;
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("repo.git");
    let repo = TreeGit::init(&path).unwrap();
    let blob = repo.write_blob(b"bytes").unwrap();
    let subtree = repo.write_tree(&[]).unwrap();
    // Out of order, and with a directory that sorts as if it ended in a slash.
    let entries = [
        ("z", 0o100755, "blob", blob),
        ("a.b", 0o100644, "blob", blob),
        ("a", 0o40000, "tree", subtree),
        ("a-b", 0o100644, "blob", blob),
        ("tab\tline", 0o100644, "blob", blob),
    ];
    let mine = repo
        .write_tree(
            &entries
                .iter()
                .map(|(name, mode, _, oid)| TreeEntry {
                    name: name.as_bytes().to_vec(),
                    mode: *mode,
                    oid: *oid,
                })
                .collect::<Vec<_>>(),
        )
        .unwrap();
    let mut child = Command::new("git")
        .arg("--git-dir")
        .arg(&path)
        .args(["mktree", "-z"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    for (name, mode, kind, oid) in &entries {
        write!(stdin, "{mode:o} {kind} {oid}\t{name}\0").unwrap();
    }
    drop(stdin);
    let out = child.wait_with_output().unwrap();
    assert_eq!(
        String::from_utf8(out.stdout).unwrap().trim(),
        mine.to_string()
    );
    let fsck = git(&path, &["fsck", "--full", "--strict", "--no-progress"]);
    assert!(fsck.status.success(), "{fsck:?}");
}

#[test]
fn write_config_replaces_a_damaged_config_and_head_before_any_git_command() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("repo.git");
    drop(TreeGit::init(&path).unwrap());
    std::fs::write(path.join("config"), b"").unwrap();
    std::fs::write(path.join("HEAD"), b"").unwrap();
    let repo = TreeGit::open(&path).unwrap();
    let upstreams = [("fork/7".to_owned(), "root".to_owned())]
        .into_iter()
        .collect();
    repo.write_config(&upstreams).unwrap();
    let get = |key: &str| {
        let out = git(&path, &["config", "--get", key]);
        assert!(out.status.success(), "{key}");
        String::from_utf8(out.stdout).unwrap()
    };
    assert_eq!(get("core.fsync"), "objects,derived-metadata\n");
    assert_eq!(get("core.fsyncMethod"), "fsync\n");
    assert_eq!(get("core.bare"), "true\n");
    assert_eq!(get("branch.fork/7.remote"), ".\n");
    assert_eq!(get("branch.fork/7.merge"), "refs/heads/root\n");
    assert_eq!(
        std::fs::read(path.join("HEAD")).unwrap(),
        b"ref: refs/heads/root\n"
    );
    assert!(git(&path, &["rev-parse", "--git-dir"]).status.success());
    // Files format, so libgit2 can open what the actor's copy leaves.
    let tip = commit(&repo);
    repo.rebuild_refs(&[update("refs/heads/root", Some(tip), None)])
        .unwrap();
    assert_eq!(
        git2::Repository::open_bare(&path)
            .unwrap()
            .head()
            .unwrap()
            .target(),
        Some(tip)
    );
}

#[test]
fn rebuild_refs_replaces_everything_with_what_it_is_given() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("repo.git");
    let repo = TreeGit::init(&path).unwrap();
    repo.write_config(&Default::default()).unwrap();
    let tip = commit(&repo);
    let blob = repo.write_blob(b"table").unwrap();
    repo.ref_transaction(&[
        update("refs/heads/root", Some(tip), None),
        update("refs/heads/gone", Some(tip), None),
        update("refs/queue/root/1", Some(tip), None),
    ])
    .unwrap();
    assert!(git(&path, &["pack-refs", "--all"]).status.success());
    std::fs::write(path.join("refs/heads/garbage"), b"not a ref\n").unwrap();
    std::fs::write(path.join("refs/heads/root.lock"), b"stale").unwrap();
    repo.rebuild_refs(&[
        update("refs/heads/root", Some(tip), None),
        update("refs/agentfs/objects/root", Some(blob), None),
    ])
    .unwrap();
    assert_eq!(
        repo.list_refs("refs/").unwrap(),
        vec![
            ("refs/agentfs/objects/root".to_owned(), blob),
            ("refs/heads/root".to_owned(), tip),
        ]
    );
    assert!(!path.join("packed-refs").exists());
    let fsck = git(&path, &["fsck", "--full", "--strict", "--no-progress"]);
    assert!(fsck.status.success(), "{fsck:?}");
}

#[test]
fn missing_objects_names_what_is_lost_of_a_tip_and_not_its_parents() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("repo.git");
    let repo = TreeGit::init(&path).unwrap();
    repo.write_config(&Default::default()).unwrap();
    let old_blob = repo.write_blob(b"parent's").unwrap();
    let old_tree = repo
        .write_tree(&[TreeEntry {
            name: b"a".to_vec(),
            mode: 0o100644,
            oid: old_blob,
        }])
        .unwrap();
    let parent = repo.write_commit(old_tree, &[], "parent\n").unwrap();
    let blob = repo.write_blob(b"tip's").unwrap();
    let tree = repo
        .write_tree(&[TreeEntry {
            name: b"a".to_vec(),
            mode: 0o100644,
            oid: blob,
        }])
        .unwrap();
    let tip = repo.write_commit(tree, &[parent], "tip\n").unwrap();
    let table = repo.write_blob(b"table").unwrap();
    assert_eq!(repo.missing_objects(&[tip, table]).unwrap(), vec![]);
    let lose = |oid: Oid| {
        let name = oid.to_string();
        std::fs::remove_file(path.join("objects").join(&name[..2]).join(&name[2..])).unwrap();
    };
    lose(old_blob);
    assert_eq!(repo.missing_objects(&[tip, table]).unwrap(), vec![]);
    lose(blob);
    assert_eq!(repo.missing_objects(&[tip, table]).unwrap(), vec![blob]);
    lose(table);
    let mut lost = repo.missing_objects(&[tip, table]).unwrap();
    lost.sort();
    let mut expected = vec![blob, table];
    expected.sort();
    assert_eq!(lost, expected);
}
