//! Plans against real history, checks and applies against real trees.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::PathBuf;

use git2::{Index, IndexEntry, IndexTime, Signature};

use super::*;
use crate::lfs::fetch::LfsObjects;

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
    /// Content over this size would be stored as LFS.
    lfs_over: Option<u64>,
    checks: crate::checks::Checks,
    /// fsync-class groups (design §8), each listed whole.
    groups: Vec<&'static [&'static str]>,
}

/// No LFS object is available.
struct NoLfs;

impl LfsObjects for NoLfs {
    fn ensure(&self, pointer: &Pointer) -> Result<PathBuf, crate::lfs::LfsError> {
        Err(crate::lfs::LfsError::local(format!(
            "no object {}",
            pointer.oid
        )))
    }
}

impl RecordPolicy for Policy {
    fn group_of(&self, path: &RepoPath) -> Vec<RepoPath> {
        self.groups
            .iter()
            .find(|g| g.contains(&path.as_str()))
            .map(|g| g.iter().map(|p| RepoPath::new(*p).unwrap()).collect())
            .unwrap_or_default()
    }

    fn refuses(&self, path: &RepoPath, _: Option<&str>) -> io::Result<Option<String>> {
        Ok(self
            .refuse
            .contains(&path.as_str())
            .then(|| "never recorded (-agentfs)".to_owned()))
    }

    fn store_for(&self, _: &RepoPath, size: u64) -> io::Result<Store> {
        Ok(match self.lfs_over {
            Some(threshold) if size > threshold => Store::Lfs,
            _ => Store::Blob,
        })
    }

    fn check_content(&self, path: &RepoPath, content: &[u8], store: Store) -> Option<Refusal> {
        self.checks.check_blob_stored(path, content, store)
    }

    fn check_size(&self, path: &RepoPath, size: u64, store: Store) -> Option<Refusal> {
        self.checks.check_size(path, size, store)
    }
}

/// Reports the listed paths as open for writing.
struct Open(Vec<&'static str>);

impl Liveness for Open {
    fn open_for_writing(&self, path: &RepoPath) -> bool {
        self.0.contains(&path.as_str())
    }

    fn records_its_writes(&self) -> bool {
        true
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
            &NoLfs,
            &self.dir(),
            &NotLive,
            false,
        )?;
        apply(&plan, &self.repo, &NoLfs, &self.dir(), &NotLive, false)
            .map_err(|e| RestoreError::Io(e.error))
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
    // The tree matches but history does not (stopped before recording):
    // every changed path is kept, with nothing to write, to be recorded.
    let plan = fx.plan(&fx.restore(".", c1)).unwrap();
    let checked = check(
        plan.clone(),
        &fx.repo,
        &Policy::default(),
        &NoLfs,
        &fx.dir(),
        &NotLive,
        false,
    )
    .unwrap();
    let changed: Vec<_> = plan
        .steps
        .iter()
        .filter(|s| s.current != s.target)
        .cloned()
        .collect();
    assert_eq!(checked.steps, changed);
    let applied = apply(&checked, &fx.repo, &NoLfs, &fx.dir(), &NotLive, false).unwrap();
    assert_eq!(applied.written.len() + applied.removed.len(), changed.len());
    assert_eq!(fx.read_tree(), tree_of(&v1), "nothing rewritten");
    // Already matching, in history too: an empty plan after the check.
    fx.commit(&v1, "recorded");
    let plan = fx.plan(&fx.restore(".", c1)).unwrap();
    let checked = check(
        plan,
        &fx.repo,
        &Policy::default(),
        &NoLfs,
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
        &NoLfs,
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
        &NoLfs,
        &fx.dir(),
        &NotLive,
        true,
    )
    .unwrap();
    apply(&plan, &fx.repo, &NoLfs, &fx.dir(), &NotLive, false).unwrap();
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
            &NoLfs,
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
        &NoLfs,
        &fx.dir(),
        &NotLive,
        true,
    )
    .unwrap_err();
    assert!(
        matches!(&err, RestoreError::InTheWay { path, what: "a file" } if path == "n"),
        "{err}"
    );
    // ... unless it already holds the restored content: then it is only
    // recorded (directly), or refused as not in history (through a mount,
    // which records only what it sees written).
    fs::write(fx.tree.join("n"), "new").unwrap();
    let plan = check(
        fx.plan(&fx.restore(".", c1)).unwrap(),
        &fx.repo,
        &Policy::default(),
        &NoLfs,
        &fx.dir(),
        &NotLive,
        false,
    )
    .unwrap();
    assert_eq!(paths(&plan.steps), [("n", "write")]);
    let err = check(
        fx.plan(&fx.restore(".", c1)).unwrap(),
        &fx.repo,
        &Policy::default(),
        &NoLfs,
        &fx.dir(),
        &Open(vec![]),
        true,
    )
    .unwrap_err();
    assert!(
        matches!(&err, RestoreError::Unrecorded { path } if path == "n"),
        "{err}"
    );
}

