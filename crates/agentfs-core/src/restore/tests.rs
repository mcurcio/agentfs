//! Plans against real history, checks and applies against real trees.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::PathBuf;

use git2::{Index, IndexEntry, IndexTime, Signature};

use super::*;

const BRANCH: &str = "refs/heads/agentfs/vol";

#[derive(Debug, Clone, PartialEq, Eq)]
enum F {
    File(&'static str),
    Exec(&'static str),
    Link(&'static str),
}

use F::{Exec, File, Link};

struct Fixture {
    _dir: tempfile::TempDir,
    repo: Repository,
    tree: PathBuf,
}

/// Allows everything but the paths it lists.
#[derive(Default)]
struct Policy {
    refuse: Vec<&'static str>,
}

impl RecordPolicy for Policy {
    fn refuses(&self, path: &RepoPath, _: Option<&str>) -> io::Result<Option<String>> {
        Ok(self
            .refuse
            .contains(&path.as_str())
            .then(|| "never recorded (-agentfs)".to_owned()))
    }

    fn check_content(&self, path: &RepoPath, content: &[u8]) -> Option<Refusal> {
        crate::checks::Checks::default().check_blob(path, content)
    }
}

/// Reports the listed paths as open for writing.
struct Open(Vec<&'static str>);

impl Liveness for Open {
    fn open_for_writing(&self, path: &RepoPath) -> bool {
        self.0.contains(&path.as_str())
    }
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init_bare(dir.path().join("git")).unwrap();
        let tree = dir.path().join("tree");
        fs::create_dir_all(&tree).unwrap();
        Self {
            _dir: dir,
            repo,
            tree,
        }
    }

    /// Commits exactly `files` on the branch (or on `branch`).
    fn commit_on(&self, branch: &str, files: &[(&str, F)], message: &str) -> Oid {
        let mut index = Index::new().unwrap();
        for (path, f) in files {
            let (content, mode) = match f {
                File(c) => (*c, 0o100644),
                Exec(c) => (*c, 0o100755),
                Link(t) => (*t, 0o120000),
            };
            let id = self.repo.blob(content.as_bytes()).unwrap();
            index
                .add(&IndexEntry {
                    ctime: IndexTime::new(0, 0),
                    mtime: IndexTime::new(0, 0),
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
                })
                .unwrap();
        }
        let tree = self
            .repo
            .find_tree(index.write_tree_to(&self.repo).unwrap())
            .unwrap();
        let sig = Signature::now("t", "t@example.com").unwrap();
        let parent = self
            .repo
            .find_reference(branch)
            .ok()
            .map(|r| r.peel_to_commit().unwrap());
        let parents: Vec<&git2::Commit<'_>> = parent.iter().collect();
        let id = self
            .repo
            .commit(None, &sig, &sig, message, &tree, &parents)
            .unwrap();
        self.repo.reference(branch, id, true, "test").unwrap();
        id
    }

    fn commit(&self, files: &[(&str, F)], message: &str) -> Oid {
        self.commit_on(BRANCH, files, message)
    }

    /// Makes the tree hold exactly `files` (plus nothing else).
    fn checkout(&self, files: &[(&str, F)]) {
        fs::remove_dir_all(&self.tree).unwrap();
        fs::create_dir_all(&self.tree).unwrap();
        for (path, f) in files {
            let p = self.tree.join(path);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            match f {
                File(c) | Exec(c) => {
                    fs::write(&p, c).unwrap();
                    let mode = if matches!(f, Exec(_)) { 0o755 } else { 0o644 };
                    fs::set_permissions(&p, fs::Permissions::from_mode(mode)).unwrap();
                }
                Link(t) => symlink(t, &p).unwrap(),
            }
        }
    }

    /// The tree's files, symlinks and their kinds.
    fn read_tree(&self) -> BTreeMap<String, F> {
        let dir = Dir::open(&self.tree).unwrap();
        let mut out = BTreeMap::new();
        for path in dir.list_tree("").unwrap() {
            let p = self.tree.join(&path);
            let meta = fs::symlink_metadata(&p).unwrap();
            let leak = |s: String| -> &'static str { Box::leak(s.into_boxed_str()) };
            let f = if meta.file_type().is_symlink() {
                Link(leak(fs::read_link(&p).unwrap().display().to_string()))
            } else if meta.mode() & 0o111 != 0 {
                Exec(leak(fs::read_to_string(&p).unwrap()))
            } else {
                File(leak(fs::read_to_string(&p).unwrap()))
            };
            out.insert(path, f);
        }
        out
    }

    fn plan(&self, request: &Request) -> Result<Plan, RestoreError> {
        plan(&self.repo, BRANCH, request)
    }

    fn restore(&self, path: &str, commit: Oid) -> Request {
        Request::Restore {
            spec: Pathspec::parse(path).unwrap(),
            commit: commit.to_string(),
        }
    }

    fn revert(&self, commit: Oid) -> Request {
        Request::Revert {
            commit: commit.to_string(),
        }
    }

    fn dir(&self) -> Dir {
        Dir::open(&self.tree).unwrap()
    }

    /// Plan, check (no force) and apply.
    fn run(&self, request: &Request) -> Result<Applied, RestoreError> {
        let plan = self.plan(request)?;
        let plan = check(
            plan,
            &self.repo,
            &Policy::default(),
            &self.dir(),
            &NotLive,
            false,
        )?;
        apply(&plan, &self.repo, &self.dir()).map_err(|e| RestoreError::Io(e.error))
    }
}

fn tree_of(files: &[(&str, F)]) -> BTreeMap<String, F> {
    files
        .iter()
        .map(|(p, f)| ((*p).to_owned(), f.clone()))
        .collect()
}

fn paths(steps: &[Step]) -> Vec<(&str, &'static str)> {
    steps
        .iter()
        .map(|s| (s.path.as_str(), s.action()))
        .collect()
}

#[test]
fn pathspecs() {
    for root in [".", "", "./", "/"] {
        assert_eq!(Pathspec::parse(root).unwrap(), Pathspec::Root, "{root:?}");
    }
    assert_eq!(
        Pathspec::parse("./a/b/").unwrap(),
        Pathspec::Path(RepoPath::new("a/b").unwrap())
    );
    for bad in ["../x", "a/../b", ".git/config", "/abs"] {
        assert!(
            matches!(Pathspec::parse(bad), Err(RestoreError::InvalidPath { .. })),
            "{bad}"
        );
    }
}

#[test]
fn restore_a_file_directory_root_and_symlink() {
    let fx = Fixture::new();
    let v1 = [
        ("a.txt", File("one")),
        ("d/x", File("x1")),
        ("d/sub/y", Exec("y1")),
        ("link", Link("a.txt")),
    ];
    let c1 = fx.commit(&v1, "v1");
    let v2 = [
        ("a.txt", File("two")),
        ("d/x", File("x2")),
        ("d/new", File("n")),
        ("link", Link("d/x")),
        ("other", File("o")),
    ];
    fx.commit(&v2, "v2");

    // A file.
    let plan = fx.plan(&fx.restore("a.txt", c1)).unwrap();
    assert_eq!(plan.kind, OperationKind::Restore);
    assert_eq!(plan.source, c1);
    assert!(
        plan.summary.starts_with("restore a.txt from "),
        "{}",
        plan.summary
    );
    assert_eq!(paths(&plan.steps), [("a.txt", "write")]);
    // A directory: written, recreated and removed paths.
    let plan = fx.plan(&fx.restore("d", c1)).unwrap();
    assert_eq!(
        paths(&plan.steps),
        [("d/new", "remove"), ("d/sub/y", "write"), ("d/x", "write")]
    );
    assert_eq!(plan.prune_floor, "");
    // A symlink.
    let plan = fx.plan(&fx.restore("link", c1)).unwrap();
    assert_eq!(paths(&plan.steps), [("link", "symlink")]);
    // The whole tree.
    fx.checkout(&v2);
    fx.run(&fx.restore(".", c1)).unwrap();
    assert_eq!(fx.read_tree(), tree_of(&v1));
    // Already matching: an empty plan after the check.
    let plan = fx.plan(&fx.restore(".", c1)).unwrap();
    let checked = check(
        plan,
        &fx.repo,
        &Policy::default(),
        &fx.dir(),
        &NotLive,
        false,
    )
    .unwrap();
    assert!(checked.steps.is_empty());
}

#[test]
fn restore_a_mode_change_only() {
    let fx = Fixture::new();
    let c1 = fx.commit(&[("s.sh", Exec("echo"))], "v1");
    let c2 = fx.commit(&[("s.sh", File("echo"))], "v2");
    fx.checkout(&[("s.sh", File("echo"))]);
    let path = fx.tree.join("s.sh");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
    let ino = fs::metadata(&path).unwrap().ino();
    let plan = fx.plan(&fx.restore("s.sh", c1)).unwrap();
    assert_eq!(paths(&plan.steps), [("s.sh", "mode")]);
    let applied = fx.run(&fx.restore("s.sh", c1)).unwrap();
    assert_eq!(applied.written.len(), 1);
    let meta = fs::metadata(&path).unwrap();
    assert_eq!(meta.ino(), ino, "chmod only");
    assert_eq!(meta.mode() & 0o7777, 0o750, "x where r was");
    // Recorded (here: committed by hand), then back: x bits cleared, the
    // other bits kept.
    fx.commit(&[("s.sh", Exec("echo"))], "v3");
    fx.run(&fx.restore("s.sh", c2)).unwrap();
    assert_eq!(fs::metadata(&path).unwrap().mode() & 0o7777, 0o640);
}

#[test]
fn plan_refusals() {
    let fx = Fixture::new();
    let c1 = fx.commit(&[("a", File("1"))], "v1");
    assert!(matches!(
        fx.plan(&Request::Revert {
            commit: "nonsense".into()
        }),
        Err(RestoreError::UnknownCommit { .. })
    ));
    assert!(matches!(
        fx.plan(&fx.restore("missing", c1)),
        Err(RestoreError::NothingAtPath { .. })
    ));
    // A commit on another branch.
    let other = fx.commit_on("refs/heads/elsewhere", &[("a", File("x"))], "x");
    let err = fx.plan(&fx.restore("a", other)).unwrap_err();
    assert!(matches!(err, RestoreError::NotInHistory { .. }), "{err}");
    assert!(err.is_refusal() && !err.overridable());
    assert!(err.hint().unwrap().contains("plain git"));
    // HEAD is the volume's branch.
    let plan = fx
        .plan(&Request::Restore {
            spec: Pathspec::Root,
            commit: "HEAD~0".into(),
        })
        .unwrap();
    assert_eq!(plan.source, c1);
    // An abbreviated id and a ref work.
    let short = &c1.to_string()[..10];
    assert!(
        fx.plan(&Request::Restore {
            spec: Pathspec::Root,
            commit: short.into()
        })
        .is_ok()
    );
    assert!(
        fx.plan(&Request::Restore {
            spec: Pathspec::Root,
            commit: format!("{BRANCH}~0")
        })
        .is_ok()
    );
}

#[test]
fn revert_adds_modifies_deletes_and_type_changes() {
    let fx = Fixture::new();
    let root = fx.commit(
        &[
            ("keep", File("k")),
            ("mod", File("m1")),
            ("gone", File("g")),
        ],
        "root",
    );
    let c = fx.commit(
        &[
            ("keep", File("k")),
            ("mod", File("m2")),
            ("added", File("a")),
            ("t", Link("keep")),
        ],
        "change things",
    );
    fx.checkout(&[
        ("keep", File("k")),
        ("mod", File("m2")),
        ("added", File("a")),
        ("t", Link("keep")),
    ]);
    let plan = fx.plan(&fx.revert(c)).unwrap();
    assert!(plan.summary.starts_with("revert ") && plan.summary.ends_with(": change things"));
    assert_eq!(
        paths(&plan.steps),
        [
            ("added", "remove"),
            ("gone", "write"),
            ("mod", "write"),
            ("t", "remove")
        ]
    );
    assert!(plan.changed_since.is_empty());
    fx.run(&fx.revert(c)).unwrap();
    assert_eq!(
        fx.read_tree(),
        tree_of(&[
            ("gone", File("g")),
            ("keep", File("k")),
            ("mod", File("m1"))
        ])
    );

    // The root commit reverts to nothing.
    let plan = fx.plan(&fx.revert(root)).unwrap();
    assert!(plan.steps.iter().all(|s| s.target.is_none()));

    // A type change: file to symlink and back.
    let fx = Fixture::new();
    fx.commit(&[("p", File("x"))], "file");
    let c = fx.commit(&[("p", Link("x"))], "link");
    fx.checkout(&[("p", Link("x"))]);
    fx.run(&fx.revert(c)).unwrap();
    assert_eq!(fx.read_tree(), tree_of(&[("p", File("x"))]));

    // A file that became a directory, reverted.
    let fx = Fixture::new();
    fx.commit(&[("p", File("file"))], "file");
    let c = fx.commit(&[("p/x", File("in dir"))], "dir");
    fx.checkout(&[("p/x", File("in dir"))]);
    fx.run(&fx.revert(c)).unwrap();
    assert_eq!(fx.read_tree(), tree_of(&[("p", File("file"))]));
}

#[test]
fn revert_refusals() {
    let fx = Fixture::new();
    let a = fx.commit(&[("a", File("1"))], "a");
    let same = fx.commit(&[("a", File("1"))], "nothing");
    assert!(matches!(
        fx.plan(&fx.revert(same)),
        Err(RestoreError::NothingToRevert { .. })
    ));
    // A merge commit.
    let sig = Signature::now("t", "t@example.com").unwrap();
    let head = fx.repo.find_commit(same).unwrap();
    let other = fx.repo.find_commit(a).unwrap();
    let merge = fx
        .repo
        .commit(
            None,
            &sig,
            &sig,
            "merge",
            &head.tree().unwrap(),
            &[&head, &other],
        )
        .unwrap();
    fx.repo.reference(BRANCH, merge, true, "merge").unwrap();
    assert!(matches!(
        fx.plan(&fx.revert(merge)),
        Err(RestoreError::MergeCommit { .. })
    ));
}

#[test]
fn revert_of_a_path_changed_since_needs_force() {
    let fx = Fixture::new();
    fx.commit(&[("f", File("1")), ("g", File("1"))], "one");
    let c = fx.commit(&[("f", File("2")), ("g", File("2"))], "two");
    let later = [("f", File("3")), ("g", File("2"))];
    fx.commit(&later, "three");
    fx.checkout(&later);
    let plan = fx.plan(&fx.revert(c)).unwrap();
    assert_eq!(plan.changed_since, [RepoPath::new("f").unwrap()]);
    let err = check(
        plan.clone(),
        &fx.repo,
        &Policy::default(),
        &fx.dir(),
        &NotLive,
        false,
    )
    .unwrap_err();
    assert!(matches!(&err, RestoreError::ChangedSince { paths } if paths == &["f"]));
    assert!(err.overridable());
    let plan = check(
        plan,
        &fx.repo,
        &Policy::default(),
        &fx.dir(),
        &NotLive,
        true,
    )
    .unwrap();
    apply(&plan, &fx.repo, &fx.dir()).unwrap();
    assert_eq!(
        fx.read_tree(),
        tree_of(&[("f", File("1")), ("g", File("1"))])
    );
}

#[test]
fn check_refusals() {
    let fx = Fixture::new();
    let c1 = fx.commit(&[("a", File("1")), ("d/x", File("x"))], "v1");
    fx.commit(&[("a", File("2")), ("d/x", File("x2"))], "v2");

    // A change not in history: never overridable.
    fx.checkout(&[("a", File("unrecorded")), ("d/x", File("x2"))]);
    for force in [false, true] {
        let plan = fx.plan(&fx.restore("a", c1)).unwrap();
        let err = check(
            plan,
            &fx.repo,
            &Policy::default(),
            &fx.dir(),
            &NotLive,
            force,
        )
        .unwrap_err();
        assert!(
            matches!(&err, RestoreError::Unrecorded { path } if path == "a"),
            "{err}"
        );
        assert!(!err.overridable());
        assert!(err.hint().unwrap().contains("--force does not"));
    }
    // A tree that matches the branch head is fine.
    fx.checkout(&[("a", File("2"))]);
    fs::create_dir(fx.tree.join("d")).unwrap();
    fs::write(fx.tree.join("d/x"), "x2").unwrap();
    assert!(fx.run(&fx.restore("d", c1)).is_ok(), "matches head: fine");

    // An untracked file where a restored file goes: in the way.
    let fx = Fixture::new();
    let c1 = fx.commit(&[("a", File("1")), ("n", File("new"))], "v1");
    fx.commit(&[("a", File("1"))], "v2");
    fx.checkout(&[("a", File("1")), ("n", File("untracked"))]);
    let err = check(
        fx.plan(&fx.restore(".", c1)).unwrap(),
        &fx.repo,
        &Policy::default(),
        &fx.dir(),
        &NotLive,
        true,
    )
    .unwrap_err();
    assert!(
        matches!(&err, RestoreError::InTheWay { path, what: "a file" } if path == "n"),
        "{err}"
    );
    // ... unless it already holds the restored content.
    fs::write(fx.tree.join("n"), "new").unwrap();
    let plan = check(
        fx.plan(&fx.restore(".", c1)).unwrap(),
        &fx.repo,
        &Policy::default(),
        &fx.dir(),
        &NotLive,
        false,
    )
    .unwrap();
    assert!(plan.steps.is_empty());
}

#[test]
fn check_ancestors_and_policies() {
    let fx = Fixture::new();
    let c1 = fx.commit(&[("d/x", File("x")), ("s/y", File("y"))], "v1");
    fx.commit(&[], "empty");
    let run_check = |request: &Request, policy: &Policy, live: &dyn Liveness, force| {
        check(
            fx.plan(request).unwrap(),
            &fx.repo,
            policy,
            &fx.dir(),
            live,
            force,
        )
    };
    let none = Policy::default();

    // A file where a directory goes.
    fx.checkout(&[("d", File("untracked"))]);
    let err = run_check(&fx.restore("d", c1), &none, &NotLive, true).unwrap_err();
    assert!(
        matches!(&err, RestoreError::InTheWay { path, what: "a file" } if path == "d"),
        "{err}"
    );
    // A symlinked ancestor: never followed.
    let outside = tempfile::tempdir().unwrap();
    fx.checkout(&[]);
    symlink(outside.path(), fx.tree.join("s")).unwrap();
    let err = run_check(&fx.restore("s", c1), &none, &NotLive, true).unwrap_err();
    assert!(
        matches!(&err, RestoreError::InTheWay { path, what: "a symlink" } if path == "s"),
        "{err}"
    );
    assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
    // Something the volume would not record.
    fx.checkout(&[]);
    let refuse = Policy {
        refuse: vec!["d/x"],
    };
    let err = run_check(&fx.restore(".", c1), &refuse, &NotLive, true).unwrap_err();
    assert!(
        matches!(&err, RestoreError::Unrecordable { path, reason } if path == "d/x" && reason.contains("-agentfs")),
        "{err}"
    );
    // Content the checks refuse.
    let fx2 = Fixture::new();
    let attrs: &'static str = Box::leak(format!("x\n{}\n", "y".repeat(3000)).into_boxed_str());
    let c = fx2.commit(&[(".gitattributes", File(attrs))], "attrs");
    fx2.commit(&[], "empty");
    let err = check(
        fx2.plan(&fx2.restore(".", c)).unwrap(),
        &fx2.repo,
        &none,
        &fx2.dir(),
        &NotLive,
        true,
    )
    .unwrap_err();
    assert!(matches!(err, RestoreError::Unrecordable { .. }), "{err}");

    // A file open for writing through a mount: --force writes anyway.
    let fx = Fixture::new();
    let c1 = fx.commit(&[("f", File("1"))], "v1");
    fx.commit(&[("f", File("2"))], "v2");
    fx.checkout(&[("f", File("2"))]);
    let open = Open(vec!["f"]);
    let plan = fx.plan(&fx.restore("f", c1)).unwrap();
    let err = check(
        plan.clone(),
        &fx.repo,
        &Policy::default(),
        &fx.dir(),
        &open,
        false,
    )
    .unwrap_err();
    assert!(matches!(&err, RestoreError::OpenForWriting { paths } if paths == &["f"]));
    assert!(err.overridable());
    assert!(check(plan, &fx.repo, &Policy::default(), &fx.dir(), &open, true).is_ok());
}

#[test]
fn apply_keeps_the_inode_mode_and_xattrs() {
    let fx = Fixture::new();
    let c1 = fx.commit(&[("f", File("old content"))], "v1");
    fx.commit(&[("f", File("new"))], "v2");
    fx.checkout(&[("f", File("new"))]);
    let path = fx.tree.join("f");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o604)).unwrap();
    let xattr = rustix::fs::setxattr(
        &path,
        "user.agentfs-test",
        b"v",
        rustix::fs::XattrFlags::empty(),
    )
    .is_ok();
    let ino = fs::metadata(&path).unwrap().ino();
    let applied = fx.run(&fx.restore("f", c1)).unwrap();
    assert_eq!(applied.written, [RepoPath::new("f").unwrap()]);
    let meta = fs::metadata(&path).unwrap();
    assert_eq!(meta.ino(), ino);
    assert_eq!(meta.mode() & 0o7777, 0o604);
    assert_eq!(fs::read(&path).unwrap(), b"old content");
    if xattr {
        let mut buf = [0u8; 8];
        let n = rustix::fs::getxattr(&path, "user.agentfs-test", &mut buf).unwrap();
        assert_eq!(&buf[..n], b"v");
    }
}

#[test]
fn apply_replaces_a_hard_linked_file() {
    let fx = Fixture::new();
    let c1 = fx.commit(&[("f", File("one")), ("g", File("two"))], "v1");
    fx.commit(&[("f", File("two")), ("g", File("two"))], "v2");
    fx.checkout(&[("g", File("two"))]);
    fs::hard_link(fx.tree.join("g"), fx.tree.join("f")).unwrap();
    fx.run(&fx.restore("f", c1)).unwrap();
    assert_eq!(fs::read(fx.tree.join("f")).unwrap(), b"one");
    assert_eq!(
        fs::read(fx.tree.join("g")).unwrap(),
        b"two",
        "the other link is untouched"
    );
    assert_eq!(fs::metadata(fx.tree.join("g")).unwrap().nlink(), 1);
}

#[test]
fn removals_prune_empty_directories_and_keep_untracked_files() {
    let fx = Fixture::new();
    let c1 = fx.commit(&[("keep", File("k"))], "v1");
    fx.commit(
        &[
            ("keep", File("k")),
            ("a/b/c", File("c")),
            ("u/t", File("t")),
        ],
        "v2",
    );
    fx.checkout(&[
        ("keep", File("k")),
        ("a/b/c", File("c")),
        ("u/t", File("t")),
        ("u/untracked", File("mine")),
    ]);
    let applied = fx.run(&fx.restore(".", c1)).unwrap();
    assert_eq!(applied.removed.len(), 2);
    assert!(!fx.tree.join("a").exists(), "emptied directories pruned");
    assert_eq!(
        fx.read_tree(),
        tree_of(&[("keep", File("k")), ("u/untracked", File("mine"))])
    );
    // A re-run changes nothing.
    let plan = fx.plan(&fx.restore(".", c1)).unwrap();
    let plan = check(
        plan,
        &fx.repo,
        &Policy::default(),
        &fx.dir(),
        &NotLive,
        false,
    )
    .unwrap();
    assert!(plan.steps.is_empty());
}

#[test]
fn restoring_a_path_never_prunes_its_parent() {
    let fx = Fixture::new();
    let c1 = fx.commit(&[("q", File("1"))], "v1");
    fx.commit(&[("q", File("1")), ("p/d/x", File("x"))], "v2");
    fx.checkout(&[("q", File("1")), ("p/d/x", File("x"))]);
    // Restoring p/d from a commit without it removes p/d/x and prunes p/d,
    // never p, the restored path's parent.
    let applied = fx.run(&fx.restore("p/d", c1)).unwrap();
    assert_eq!(applied.removed, [RepoPath::new("p/d/x").unwrap()]);
    assert!(fx.tree.join("p").is_dir());
    assert!(!fx.tree.join("p/d").exists());
}

#[test]
fn new_files_and_directories_take_the_parents_owner() {
    if !crate::confined::can_chown() {
        eprintln!("skipped: needs CAP_CHOWN");
        return;
    }
    let fx = Fixture::new();
    let c1 = fx.commit(&[("d/new/f", File("f")), ("l", Link("d"))], "v1");
    fx.commit(&[], "empty");
    fx.checkout(&[]);
    fs::create_dir(fx.tree.join("d")).unwrap();
    rustix::fs::chown(
        fx.tree.join("d"),
        Some(rustix::fs::Uid::from_raw(10000)),
        Some(rustix::fs::Gid::from_raw(10001)),
    )
    .unwrap();
    fx.run(&fx.restore(".", c1)).unwrap();
    for rel in ["d/new", "d/new/f"] {
        let meta = fs::symlink_metadata(fx.tree.join(rel)).unwrap();
        assert_eq!((meta.uid(), meta.gid()), (10000, 10001), "{rel}");
    }
    assert_eq!(
        fs::metadata(fx.tree.join("d/new")).unwrap().mode() & 0o7777,
        0o755
    );
}

#[test]
fn unrecorded_changes_where_history_already_matches_are_refused() {
    let fx = Fixture::new();
    let c1 = fx.commit(&[("a", File("1")), ("b", File("b"))], "v1");
    fx.commit(&[("a", File("2")), ("b", File("b"))], "v2");
    let c3 = fx.commit(&[("a", File("1")), ("b", File("b"))], "v3");
    let _ = c3;
    // History already has `a` as in v1, but the tree changed it without a
    // record: not "already matches", and not overwritten.
    fx.checkout(&[("a", File("unrecorded")), ("b", File("b"))]);
    for spec in ["a", "."] {
        let err = fx.run(&fx.restore(spec, c1)).unwrap_err();
        assert!(
            matches!(&err, RestoreError::Unrecorded { path } if path == "a"),
            "{err}"
        );
    }
    // A path the rules now refuse is fine where nothing is written.
    fx.checkout(&[("a", File("1")), ("b", File("b"))]);
    let refuse = Policy { refuse: vec!["b"] };
    let plan = check(
        fx.plan(&fx.restore(".", c1)).unwrap(),
        &fx.repo,
        &refuse,
        &fx.dir(),
        &NotLive,
        false,
    )
    .unwrap();
    assert!(plan.steps.is_empty());
}
