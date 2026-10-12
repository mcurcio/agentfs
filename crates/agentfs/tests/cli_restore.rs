//! `agentfs restore` and `agentfs revert` with no mount running: direct
//! mode, writing the backing tree (design §15, §18 test 11). The same
//! commands through a running mount are tested in `mount.rs`.

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

struct Volume {
    dir: tempfile::TempDir,
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

impl Volume {
    fn new() -> Self {
        let v = Self {
            dir: tempfile::tempdir().unwrap(),
        };
        fs::create_dir_all(v.tree()).unwrap();
        fs::create_dir_all(v.dir.path().join("home")).unwrap();
        let out = v.raw(&["init", "--volume", "demo"]);
        assert!(out.status.success(), "{}", stderr(&out));
        v.config("agentfs.commitWindow", "10ms");
        v
    }

    fn state(&self) -> PathBuf {
        self.dir.path().join("state")
    }

    fn tree(&self) -> PathBuf {
        self.dir.path().join("tree")
    }

    fn t(&self, rel: &str) -> PathBuf {
        self.tree().join(rel)
    }

    fn config(&self, key: &str, value: &str) {
        git2::Config::open(&self.state().join("git/config"))
            .unwrap()
            .set_str(key, value)
            .unwrap();
    }

    fn command(&self) -> Command {
        let home = self.dir.path().join("home");
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_agentfs"));
        for (name, _) in std::env::vars_os() {
            let n = name.to_string_lossy();
            if n.starts_with("OTEL_") || n.starts_with("AGENTFS_") {
                cmd.env_remove(name);
            }
        }
        cmd.arg("--state")
            .arg(self.state())
            .env("AGENTFS_REPLICA", "test-0")
            .env("HOME", &home)
            .env("GIT_CONFIG_NOSYSTEM", "1");
        cmd
    }

    /// Runs with `--state` only.
    fn raw(&self, args: &[&str]) -> Output {
        self.command().args(args).output().unwrap()
    }

    /// Runs with `--state` and `--root`.
    fn run(&self, args: &[&str]) -> Output {
        self.command()
            .arg("--root")
            .arg(self.tree())
            .args(args)
            .output()
            .unwrap()
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "{args:?}: {}{}",
            stdout(&out),
            stderr(&out)
        );
        stdout(&out)
    }

    /// Runs and expects exit status 1 with `needle` on stderr.
    fn refused(&self, args: &[&str], needle: &str) -> String {
        let out = self.run(args);
        let err = stderr(&out);
        assert_eq!(
            out.status.code(),
            Some(1),
            "{args:?}: {}{err}",
            stdout(&out)
        );
        assert!(err.contains(needle), "{args:?}: {needle:?} not in {err}");
        err
    }

    fn write(&self, rel: &str, content: &str) {
        let p = self.t(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, content).unwrap();
    }

    fn record(&self, paths: &[&str]) {
        let mut args = vec!["record"];
        args.extend_from_slice(paths);
        self.ok(&args);
    }

    fn repo(&self) -> git2::Repository {
        git2::Repository::open_bare(self.state().join("git")).unwrap()
    }

    fn head(&self) -> git2::Oid {
        self.repo()
            .refname_to_id("refs/heads/agentfs/demo")
            .unwrap()
    }

    fn message(&self, id: git2::Oid) -> String {
        self.repo()
            .find_commit(id)
            .unwrap()
            .message()
            .unwrap()
            .to_owned()
    }

    fn read(&self, rel: &str) -> String {
        fs::read_to_string(self.t(rel)).unwrap()
    }

    /// The branch head's tree: path to content (symlinks as `-> target`,
    /// executables with `*`).
    fn history(&self) -> Vec<(String, String)> {
        let repo = self.repo();
        let commit = repo.find_commit(self.head()).unwrap();
        let mut out = Vec::new();
        commit
            .tree()
            .unwrap()
            .walk(git2::TreeWalkMode::PreOrder, |root, e| {
                if e.kind() == Some(git2::ObjectType::Blob) {
                    let blob = repo.find_blob(e.id()).unwrap();
                    let content = String::from_utf8_lossy(blob.content()).into_owned();
                    let shown = match e.filemode() {
                        0o120000 => format!("-> {content}"),
                        0o100755 => format!("{content}*"),
                        _ => content,
                    };
                    out.push((format!("{root}{}", e.name().unwrap()), shown));
                }
                git2::TreeWalkResult::Ok
            })
            .unwrap();
        out
    }
}