/// The tree already holds the target but the branch head does not (a
/// restore stopped between writing and recording, or `git show > file`
/// without `agentfs record`): the path is kept so it gets recorded, never
/// reported as matching.
#[test]
fn a_tree_ahead_of_history_is_recorded_not_skipped() {
    let fx = Fixture::new();
    let c1 = fx.commit(
        &[("d/a", File("a1")), ("d/b", File("b1")), ("r", File("r"))],
        "v1",
    );
    fx.commit(
        &[("d/a", File("a2")), ("d/b", File("b2")), ("x", File("x"))],
        "v2",
    );
    // d/a written, d/b not yet; x removed and r written by an earlier run
    // of `restore .`, none of it recorded.
    fx.checkout(&[("d/a", File("a1")), ("d/b", File("b2")), ("r", File("r"))]);
    let plan = check(
        fx.plan(&fx.restore(".", c1)).unwrap(),
        &fx.repo,
        &Policy::default(),
        &NoLfs,
        &fx.dir(),
        &NotLive,
        false,
    )
    .unwrap();
    assert_eq!(
        paths(&plan.steps),
        [
            ("d/a", "write"),
            ("d/b", "write"),
            ("r", "write"),
            ("x", "remove")
        ]
    );
    let applied = apply(&plan, &fx.repo, &NoLfs, &fx.dir(), &NotLive, false).unwrap();
    let names = |v: &[RepoPath]| v.iter().map(ToString::to_string).collect::<Vec<_>>();
    assert_eq!(names(&applied.written), ["r", "d/a", "d/b"], "all recorded");
    assert_eq!(names(&applied.removed), ["x"]);
    assert_eq!(
        fx.read_tree(),
        tree_of(&[("d/a", File("a1")), ("d/b", File("b1")), ("r", File("r"))])
    );
}

