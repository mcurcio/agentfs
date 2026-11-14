#![allow(
    clippy::disallowed_methods,
    reason = "a test reads the store History writes"
)]
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
    repo.checkout_tree(envelope, &checkout, None).unwrap();
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

/// From a checkout of one tree to the next, only entries that differ are
/// written: an unchanged file is not touched, a removed one goes, and a
/// changed subtree is updated inside.
#[test]
fn an_incremental_checkout_rewrites_only_what_differs() {
    let temp = tempfile::tempdir().unwrap();
    let repo = TreeGit::init(temp.path().join("repo.git")).unwrap();
    let entry = |name: &str, mode: u32, oid: Oid| TreeEntry {
        name: name.as_bytes().to_vec(),
        mode,
        oid,
    };
    let blob = |bytes: &[u8]| repo.write_blob(bytes).unwrap();
    let inner = repo
        .write_tree(&[entry("x", 0o100644, blob(b"x1"))])
        .unwrap();
    let before = repo
        .write_tree(&[
            entry("same", 0o100644, blob(b"same")),
            entry("gone", 0o100644, blob(b"gone")),
            entry("changed", 0o100644, blob(b"old")),
            entry("dir", 0o40000, inner),
        ])
        .unwrap();
    let inner = repo
        .write_tree(&[entry("x", 0o100644, blob(b"x2"))])
        .unwrap();
    let after = repo
        .write_tree(&[
            entry("same", 0o100644, blob(b"same")),
            entry("changed", 0o100644, blob(b"new")),
            entry("added", 0o100755, blob(b"added")),
            entry("dir", 0o40000, inner),
        ])
        .unwrap();
    let checkout = temp.path().join("cache");
    repo.checkout_tree(before, &checkout, None).unwrap();
    // Not what the tree holds: only a rewrite of this entry would undo it.
    std::fs::write(checkout.join("same"), b"untouched").unwrap();
    repo.checkout_tree(after, &checkout, Some(before)).unwrap();
    let read = |name: &str| std::fs::read(checkout.join(name)).unwrap();
    assert_eq!(read("same"), b"untouched");
    assert!(!checkout.join("gone").exists());
    assert_eq!(read("changed"), b"new");
    assert_eq!(read("added"), b"added");
    assert_eq!(read("dir/x"), b"x2");
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(checkout.join("added"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o755);
    // A whole checkout of the same tree gives the same files.
    let whole = temp.path().join("whole");
    repo.checkout_tree(after, &whole, None).unwrap();
    let mut names: Vec<_> = std::fs::read_dir(&checkout)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    let mut whole_names: Vec<_> = std::fs::read_dir(&whole)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    names.sort();
    whole_names.sort();
    assert_eq!(names, whole_names);
}

/// A commit written in this process is the one `git commit-tree` writes for
/// the same tree, parents, identity, date and message.
#[test]
fn a_commit_is_the_one_git_itself_would_make() {
    use std::io::Write;
    use std::process::Stdio;
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("repo.git");
    let repo = TreeGit::init(&path).unwrap();
    let tree = repo.write_tree(&[]).unwrap();
    let first = repo.write_commit(tree, &[], "first\n").unwrap();
    let message = "Subject\n\nBody\nAgentfs-Event: 7\n";
    let mine = repo.write_commit(tree, &[first], message).unwrap();
    let raw = git(&path, &["cat-file", "commit", &mine.to_string()]).stdout;
    let raw = String::from_utf8(raw).unwrap();
    let author = raw.lines().find_map(|l| l.strip_prefix("author ")).unwrap();
    let date = author.split_once("> ").unwrap().1.to_owned();
    let mut child = Command::new("git")
        .arg("--git-dir")
        .arg(&path)
        .args(["commit-tree", &tree.to_string(), "-p", &first.to_string()])
        .env("GIT_AUTHOR_NAME", "agentfs")
        .env("GIT_AUTHOR_EMAIL", "agentfs@localhost")
        .env("GIT_COMMITTER_NAME", "agentfs")
        .env("GIT_COMMITTER_EMAIL", "agentfs@localhost")
        .env("GIT_AUTHOR_DATE", &date)
        .env("GIT_COMMITTER_DATE", &date)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(message.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert_eq!(
        String::from_utf8(out.stdout).unwrap().trim(),
        mine.to_string()
    );
    let fsck = git(&path, &["fsck", "--full", "--strict", "--no-progress"]);
    assert!(fsck.status.success(), "{fsck:?}");
    // Git's checks hold: a commit names a tree it stores and commits as parents.
    assert!(repo.write_commit(first, &[], "x\n").is_err());
    assert!(repo.write_commit(tree, &[tree], "x\n").is_err());
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
    let merges = [("fork/7".to_owned(), "refs/heads/root".to_owned())]
        .into_iter()
        .collect();
    repo.write_config("refs/heads/root", &merges).unwrap();
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
    repo.write_config("refs/heads/root", &Default::default())
        .unwrap();
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
    repo.write_config("refs/heads/root", &Default::default())
        .unwrap();
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