fn h(items: &[(&str, &str)]) -> Vec<(String, String)> {
    items
        .iter()
        .map(|(p, c)| ((*p).to_owned(), (*c).to_owned()))
        .collect()
}

fn uid() -> u32 {
    fs::metadata("/proc/self").unwrap().uid()
}

#[test]
fn restore_files_directories_symlinks_and_modes() {
    let v = Volume::new();
    v.write("a.txt", "one");
    v.write("d/x", "x1");
    v.write("d/sub/y", "y1");
    fs::set_permissions(v.t("d/sub/y"), fs::Permissions::from_mode(0o755)).unwrap();
    symlink("a.txt", v.t("link")).unwrap();
    v.record(&["a.txt", "d/x", "d/sub/y", "link"]);
    let c1 = v.head().to_string();

    v.write("a.txt", "two");
    v.write("d/x", "x2");
    v.write("d/new", "n");
    fs::remove_file(v.t("d/sub/y")).unwrap();
    fs::remove_file(v.t("link")).unwrap();
    symlink("d/x", v.t("link")).unwrap();
    v.record(&["a.txt", "d/x", "d/new", "d/sub/y", "link"]);
    let c2 = v.head();

    // A file. The plan is listed; one commit with the trailers, authored
    // by the user who ran it.
    let out = v.ok(&["restore", "a.txt", &c1]);
    assert!(out.contains("  write a.txt\n"), "{out}");
    assert!(
        out.contains("restored 1 path(s), removed 0: commit "),
        "{out}"
    );
    assert_eq!(v.read("a.txt"), "one");
    let head = v.head();
    let repo = v.repo();
    let commit = repo.find_commit(head).unwrap();
    assert_eq!(commit.parent_id(0).unwrap(), c2, "one commit");
    assert_eq!(commit.author().name().unwrap(), format!("uid-{}", uid()));
    let message = v.message(head);
    assert!(message.starts_with("restore a.txt from "), "{message}");
    assert!(message.contains("Agentfs-Trigger: restore\n"), "{message}");
    assert!(
        message.contains(&format!("Agentfs-Restore: {c1}\n")),
        "{message}"
    );
    assert!(
        message.contains(&format!("Agentfs-Uid: {}\n", uid())),
        "{message}"
    );
    assert!(out.contains(&head.to_string()), "{out}");

    // A directory: rewritten, recreated (with its exec bit) and removed.
    let out = v.ok(&["restore", "d", &c1]);
    assert!(out.contains("remove d/new"), "{out}");
    assert_eq!(v.read("d/x"), "x1");
    assert!(!v.t("d/new").exists());
    assert_eq!(
        fs::metadata(v.t("d/sub/y")).unwrap().mode() & 0o111,
        0o111,
        "exec bit restored"
    );
    // A symlink.
    v.ok(&["restore", "link", &c1]);
    assert_eq!(fs::read_link(v.t("link")).unwrap(), Path::new("a.txt"));
    assert_eq!(
        v.history(),
        h(&[
            ("a.txt", "one"),
            ("d/sub/y", "y1*"),
            ("d/x", "x1"),
            ("link", "-> a.txt")
        ])
    );
    // The whole tree, back to v2 (by revision syntax), then a re-run.
    v.ok(&["restore", ".", "HEAD~3"]);
    assert_eq!(v.read("a.txt"), "two");
    assert!(!v.t("d/sub").exists(), "emptied directory pruned");
    let out = v.ok(&["restore", ".", &c2.to_string()]);
    assert!(out.contains("already matches"), "{out}");

    // An exec bit alone.
    fs::set_permissions(v.t("a.txt"), fs::Permissions::from_mode(0o755)).unwrap();
    v.record(&["a.txt"]);
    let ino = fs::metadata(v.t("a.txt")).unwrap().ino();
    let out = v.ok(&["restore", "a.txt", "HEAD~1"]);
    assert!(out.contains("  mode a.txt"), "{out}");
    assert_eq!(fs::metadata(v.t("a.txt")).unwrap().mode() & 0o777, 0o644);
    assert_eq!(fs::metadata(v.t("a.txt")).unwrap().ino(), ino);
}