/// Each path is re-checked just before it changes: a file a workload
/// changed or opened for writing after the check is left alone.
#[test]
fn apply_rechecks_each_path() {
    let fx = Fixture::new();
    let c1 = fx.commit(&[("a", File("1")), ("b", File("1"))], "v1");
    fx.commit(&[("a", File("2")), ("b", File("2"))], "v2");
    fx.checkout(&[("a", File("2")), ("b", File("2"))]);
    let checked = || {
        check(
            fx.plan(&fx.restore(".", c1)).unwrap(),
            &fx.repo,
            &Policy::default(),
            &NoLfs,
            &fx.dir(),
            &NotLive,
            false,
        )
        .unwrap()
    };
    // Changed after the check: not overwritten, whatever --force says.
    let plan = checked();
    fs::write(fx.tree.join("b"), "workload").unwrap();
    for force in [false, true] {
        let err = apply(&plan, &fx.repo, &NoLfs, &fx.dir(), &NotLive, force).unwrap_err();
        assert_eq!(err.path, "b");
        assert!(!err.started);
        assert!(
            err.error.to_string().contains("changed since the check"),
            "{}",
            err.error
        );
        assert_eq!(fs::read(fx.tree.join("b")).unwrap(), b"workload");
        assert!(err.left_changed(&plan, &fx.repo, &fx.dir()).is_none());
    }
    // Opened for writing after the check: left alone unless --force.
    fx.checkout(&[("a", File("2")), ("b", File("2"))]);
    let plan = checked();
    let err = apply(&plan, &fx.repo, &NoLfs, &fx.dir(), &Open(vec!["b"]), false).unwrap_err();
    assert_eq!((err.path.as_str(), err.started), ("b", false));
    assert!(err.error.to_string().contains("opened for writing"));
    assert_eq!(fs::read(fx.tree.join("b")).unwrap(), b"2");
    apply(&plan, &fx.repo, &NoLfs, &fx.dir(), &Open(vec!["b"]), true).unwrap();
    assert_eq!(fs::read(fx.tree.join("b")).unwrap(), b"1");
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
            &NoLfs,
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
        ..Policy::default()
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
        &NoLfs,
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
        &NoLfs,
        &fx.dir(),
        &open,
        false,
    )
    .unwrap_err();
    assert!(matches!(&err, RestoreError::OpenForWriting { paths } if paths == &["f"]));
    assert!(err.overridable());
    assert!(
        check(
            plan,
            &fx.repo,
            &Policy::default(),
            &NoLfs,
            &fx.dir(),
            &open,
            true
        )
        .is_ok()
    );
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
    // A re-run changes nothing (once history holds what was written).
    fx.commit(&[("keep", File("k"))], "recorded");
    let plan = fx.plan(&fx.restore(".", c1)).unwrap();
    let plan = check(
        plan,
        &fx.repo,
        &Policy::default(),
        &NoLfs,
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
    let refuse = Policy {
        refuse: vec!["b"],
        ..Policy::default()
    };
    let plan = check(
        fx.plan(&fx.restore(".", c1)).unwrap(),
        &fx.repo,
        &refuse,
        &NoLfs,
        &fx.dir(),
        &NotLive,
        false,
    )
    .unwrap();
    assert!(plan.steps.is_empty());
}

/// A write cut short part way (EIO) is reported for recording as it is;
/// a path left as history has it is not.
#[test]
fn a_write_cut_short_is_left_changed() {
    let fx = Fixture::new();
    let c1 = fx.commit(&[("f", File("old"))], "v1");
    fx.commit(&[("f", File("new"))], "v2");
    fx.checkout(&[("f", File("new"))]);
    let plan = fx.plan(&fx.restore("f", c1)).unwrap();
    let failed = |started| ApplyError {
        applied: Applied::default(),
        path: "f".into(),
        error: io::Error::other("EIO"),
        started,
    };
    assert_eq!(failed(true).left_changed(&plan, &fx.repo, &fx.dir()), None);
    fs::write(fx.tree.join("f"), "ol").unwrap();
    assert_eq!(
        failed(true).left_changed(&plan, &fx.repo, &fx.dir()),
        Some(RepoPath::new("f").unwrap())
    );
    assert_eq!(failed(false).left_changed(&plan, &fx.repo, &fx.dir()), None);
}

mod lfs {
    use std::cell::Cell;
    use std::collections::{BTreeSet, HashMap};

    use super::*;
    use crate::checks::{BlobLimit, Checks, Limits};
    use crate::lfs::ObjectCache;
    use crate::lfs::fetch::CacheOnly;