#[test]
fn dry_run_writes_nothing() {
    let v = Volume::new();
    v.write("a", "1");
    v.record(&["a"]);
    let c1 = v.head().to_string();
    v.write("a", "2");
    v.record(&["a"]);
    let head = v.head();
    let out = v.ok(&["restore", "--dry-run", "a", &c1]);
    assert!(out.contains("  write a\n"), "{out}");
    assert!(
        out.contains("dry run: 1 change(s) planned, nothing written"),
        "{out}"
    );
    assert_eq!(v.read("a"), "2");
    assert_eq!(v.head(), head);
    let out = v.ok(&["revert", "--dry-run", &head.to_string()]);
    assert!(out.contains("dry run"), "{out}");
    assert_eq!(v.head(), head);
}

#[test]
fn revert_adds_modifies_and_deletes() {
    let v = Volume::new();
    v.write("keep", "k");
    v.write("mod", "m1");
    v.write("gone", "g");
    v.record(&["keep", "mod", "gone"]);
    v.write("mod", "m2");
    v.write("added", "a");
    fs::remove_file(v.t("gone")).unwrap();
    v.record(&["mod", "added", "gone"]);
    let c = v.head();
    let out = v.ok(&["revert", &c.to_string()]);
    assert!(out.contains("restored 2 path(s), removed 1"), "{out}");
    assert_eq!(
        v.history(),
        h(&[("gone", "g"), ("keep", "k"), ("mod", "m1")])
    );
    assert_eq!(v.read("gone"), "g");
    assert!(!v.t("added").exists());
    let message = v.message(v.head());
    assert!(
        message.starts_with(&format!("revert {}", &c.to_string()[..7])),
        "{message}"
    );
    assert!(
        message.contains(&format!("Agentfs-Revert: {c}\n")),
        "{message}"
    );
    assert!(message.contains("Agentfs-Trigger: restore\n"), "{message}");

    // Undoing the revert: revert the revert.
    let r = v.head().to_string();
    v.ok(&["revert", &r]);
    assert_eq!(
        v.history(),
        h(&[("added", "a"), ("keep", "k"), ("mod", "m2")])
    );
}

#[test]
fn revert_of_later_changed_paths_needs_force() {
    let v = Volume::new();
    v.write("f", "1");
    v.record(&["f"]);
    v.write("f", "2");
    v.record(&["f"]);
    let c = v.head().to_string();
    v.write("f", "3");
    v.record(&["f"]);
    let err = v.refused(
        &["revert", &c],
        "changed again since the reverted commit: f",
    );
    assert!(err.contains("--force reverts them anyway"), "{err}");
    assert_eq!(v.read("f"), "3");
    v.ok(&["revert", "--force", &c]);
    assert_eq!(v.read("f"), "1");
}

#[test]
fn refusals_say_why_and_change_nothing() {
    let v = Volume::new();
    fs::write(v.state().join("git/info/attributes"), "secret -agentfs\n").unwrap();
    fs::write(v.state().join("git/info/exclude"), "*.log\n").unwrap();
    v.write("a", "1");
    v.write("d/x", "x");
    v.record(&["a", "d/x"]);
    let c1 = v.head().to_string();
    v.write("a", "2");
    fs::remove_dir_all(v.t("d")).unwrap();
    v.record(&["a", "d/x"]);
    let head = v.head();

    v.refused(&["restore", "a", "nonsense"], "is not a commit");
    v.refused(&["restore", "nothing", &c1], "nothing at nothing");
    v.refused(&["restore", "../x", &c1], "is not a path agentfs records");
    // A commit that is not on the volume's branch.
    let repo = v.repo();
    let sig = git2::Signature::now("o", "o@example.com").unwrap();
    let tree = repo.find_commit(head).unwrap().tree().unwrap();
    let stray = repo.commit(None, &sig, &sig, "stray", &tree, &[]).unwrap();
    v.refused(
        &["restore", "a", &stray.to_string()],
        "is not on the volume's branch",
    );
    // A change that is not recorded yet: never overwritten, even forced.
    v.write("a", "unrecorded");
    for force in [&[][..], &["--force"][..]] {
        let mut args = vec!["restore"];
        args.extend_from_slice(force);
        args.extend(["a", &c1]);
        let err = v.refused(&args, "a differs from history");
        assert!(err.contains("--force does not override"), "{err}");
    }
    assert_eq!(v.read("a"), "unrecorded");
    v.write("a", "2");
    // An untracked file where a restored file goes.
    v.write("d", "untracked");
    v.refused(&["restore", "d", &c1], "d is in the way: a file");
    fs::remove_file(v.t("d")).unwrap();
    // A symlinked ancestor is never followed.
    let outside = tempfile::tempdir().unwrap();
    symlink(outside.path(), v.t("d")).unwrap();
    v.refused(&["restore", "d", &c1], "d is in the way: a symlink");
    assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
    fs::remove_file(v.t("d")).unwrap();
    assert_eq!(v.head(), head, "nothing was committed");

    // Paths the volume would not record: -agentfs, and ignored untracked.
    fs::write(v.state().join("git/info/attributes"), "").unwrap();
    fs::write(v.state().join("git/info/exclude"), "").unwrap();
    v.write("secret", "s");
    v.write("x.log", "l");
    v.record(&["secret", "x.log"]);
    let c3 = v.head().to_string();
    fs::remove_file(v.t("secret")).unwrap();
    fs::remove_file(v.t("x.log")).unwrap();
    v.record(&["secret", "x.log"]);
    fs::write(v.state().join("git/info/attributes"), "secret -agentfs\n").unwrap();
    fs::write(v.state().join("git/info/exclude"), "*.log\n").unwrap();
    v.refused(&["restore", "secret", &c3], "secret would not be recorded");
    v.refused(&["restore", "x.log", &c3], "x.log would not be recorded");
    assert!(!v.t("secret").exists() && !v.t("x.log").exists());
}

#[test]
fn needs_root_without_a_mount() {
    let v = Volume::new();
    v.write("a", "1");
    v.record(&["a"]);
    let out = v.raw(&["restore", "a", "HEAD"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("needs --root"), "{}", stderr(&out));
}

fn spawn_run(v: &Volume) -> Child {
    let mut child = v
        .command()
        .arg("--root")
        .arg(v.tree())
        .arg("run")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while !stdout(&v.raw(&["status"])).contains("command=run") {
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("run never took the lock");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    child
}

#[test]
fn a_busy_state_area_exits_75() {
    let v = Volume::new();
    v.write("a", "1");
    v.record(&["a"]);
    let mut run = spawn_run(&v);
    let out = v.run(&["restore", "a", "HEAD"]);
    let _ = run.kill();
    let _ = run.wait();
    assert_eq!(out.status.code(), Some(75), "{}", stderr(&out));
    let err = stderr(&out);
    assert!(err.contains("is in use"), "{err}");
    assert!(err.contains("`agentfs run` holds it"), "{err}");
}

impl Volume {
    /// Runs `agentfs --root <tree> args` under `sh -c "<prefix>; exec ..."`
    /// (a `ulimit`, a `trap`), with the same environment as [`Self::run`].
    fn run_limited(&self, prefix: &str, args: &[&str]) -> Output {
        let base = self.command();
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg(format!("{prefix}; exec \"$@\""))
            .arg("sh")
            .arg(base.get_program())
            .args(base.get_args())
            .arg("--root")
            .arg(self.tree())
            .args(args);
        for (name, value) in base.get_envs() {
            match value {
                Some(value) => cmd.env(name, value),
                None => cmd.env_remove(name),
            };
        }
        cmd.output().unwrap()
    }
}

/// The tree already holds what the restore writes, but history does not
/// (content put back by hand without `agentfs record`): the restore records
/// it instead of reporting that everything matches.
#[test]
fn a_tree_ahead_of_history_is_recorded() {
    let v = Volume::new();
    v.write("p", "v1");
    v.write("keep", "k");
    v.record(&["p", "keep"]);
    let c1 = v.head().to_string();
    v.write("p", "v2");
    v.write("q", "new");
    v.record(&["p", "q"]);
    // `git show C1:p > tree/p`, and q deleted, neither recorded.
    v.write("p", "v1");
    fs::remove_file(v.t("q")).unwrap();
    let out = v.ok(&["restore", ".", &c1]);
    assert!(
        out.contains("  write p\n") && out.contains("  remove q\n"),
        "{out}"
    );
    assert!(
        out.contains("restored 1 path(s), removed 1: commit "),
        "{out}"
    );
    assert_eq!(v.history(), h(&[("keep", "k"), ("p", "v1")]));
    let message = v.message(v.head());
    assert!(message.contains("Agentfs-Trigger: restore\n"), "{message}");
    // Now both agree: a re-run has nothing to do.
    let out = v.ok(&["restore", ".", &c1]);
    assert!(out.contains("already matches"), "{out}");
}

/// A restore killed after writing some paths and before recording them
/// (here by SIGXFSZ) is finished by running it again: what the tree
/// already holds is recorded, the rest written.
#[test]
fn a_restore_killed_part_way_is_finished_by_a_rerun() {
    let v = Volume::new();
    let big = "b".repeat(20 << 10);
    v.write("d/a", "a1");
    v.write("d/b", &big);
    v.record(&["d/a", "d/b"]);
    let c1 = v.head().to_string();
    v.write("d/a", "a2");
    v.write("d/b", "b2");
    v.record(&["d/a", "d/b"]);
    let head = v.head();
    // Writing d/a fits in 4 KiB; d/b does not, and SIGXFSZ kills it.
    let out = v.run_limited("ulimit -f 8", &["restore", "d", &c1]);
    assert_eq!(
        out.status.code(),
        None,
        "killed: {}{}",
        stdout(&out),
        stderr(&out)
    );
    assert_eq!(v.read("d/a"), "a1", "written before the kill");
    assert_eq!(v.read("d/b"), "b2", "the failed write left it whole");
    assert_eq!(v.head(), head, "nothing recorded");
    let out = v.ok(&["restore", "d", &c1]);
    assert!(
        out.contains("  write d/a\n") && out.contains("  write d/b\n"),
        "{out}"
    );
    assert!(
        out.contains("restored 2 path(s), removed 0: commit "),
        "{out}"
    );
    assert_eq!(v.history(), h(&[("d/a", "a1"), ("d/b", &big)]));
    assert_eq!(v.read("d/b"), big);
}

/// A write that fails (EFBIG here, as ENOSPC would) leaves the tree and
/// history agreeing, so running the command again finishes the job.
#[test]
fn a_failed_write_can_be_finished_by_a_rerun() {
    let v = Volume::new();
    let big = "x".repeat(600 << 10);
    v.write("big", &big);
    v.record(&["big"]);
    let c1 = v.head().to_string();
    v.write("big", "small");
    v.record(&["big"]);
    let out = v.run_limited("trap '' XFSZ; ulimit -f 200", &["restore", "big", &c1]);
    let (so, se) = (stdout(&out), stderr(&out));
    assert_eq!(out.status.code(), Some(1), "{so}{se}");
    assert!(se.contains("failed part way"), "{se}");
    assert!(!so.contains("already matched"), "{so}");
    // Whatever the tree holds now, history holds too (the space is
    // allocated first, so normally the file is untouched).
    let now = v.read("big");
    assert_eq!(v.history(), vec![("big".to_owned(), now)]);
    let out = v.ok(&["restore", "big", &c1]);
    assert!(out.contains(": commit "), "{out}");
    assert_eq!(v.read("big"), big);
    assert_eq!(v.history(), vec![("big".to_owned(), big)]);
}

#[test]
fn restoring_a_database_member_restores_its_group_in_one_commit() {
    let v = Volume::new();
    fs::write(
        v.state().join("git/info/attributes"),
        "*.db* agentfs.commit=fsync agentfs.group=sqlite\n",
    )
    .unwrap();
    v.write("d/s.db", "v1");
    v.write("d/s.db-wal", "w1");
    v.record(&["d/s.db"]);
    let c1 = v.head().to_string();
    v.write("d/s.db", "v2");
    fs::remove_file(v.t("d/s.db-wal")).unwrap();
    v.write("d/s.db-journal", "j2");
    v.record(&["d/s.db"]);
    let c2 = v.head();
    assert_eq!(
        v.history(),
        h(&[("d/s.db", "v2"), ("d/s.db-journal", "j2")])
    );

    // Only the database is named: the whole group comes back from c1.
    let out = v.ok(&["restore", "d/s.db", &c1]);
    assert!(out.contains("restored 2 path(s), removed 1"), "{out}");
    assert_eq!(v.history(), h(&[("d/s.db", "v1"), ("d/s.db-wal", "w1")]));
    assert_eq!(v.read("d/s.db-wal"), "w1");
    assert!(!v.t("d/s.db-journal").exists());
    // One commit, so no version in history pairs v1 with j2.
    let repo = v.repo();
    let head = repo.find_commit(v.head()).unwrap();
    assert_eq!(head.parent_id(0).unwrap(), c2);
    assert!(
        v.message(v.head())
            .contains(&format!("Agentfs-Restore: {c1}\n")),
        "{}",
        v.message(v.head())
    );
}