    fn leak(s: String) -> &'static str {
        Box::leak(s.into_boxed_str())
    }

    struct Lfs {
        fx: Fixture,
        _dir: tempfile::TempDir,
        cache: CacheOnly,
    }

    impl Lfs {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let cache = CacheOnly(ObjectCache::open(dir.path().join("lfs")).unwrap());
            Self {
                fx: Fixture::new(),
                _dir: dir,
                cache,
            }
        }

        fn cache_object(&self, content: &str) -> Pointer {
            let src = self._dir.path().join("src");
            fs::write(&src, content).unwrap();
            self.cache.0.insert_file(&src).unwrap()
        }

        /// Commits `files` plus `lfs` (path, content) stored as LFS, with
        /// the managed block after `user` in the root .gitattributes; the
        /// objects are cached.
        fn commit(&self, files: &[(&str, F)], lfs: &[(&str, &str)], user: &str) -> Oid {
            let mut all: Vec<(&str, F)> = files.to_vec();
            let mut managed = BTreeSet::new();
            for (path, content) in lfs {
                let pointer = self.cache_object(content);
                all.push((path, File(leak(pointer.encode()))));
                managed.insert(RepoPath::new(*path).unwrap());
            }
            let attrs = attributes::compose(user.as_bytes(), &managed);
            if !attrs.is_empty() {
                all.push((
                    ".gitattributes",
                    File(leak(String::from_utf8(attrs).unwrap())),
                ));
            }
            self.fx.commit(&all, "lfs")
        }

        fn check_with(
            &self,
            request: &Request,
            policy: &Policy,
            objects: &dyn LfsObjects,
        ) -> Result<Plan, RestoreError> {
            check(
                self.fx.plan(request)?,
                &self.fx.repo,
                policy,
                objects,
                &self.fx.dir(),
                &NotLive,
                false,
            )
        }

        fn run_with(
            &self,
            request: &Request,
            objects: &dyn LfsObjects,
        ) -> Result<Applied, RestoreError> {
            let plan = self.check_with(request, &Policy::default(), objects)?;
            apply(
                &plan,
                &self.fx.repo,
                objects,
                &self.fx.dir(),
                &NotLive,
                false,
            )
            .map_err(|e| RestoreError::Io(e.error))
        }
    }

    #[test]
    fn lfs_paths_are_restored_and_reverted_from_the_cache() {
        let l = Lfs::new();
        let c1 = l.commit(&[("a", File("a"))], &[("big.bin", "big one")], "");
        let c2 = l.commit(&[("a", File("a"))], &[("big.bin", "big two, longer")], "");
        l.fx.checkout(&[("a", File("a")), ("big.bin", File("big two, longer"))]);

        // Entries are the content, not the pointers.
        let plan = l.fx.plan(&l.fx.restore("big.bin", c1)).unwrap();
        assert!(matches!(
            plan.steps[0].target.unwrap().stored,
            Stored::Lfs(p) if p.size == 7
        ));
        l.run_with(&l.fx.restore("big.bin", c1), &l.cache).unwrap();
        assert_eq!(
            fs::read_to_string(l.fx.tree.join("big.bin")).unwrap(),
            "big one"
        );

        // Recorded (as the CLI would), then reverted back.
        l.commit(&[("a", File("a"))], &[("big.bin", "big one")], "");
        l.run_with(
            &l.fx.revert(l.fx.repo.refname_to_id(BRANCH).unwrap()),
            &l.cache,
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(l.fx.tree.join("big.bin")).unwrap(),
            "big two, longer"
        );
        let _ = c2;
    }

    #[test]
    fn matching_lfs_content_is_already_done_and_changed_content_is_refused() {
        let l = Lfs::new();
        let c1 = l.commit(&[], &[("big.bin", "content")], "");
        l.fx.checkout(&[("big.bin", File("content"))]);
        let plan = l
            .check_with(&l.fx.restore(".", c1), &Policy::default(), &NoLfs)
            .unwrap();
        assert!(plan.steps.is_empty(), "{:?}", plan.steps);
        // Same size, other bytes: not what history holds.
        l.fx.checkout(&[("big.bin", File("CONTENT"))]);
        let err = l
            .check_with(&l.fx.restore(".", c1), &Policy::default(), &l.cache)
            .unwrap_err();
        assert!(matches!(err, RestoreError::Unrecorded { .. }), "{err}");
    }

    #[test]
    fn an_unavailable_object_refuses_before_any_write() {
        let l = Lfs::new();
        let c1 = l.commit(&[("a", File("one"))], &[("big.bin", "big one")], "");
        l.commit(&[("a", File("two"))], &[("big.bin", "big two")], "");
        l.fx.checkout(&[("a", File("two")), ("big.bin", File("big two"))]);
        let err = l.run_with(&l.fx.restore(".", c1), &NoLfs).unwrap_err();
        assert!(
            matches!(&err, RestoreError::LfsObject { path, .. } if path == "big.bin"),
            "{err}"
        );
        assert_eq!(err.kind(), "lfs_object");
        assert!(err.hint().is_some());
        assert_eq!(
            l.fx.read_tree(),
            tree_of(&[("a", File("two")), ("big.bin", File("big two"))]),
            "nothing written"
        );
    }

    /// Fills the cache on demand, as a download would.
    struct Downloads<'a> {
        cache: &'a ObjectCache,
        remote: HashMap<String, &'static str>,
        calls: Cell<usize>,
    }

    impl LfsObjects for Downloads<'_> {
        fn ensure(&self, pointer: &Pointer) -> Result<PathBuf, crate::lfs::LfsError> {
            if !self.cache.contains(pointer) {
                self.calls.set(self.calls.get() + 1);
                let content = self.remote[&pointer.oid.to_string()];
                self.cache
                    .insert_verified(pointer, &mut content.as_bytes())?;
            }
            Ok(self.cache.path(&pointer.oid))
        }
    }

    #[test]
    fn a_cache_miss_is_downloaded_once() {
        let l = Lfs::new();
        let c1 = l.commit(&[], &[("big.bin", "from the remote")], "");
        l.commit(&[], &[], "");
        l.fx.checkout(&[]);
        // The object is not in this cache, only "on the remote".
        let other = tempfile::tempdir().unwrap();
        let cache = ObjectCache::open(other.path()).unwrap();
        let pointer = Pointer::of_reader(&b"from the remote"[..]).unwrap();
        let objects = Downloads {
            cache: &cache,
            remote: [(pointer.oid.to_string(), "from the remote")].into(),
            calls: Cell::new(0),
        };
        l.run_with(&l.fx.restore("big.bin", c1), &objects).unwrap();
        assert_eq!(
            fs::read_to_string(l.fx.tree.join("big.bin")).unwrap(),
            "from the remote"
        );
        assert_eq!(
            objects.calls.get(),
            1,
            "fetched in the check, reused to write"
        );
    }

    #[test]
    fn gitattributes_is_restored_without_the_managed_block() {
        let l = Lfs::new();
        let c1 = l.commit(&[], &[("big.bin", "x1")], "*.txt text\n");
        l.commit(&[], &[("big.bin", "x2"), ("more.bin", "y")], "*.md text\n");
        l.fx.checkout(&[
            (".gitattributes", File("*.md text\n")),
            ("big.bin", File("x2")),
            ("more.bin", File("y")),
        ]);
        // The tree's .gitattributes (no block) matches history's.
        let plan = l
            .check_with(
                &l.fx.restore(".gitattributes", c1),
                &Policy::default(),
                &l.cache,
            )
            .unwrap();
        assert_eq!(paths(&plan.steps), [(".gitattributes", "write")]);
        l.run_with(&l.fx.restore(".gitattributes", c1), &l.cache)
            .unwrap();
        assert_eq!(
            fs::read_to_string(l.fx.tree.join(".gitattributes")).unwrap(),
            "*.txt text\n"
        );
        // A .gitattributes holding only the block is agentfs's own: absent
        // to the tree.
        let l = Lfs::new();
        let c1 = l.commit(&[], &[("big.bin", "x1")], "");
        l.fx.checkout(&[("big.bin", File("x1"))]);
        let plan = l.fx.plan(&l.fx.restore(".", c1)).unwrap();
        assert_eq!(paths(&plan.steps), [("big.bin", "write")]);
        let checked = l
            .check_with(&l.fx.restore(".", c1), &Policy::default(), &l.cache)
            .unwrap();
        assert!(checked.steps.is_empty());
        // Only the block changed between two commits: nothing to revert
        // there.
        let l = Lfs::new();
        l.commit(&[("a", File("a"))], &[("big.bin", "x1")], "*.txt text\n");
        let c2 = l.commit(
            &[("a", File("a"))],
            &[("big.bin", "x1"), ("new.bin", "n")],
            "*.txt text\n",
        );
        let plan = l.fx.plan(&l.fx.revert(c2)).unwrap();
        assert_eq!(paths(&plan.steps), [("new.bin", "remove")]);
    }

    #[test]
    fn the_store_the_restore_would_record_decides_the_blob_limit() {
        let l = Lfs::new();
        let c1 = l.commit(&[], &[("big.bin", "seven b")], "");
        l.commit(&[], &[], "");
        l.fx.checkout(&[]);
        let limited = |lfs_over| Policy {
            lfs_over,
            checks: Checks::new(Limits {
                max_blob_size: Some(BlobLimit {
                    bytes: 3,
                    remote: "r".into(),
                }),
            }),
            ..Policy::default()
        };
        let err = l
            .check_with(&l.fx.restore(".", c1), &limited(None), &l.cache)
            .unwrap_err();
        assert!(
            matches!(&err, RestoreError::Unrecordable { reason, .. } if reason.contains("limit")),
            "{err}"
        );
        l.check_with(&l.fx.restore(".", c1), &limited(Some(3)), &l.cache)
            .unwrap();
    }
}

const DB: &[&str] = &["d/s.db", "d/s.db-journal", "d/s.db-wal"];

/// Plan and check with the database group `DB` (design §8).
fn check_db(fx: &Fixture, request: &Request, force: bool) -> Result<Plan, RestoreError> {
    let policy = Policy {
        groups: vec![DB],
        ..Policy::default()
    };
    check(
        fx.plan(request)?,
        &fx.repo,
        &policy,
        &NoLfs,
        &fx.dir(),
        &NotLive,
        force,
    )
}

#[test]
fn restoring_one_member_restores_its_whole_group() {
    let fx = Fixture::new();
    let c1 = fx.commit(
        &[
            ("d/s.db", File("v1")),
            ("d/s.db-wal", File("w1")),
            ("d/x", File("x")),
        ],
        "v1",
    );
    let head = [
        ("d/s.db", File("v2")),
        ("d/s.db-journal", File("j2")),
        ("d/x", File("y")),
    ];
    fx.commit(&head, "v2");
    fx.checkout(&head);
    // Asked for the database alone: its -wal comes back and its -journal
    // goes, from the same commit; the unrelated file is left alone.
    let plan = check_db(&fx, &fx.restore("d/s.db", c1), false).unwrap();
    assert_eq!(
        paths(&plan.steps),
        [
            ("d/s.db", "write"),
            ("d/s.db-journal", "remove"),
            ("d/s.db-wal", "write")
        ]
    );
    apply(&plan, &fx.repo, &NoLfs, &fx.dir(), &NotLive, false).unwrap();
    assert_eq!(
        fx.read_tree(),
        tree_of(&[
            ("d/s.db", File("v1")),
            ("d/s.db-wal", File("w1")),
            ("d/x", File("y"))
        ])
    );
}

#[test]
fn an_untracked_member_in_the_tree_refuses_a_group_restore() {
    let fx = Fixture::new();
    let c1 = fx.commit(&[("d/s.db", File("v1"))], "v1");
    fx.commit(&[("d/s.db", File("v2"))], "v2");
    // A hot journal history never saw: restoring the database under it
    // would hand SQLite another version's journal.
    fx.checkout(&[("d/s.db", File("v2")), ("d/s.db-journal", File("hot"))]);
    let err = check_db(&fx, &fx.restore("d/s.db", c1), false).unwrap_err();
    assert!(
        matches!(err, RestoreError::InTheWay { ref path, .. } if path == "d/s.db-journal"),
        "{err}"
    );
}

#[test]
fn reverting_one_member_reverts_its_whole_group() {
    let fx = Fixture::new();
    fx.commit(&[("d/s.db", File("v1")), ("d/s.db-wal", File("w1"))], "c1");
    // c2 changes the database only; c3 the WAL only.
    let c2 = fx.commit(&[("d/s.db", File("v2")), ("d/s.db-wal", File("w1"))], "c2");
    let head = [("d/s.db", File("v2")), ("d/s.db-wal", File("w3"))];
    fx.commit(&head, "c3");
    fx.checkout(&head);
    // Undoing c2 alone would pair v1 with w3: the WAL is part of the
    // revert, and it changed since, so it needs --force.
    let err = check_db(&fx, &fx.revert(c2), false).unwrap_err();
    assert!(
        matches!(err, RestoreError::ChangedSince { ref paths } if paths == &["d/s.db-wal"]),
        "{err}"
    );
    let plan = check_db(&fx, &fx.revert(c2), true).unwrap();
    assert_eq!(
        paths(&plan.steps),
        [("d/s.db", "write"), ("d/s.db-wal", "write")]
    );
    apply(&plan, &fx.repo, &NoLfs, &fx.dir(), &NotLive, true).unwrap();
    assert_eq!(
        fx.read_tree(),
        tree_of(&[("d/s.db", File("v1")), ("d/s.db-wal", File("w1"))])
    );
}
