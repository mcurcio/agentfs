//! Real FUSE mounts (design §18 tests 1, 2, 4, 5, 6, 7, 11 and 15, plus SQLite
//! in and out of the fsync class (design §8),
//! git on the mount, restores through the mount, the daemon's lifecycle, and
//! `probe`, `--on-stale-mount hold` and `recover` after a crash (design §17.1), and two
//! front mounts of one backing tree feeding one history service (design §14).
//!
//! Every test needs `/dev/fuse` and root, so all are `#[ignore]`d; the
//! `fuse` CI job runs them through `ci/fuse-tests.sh`:
//!
//! ```sh
//! sudo AGENTFS_TEST_PASSTHROUGH=auto target/debug/deps/mount-<hash> --ignored --test-threads=1
//! ```
//!
//! - `AGENTFS_TEST_PASSTHROUGH` (`auto`, `on`, `off`) is passed to every
//!   mount.
//! - `AGENTFS_TEST_UNPRIV_USER` names a user to run an unprivileged daemon
//!   as (needs `user_allow_other` in `/etc/fuse.conf`); without it that test
//!   is skipped.
//! - Daemon logs go to `$RUNNER_TEMP/agentfs-mount-logs` when set.
//!
//! Each test mounts its own volume; history is read with git2 straight from
//! the state area while the daemon holds it (reading needs no lock).

use std::collections::BTreeMap;
use std::fs;
use std::io::Write as _;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::time::{Duration, Instant};

const UID: u32 = 10000;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Item {
    File(String),
    Exec(String),
    Link(String),
}

use Item::{Exec, File, Link};

fn f(s: &str) -> Item {
    File(s.into())
}

type Tree = BTreeMap<String, Item>;

fn tree(items: &[(&str, Item)]) -> Tree {
    items
        .iter()
        .map(|(p, i)| ((*p).to_owned(), i.clone()))
        .collect()
}

fn require_root() {
    assert!(
        rustix::process::geteuid().is_root(),
        "the mount tests run as root (ci/fuse-tests.sh)"
    );
}

fn passthrough() -> String {
    std::env::var("AGENTFS_TEST_PASSTHROUGH").unwrap_or_else(|_| "auto".into())
}

fn log_dir() -> PathBuf {
    let dir = match std::env::var_os("RUNNER_TEMP") {
        Some(t) => PathBuf::from(t).join("agentfs-mount-logs"),
        None => std::env::temp_dir().join("agentfs-mount-logs"),
    };
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// A volume: `backing/`, `state/` and `mnt/` side by side.
struct Vol {
    dir: tempfile::TempDir,
    name: &'static str,
}

impl Vol {
    fn new(name: &'static str) -> Self {
        require_root();
        let dir = tempfile::tempdir().unwrap();
        // Other users (uid 10000) must reach the mount point.
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).unwrap();
        let v = Self { dir, name };
        for d in ["backing", "mnt"] {
            fs::create_dir_all(v.p(d)).unwrap();
            fs::set_permissions(v.p(d), fs::Permissions::from_mode(0o755)).unwrap();
        }
        let out = v.agentfs(&["init", "--volume", "demo"]);
        assert!(out.status.success(), "{}", text(&out.stderr));
        v.config("agentfs.commitWindow", "50ms");
        v
    }

    fn p(&self, rel: &str) -> PathBuf {
        self.dir.path().join(rel)
    }

    fn backing(&self) -> PathBuf {
        self.p("backing")
    }

    fn mnt(&self) -> PathBuf {
        self.p("mnt")
    }

    fn state(&self) -> PathBuf {
        self.p("state")
    }

    fn config(&self, key: &str, value: &str) {
        let mut c = git2::Config::open(&self.state().join("git/config")).unwrap();
        c.set_str(key, value).unwrap();
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_agentfs"));
        for (name, _) in std::env::vars_os() {
            let n = name.to_string_lossy();
            if n.starts_with("OTEL_") || n.starts_with("AGENTFS_") {
                cmd.env_remove(name);
            }
        }
        cmd.arg("--state")
            .arg(self.state())
            .env("AGENTFS_REPLICA", "test-0");
        cmd
    }

    fn agentfs(&self, args: &[&str]) -> Output {
        self.command().args(args).output().unwrap()
    }

    fn log_path(&self) -> PathBuf {
        log_dir().join(format!("{}.log", self.name))
    }

    /// Starts `agentfs mount` and waits until the kernel lists the mount.
    fn mount(&self, extra: &[&str]) -> Mounted {
        self.mount_with(extra, |c| c)
    }

    /// `agentfs mount` with `extra` options, logging to the test's log.
    fn mount_command(&self, extra: &[&str]) -> Command {
        let mut log = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.log_path())
            .unwrap();
        writeln!(log, "---- mount {extra:?}").unwrap();
        let mut cmd = self.command();
        cmd.args([
            "mount",
            "--passthrough",
            &passthrough(),
            "--unmount-grace",
            "3s",
        ])
        .args(extra)
        .arg(self.backing())
        .arg(self.mnt())
        .stdout(log.try_clone().unwrap())
        .stderr(log)
        .stdin(Stdio::null());
        cmd
    }

    fn mount_with(&self, extra: &[&str], wrap: impl FnOnce(Command) -> Command) -> Mounted {
        let mut cmd = wrap(self.mount_command(extra));
        // The log is appended to across mounts: wait for this one's line.
        let before = startup_lines(&fs::read_to_string(self.log_path()).unwrap_or_default());
        let child = cmd.spawn().unwrap();
        let mounted = Mounted {
            child: Some(child),
            mnt: self.mnt(),
            log: self.log_path(),
        };
        mounted.wait_mounted(before);
        mounted
    }

    /// The branch's tree: path to content, mode.
    fn history(&self) -> Tree {
        let mut out = Tree::new();
        let Ok(repo) = git2::Repository::open_bare(self.state().join("git")) else {
            return out;
        };
        let Ok(reference) = repo.find_reference("refs/heads/agentfs/demo") else {
            return out;
        };
        let commit = reference.peel_to_commit().unwrap();
        commit
            .tree()
            .unwrap()
            .walk(git2::TreeWalkMode::PreOrder, |root, e| {
                if e.kind() == Some(git2::ObjectType::Blob) {
                    let blob = repo.find_blob(e.id()).unwrap();
                    let content = String::from_utf8_lossy(blob.content()).into_owned();
                    let item = match e.filemode() {
                        0o100755 => Exec(content),
                        0o120000 => Link(content),
                        _ => File(content),
                    };
                    out.insert(format!("{root}{}", e.name().unwrap()), item);
                }
                git2::TreeWalkResult::Ok
            })
            .unwrap();
        out
    }

    /// The head commit's author name and message.
    fn head(&self) -> (String, String) {
        let repo = git2::Repository::open_bare(self.state().join("git")).unwrap();
        let commit = repo
            .find_reference("refs/heads/agentfs/demo")
            .unwrap()
            .peel_to_commit()
            .unwrap();
        (
            commit.author().name().unwrap_or("").to_owned(),
            commit.message().unwrap_or("").to_owned(),
        )
    }

    /// The branch head commit id.
    fn head_id(&self) -> String {
        let repo = git2::Repository::open_bare(self.state().join("git")).unwrap();
        repo.refname_to_id("refs/heads/agentfs/demo")
            .unwrap()
            .to_string()
    }

    /// The id of the head commit's first parent.
    fn parent_id(&self) -> String {
        let repo = git2::Repository::open_bare(self.state().join("git")).unwrap();
        let head = repo
            .find_reference("refs/heads/agentfs/demo")
            .unwrap()
            .peel_to_commit()
            .unwrap();
        head.parent_id(0).unwrap().to_string()
    }

    /// Runs `agentfs` and expects success; its stdout.
    fn ok(&self, args: &[&str]) -> String {
        let out = self.agentfs(args);
        assert!(
            out.status.success(),
            "{args:?}: {}{}\nlog:\n{}",
            text(&out.stdout),
            text(&out.stderr),
            fs::read_to_string(self.log_path()).unwrap_or_default()
        );
        text(&out.stdout)
    }

    /// Waits until history is exactly `want`.
    fn expect(&self, want: &Tree) {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let got = self.history();
            if &got == want {
                return;
            }
            if Instant::now() > deadline {
                panic!(
                    "history never became {want:#?}\ngot {got:#?}\ndaemon log:\n{}",
                    fs::read_to_string(self.log_path()).unwrap_or_default()
                );
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Runs a shell command in the mount as root; it must succeed.
    fn sh(&self, script: &str) -> String {
        self.sh_as(None, script, true)
    }

    /// Runs a shell command in the mount as `uid` (and gid) if given.
    fn sh_as(&self, uid: Option<u32>, script: &str, must_succeed: bool) -> String {
        let mut cmd = match uid {
            None => Command::new("sh"),
            Some(uid) => {
                let mut c = Command::new("setpriv");
                c.args([
                    format!("--reuid={uid}"),
                    format!("--regid={uid}"),
                    "--clear-groups".into(),
                    "--".into(),
                    "sh".into(),
                ]);
                c
            }
        };
        let out = cmd
            .arg("-c")
            .arg(script)
            .current_dir(self.mnt())
            .output()
            .unwrap();
        if must_succeed {
            assert!(
                out.status.success(),
                "`{script}` failed: {}{}",
                text(&out.stdout),
                text(&out.stderr)
            );
        }
        text(&out.stdout)
    }
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// How many startup lines (`mounted ...`) a daemon log holds.
fn startup_lines(log: &str) -> usize {
    log.lines().filter(|l| l.starts_with("mounted ")).count()
}

fn is_mounted(mnt: &Path) -> bool {
    let info = fs::read_to_string("/proc/self/mountinfo").unwrap();
    let mnt = mnt.to_str().unwrap();
    info.lines()
        .any(|l| l.split(' ').nth(4).is_some_and(|m| m == mnt))
}

/// A running `agentfs mount`.
struct Mounted {
    child: Option<Child>,
    mnt: PathBuf,
    log: PathBuf,
}

impl Mounted {
    fn log(&self) -> String {
        fs::read_to_string(&self.log).unwrap_or_default()
    }

    /// Waits until the kernel lists the mount and the daemon has printed
    /// a startup line beyond the `before` already in the log.
    fn wait_mounted(&self, before: usize) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !(is_mounted(&self.mnt) && startup_lines(&self.log()) > before) {
            if Instant::now() > deadline {
                panic!("never mounted; log:\n{}", self.log());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn pid(&self) -> rustix::process::Pid {
        rustix::process::Pid::from_child(self.child.as_ref().unwrap())
    }

    fn signal(&self, sig: rustix::process::Signal) {
        rustix::process::kill_process(self.pid(), sig).unwrap();
    }

    fn wait_exit(&mut self, timeout: Duration) -> ExitStatus {
        let deadline = Instant::now() + timeout;
        let child = self.child.as_mut().unwrap();
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                self.child = None;
                return status;
            }
            if Instant::now() > deadline {
                panic!("the daemon did not exit; log:\n{}", self.log());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// SIGTERM; the daemon must exit 0 and leave nothing mounted.
    fn stop(mut self) -> String {
        self.signal(rustix::process::Signal::TERM);
        let status = self.wait_exit(Duration::from_secs(30));
        let log = self.log();
        assert!(status.success(), "exit {status:?}; log:\n{log}");
        assert!(!is_mounted(&self.mnt), "still mounted; log:\n{log}");
        // No open fell back to the daemon: with passthrough on, every
        // regular file's I/O really went to its backing file.
        assert!(
            !log.contains("passthrough refused"),
            "passthrough fell back; log:\n{log}"
        );
        log
    }
}

impl Drop for Mounted {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
        if is_mounted(&self.mnt) {
            let _ = Command::new("umount").arg("-l").arg(&self.mnt).status();
        }
    }
}

// ---- design §18 test 1: no gaps ----

#[test]
#[ignore = "needs /dev/fuse and root: ci/fuse-tests.sh"]
fn no_gaps() {
    let v = Vol::new("no_gaps");
    let m = v.mount(&[]);
    let mut want = Tree::new();

    // An editor's save.
    v.sh("printf old > f && printf new > .f.swp && mv .f.swp f");
    want.insert("f".into(), f("new"));
    v.expect(&want);

    v.sh("printf more >> f");
    want.insert("f".into(), f("newmore"));
    v.expect(&want);

    v.sh("truncate -s 3 f");
    want.insert("f".into(), f("new"));
    v.expect(&want);

    v.sh("mkdir -p dir/sub && printf x > dir/sub/x && printf y > dir/y && mv dir dir2");
    want.insert("dir2/sub/x".into(), f("x"));
    want.insert("dir2/y".into(), f("y"));
    v.expect(&want);

    v.sh("rm -r dir2");
    want.retain(|k, _| !k.starts_with("dir2/"));
    v.expect(&want);

    v.sh("ln f hard");
    want.insert("hard".into(), f("new"));
    v.expect(&want);

    v.sh("chmod +x f");
    want.insert("f".into(), Exec("new".into()));
    want.insert("hard".into(), Exec("new".into()));
    v.expect(&want);

    v.sh("ln -s f sym");
    want.insert("sym".into(), Link("f".into()));
    v.expect(&want);

    // Unlink of an open file.
    v.sh("printf g > g");
    want.insert("g".into(), f("g"));
    v.expect(&want);
    v.sh("exec 3>>g; rm g; echo x >&3; exec 3>&-");
    want.remove("g");
    v.expect(&want);

    // Rename over an open file.
    v.sh("printf old > h && printf fresh > h2");
    v.sh("exec 3>>h; mv h2 h; echo late >&3; exec 3>&-");
    want.insert("h".into(), f("fresh"));
    v.expect(&want);

    // Log rotation while the writer is open.
    v.sh(
        "i=0; while [ $i -lt 300 ]; do echo line$i; i=$((i+1)); sleep 0.002; done >> app.log & \
         sleep 0.3; mv app.log app.log.1; printf 'fresh\\n' > app.log; wait",
    );
    let rotated = fs::read_to_string(v.backing().join("app.log.1")).unwrap();
    assert_eq!(rotated.lines().count(), 300, "{rotated}");
    want.insert("app.log".into(), f("fresh\n"));
    want.insert("app.log.1".into(), File(rotated));
    v.expect(&want);

    // A writer other than root.
    v.sh("mkdir u && chown 10000:10000 u");
    v.sh_as(Some(UID), "printf mine > u/by-10000", true);
    want.insert("u/by-10000".into(), f("mine"));
    v.expect(&want);
    let (author, message) = v.head();
    assert_eq!(author, "uid-10000", "{message}");

    // The backing tree has the same content.
    assert_eq!(fs::read_to_string(v.backing().join("f")).unwrap(), "new");
    m.stop();
}

// ---- design §18 test 2: before return ----

#[test]
#[ignore = "needs /dev/fuse and root: ci/fuse-tests.sh"]
fn recorded_before_close_returns() {
    let v = Vol::new("before_return");
    // Nothing is committed before the kill, so only the journal can carry
    // the record over.
    v.config("agentfs.commitWindow", "10m");
    let mut m = v.mount(&[]);
    v.sh("printf data > f");
    // close has returned: kill the daemon at once.
    m.signal(rustix::process::Signal::KILL);
    m.wait_exit(Duration::from_secs(10));

    // A stale mount point is refused with the way out.
    let err = rustix::fs::stat(v.mnt()).unwrap_err();
    assert_eq!(err, rustix::io::Errno::NOTCONN);
    let out = v.agentfs(&[
        "mount",
        v.backing().to_str().unwrap(),
        v.mnt().to_str().unwrap(),
    ]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        text(&out.stderr).contains("stale FUSE mount"),
        "{}",
        text(&out.stderr)
    );
    // The kernel released the lock with the process.
    let status = text(&v.agentfs(&["status"]).stdout);
    assert!(status.contains("lock:      free"), "{status}");
    assert!(
        Command::new("umount")
            .arg("-l")
            .arg(v.mnt())
            .status()
            .unwrap()
            .success()
    );

    assert!(v.history().is_empty(), "committed before the kill");

    // Remount: recovery finds the record made before close returned in
    // the journal, and commits it.
    v.config("agentfs.commitWindow", "50ms");
    let m = v.mount(&[]);
    let log = m.log();
    let line = log.lines().rfind(|l| l.starts_with("mounted ")).unwrap();
    // The shell's redirection closes the file twice (dup2, then close), so
    // there may be a record of the empty file before the one with data.
    let recovered: usize = line
        .split(", ")
        .find_map(|p| p.strip_suffix(" uncommitted record(s) recovered"))
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("{line}"));
    assert!(recovered >= 1, "{line}");
    v.expect(&tree(&[("f", f("data"))]));
    assert_eq!(fs::read_to_string(v.backing().join("f")).unwrap(), "data");
    m.stop();
}

// ---- design §18 test 4: any user ----

#[test]
#[ignore = "needs /dev/fuse and root: ci/fuse-tests.sh"]
fn any_user() {
    let v = Vol::new("any_user");
    let m = v.mount(&[]);
    v.sh("mkdir u && chown 10000:10000 u");
    v.sh_as(Some(UID), "printf a > u/f", true);
    let meta = fs::metadata(v.backing().join("u/f")).unwrap();
    assert_eq!((meta.uid(), meta.gid()), (UID, UID), "owned by its creator");
    v.expect(&tree(&[("u/f", f("a"))]));
    assert_eq!(v.head().0, "uid-10000");

    // Ordinary permission semantics.
    v.sh("printf r > rootfile && chmod 644 rootfile");
    v.sh_as(Some(UID), "printf x >> rootfile", false);
    assert_eq!(
        fs::read_to_string(v.backing().join("rootfile")).unwrap(),
        "r"
    );
    let denied = Command::new("setpriv")
        .args(["--reuid=10000", "--regid=10000", "--clear-groups", "--"])
        .args(["sh", "-c", "printf x >> rootfile"])
        .current_dir(v.mnt())
        .output()
        .unwrap();
    assert!(!denied.status.success());
    assert!(
        text(&denied.stderr).contains("Permission denied"),
        "{}",
        text(&denied.stderr)
    );

    // Root reads a 0600 file of uid 10000; chmod 600 persists.
    v.sh_as(Some(UID), "printf s > u/secret && chmod 600 u/secret", true);
    assert_eq!(v.sh("cat u/secret"), "s");
    let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o7777;
    assert_eq!(mode(&v.backing().join("u/secret")), 0o600);
    assert_eq!(mode(&v.mnt().join("u/secret")), 0o600);

    // chown and utimens pass through.
    v.sh("chown 10000 rootfile && TZ=UTC touch -d '2001-02-03 04:05:06' rootfile");
    let meta = fs::metadata(v.backing().join("rootfile")).unwrap();
    assert_eq!(meta.uid(), UID);
    assert_eq!(meta.mtime(), 981_173_106);

    // A setgid directory's group is inherited.
    v.sh("mkdir sg && chown 10000:4242 sg && chmod 2775 sg");
    v.sh_as(Some(UID), "printf z > sg/f && mkdir sg/d", true);
    assert_eq!(fs::metadata(v.backing().join("sg/f")).unwrap().gid(), 4242);
    let d = fs::metadata(v.backing().join("sg/d")).unwrap();
    assert_eq!(d.gid(), 4242);
    assert_eq!(d.permissions().mode() & 0o2000, 0o2000, "setgid kept");
    m.stop();

    // chmod 600 survives a remount.
    let m = v.mount(&[]);
    assert_eq!(mode(&v.mnt().join("u/secret")), 0o600);
    m.stop();
}

// ---- design §18 test 5: invisible ----

fn walk(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for e in fs::read_dir(&dir).unwrap() {
            let e = e.unwrap();
            let rel = e
                .path()
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            if e.file_type().unwrap().is_dir() {
                stack.push(e.path());
            }
            out.push(rel);
        }
    }
    out.sort();
    out
}

#[test]
#[ignore = "needs /dev/fuse and root: ci/fuse-tests.sh"]
fn nothing_of_the_state_area_is_visible() {
    let v = Vol::new("invisible");
    let creds = v.p("creds");
    fs::write(&creds, "username=bot\npassword=CRED-SECRET-4711\n").unwrap();
    v.config(
        "remote.origin.url",
        &format!("file://{}", v.p("remote.git").display()),
    );
    v.config("remote.origin.agentfsCredentials", creds.to_str().unwrap());
    git2::Repository::init_bare(v.p("remote.git")).unwrap();
    let m = v.mount(&[]);
    v.sh("mkdir -p a/b && printf one > a/b/one && printf two > two && ln -s two l");
    v.expect(&tree(&[
        ("a/b/one", f("one")),
        ("l", Link("two".into())),
        ("two", f("two")),
    ]));
    assert_eq!(walk(&v.mnt()), walk(&v.backing()));
    let grep = Command::new("grep")
        .args(["-r", "-l", "CRED-SECRET"])
        .arg(v.mnt())
        .output()
        .unwrap();
    assert!(grep.stdout.is_empty(), "{}", text(&grep.stdout));
    for name in ["git", "journal", "pending", "meta", "HEAD", "objects"] {
        assert!(!v.mnt().join(name).exists(), "{name} visible");
    }
    // `..` from the mount root is the mount point's parent.
    let up = fs::metadata(v.mnt().join("..")).unwrap();
    let parent = fs::metadata(v.dir.path()).unwrap();
    assert_eq!((up.dev(), up.ino()), (parent.dev(), parent.ino()));
    // Nothing reachable through the mount is the state area.
    let state = fs::metadata(v.state()).unwrap();
    for rel in walk(&v.mnt()) {
        let meta = fs::symlink_metadata(v.mnt().join(&rel)).unwrap();
        assert_ne!(
            (meta.dev(), meta.ino()),
            (state.dev(), state.ino()),
            "{rel}"
        );
    }
    m.stop();
}

// ---- design §18 test 6: rules ----

#[test]
#[ignore = "needs /dev/fuse and root: ci/fuse-tests.sh"]
fn rules_through_the_mount() {
    let v = Vol::new("rules");
    let exclude = v.state().join("git/info/exclude");
    fs::create_dir_all(exclude.parent().unwrap()).unwrap();
    fs::write(&exclude, "*.log\n").unwrap();
    let m = v.mount(&[]);
    v.sh("printf a > a.log && printf t > t.txt");
    v.expect(&tree(&[("t.txt", f("t"))]));
    m.stop();

    // Ignored after it was recorded: still recorded.
    fs::write(&exclude, "*.log\n*.txt\n").unwrap();
    let m = v.mount(&[]);
    v.sh("printf t2 > t.txt && printf n > new.txt && printf x > x.dat");
    v.expect(&tree(&[("t.txt", f("t2")), ("x.dat", f("x"))]));

    // Tracked rule files cannot turn recording off by default, and are
    // content themselves.
    v.sh("printf '*\\n' > .gitignore && printf '* -agentfs\\n' > .gitattributes");
    v.sh("printf z > z.dat");
    v.expect(&tree(&[
        (".gitattributes", f("* -agentfs\n")),
        (".gitignore", f("*\n")),
        ("t.txt", f("t2")),
        ("x.dat", f("x")),
        ("z.dat", f("z")),
    ]));
    m.stop();
}

// ---- design §18 test 7: nested repositories ----

fn source_repo(v: &Vol) -> PathBuf {
    let src = v.p("src");
    let git = |args: &[&str]| {
        let ok = Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
            .args(["-c", "init.defaultBranch=main"])
            .args(args)
            .current_dir(v.dir.path())
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?}");
    };
    git(&["init", "-q", "src"]);
    fs::write(src.join("README"), "hello").unwrap();
    fs::create_dir_all(src.join("lib")).unwrap();
    fs::write(src.join("lib/code.rs"), "fn main() {}").unwrap();
    git(&["-C", "src", "add", "."]);
    git(&["-C", "src", "commit", "-q", "-m", "init"]);
    src
}

#[test]
#[ignore = "needs /dev/fuse and root: ci/fuse-tests.sh"]
fn nested_repositories() {
    let v = Vol::new("nested");
    let src = source_repo(&v);
    let m = v.mount(&[]);
    v.sh("printf k > keep");
    v.expect(&tree(&[("keep", f("k"))]));
    let src = src.to_str().unwrap();
    v.sh(&format!("git clone -q {src} work"));
    v.sh(&format!("git clone -q --bare {src} b.git"));
    v.sh("git init -q .");
    v.sh("printf n > new");
    v.expect(&tree(&[("keep", f("k")), ("new", f("n"))]));
    let log = m.stop();
    assert!(!log.contains("alarm:"), "{log}");

    v.config("agentfs.nested", "working-tree");
    let m = v.mount(&[]);
    v.sh(&format!("git clone -q {src} wt"));
    v.expect(&tree(&[
        ("keep", f("k")),
        ("new", f("n")),
        ("wt/README", f("hello")),
        ("wt/lib/code.rs", f("fn main() {}")),
    ]));
    let log = m.stop();
    assert!(!log.contains("alarm:"), "{log}");
}

// ---- workloads ----

fn sqlite(v: &Vol, db: &str, mode: &str) {
    let mut sql =
        format!("PRAGMA journal_mode={mode};\nCREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT);\n");
    for tx in 0..10 {
        sql.push_str("BEGIN;\n");
        for i in 0..100 {
            sql.push_str(&format!("INSERT INTO t(v) VALUES ('row {tx}-{i}');\n"));
        }
        sql.push_str("COMMIT;\n");
    }
    let script = v.p(&format!("{db}.sql"));
    fs::write(&script, sql).unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o644)).unwrap();
    v.sh_as(
        Some(UID),
        &format!("sqlite3 u/{db} < {} > /dev/null", script.display()),
        true,
    );
    check_sqlite(v, db);
}

fn check_sqlite(v: &Vol, db: &str) {
    let out = v.sh_as(
        Some(UID),
        &format!("sqlite3 u/{db} 'PRAGMA integrity_check; SELECT count(*) FROM t;'"),
        true,
    );
    assert_eq!(out, "ok\n1000\n", "{db}");
}

#[test]
#[ignore = "needs /dev/fuse and root: ci/fuse-tests.sh"]
fn sqlite_on_the_mount() {
    let v = Vol::new("sqlite");
    let m = v.mount(&[]);
    v.sh("mkdir u && chown 10000:10000 u");
    sqlite(&v, "wal.db", "WAL");
    sqlite(&v, "delete.db", "DELETE");
    m.stop();
    let m = v.mount(&[]);
    check_sqlite(&v, "wal.db");
    check_sqlite(&v, "delete.db");
    m.stop();
}

// ---- the fsync class (design §8) ----

/// The attribute line that puts SQLite databases in the fsync class.
const SQLITE_CLASS: &str = "*.db* agentfs.commit=fsync agentfs.group=sqlite agentfs.coalesce=1s\n";

/// Writes `db` in `mode` from one long-lived `sqlite3` process: `txs`
/// transactions of 50 rows, `pause` apart, so captures land while the
/// database (and its WAL) is open and in use.
fn sqlite_slowly(v: &Vol, db: &str, mode: &str, txs: u32, pause: &str) {
    v.sh_as(
        Some(UID),
        &format!(
            "( echo 'PRAGMA journal_mode={mode};'; echo 'PRAGMA synchronous=NORMAL;'; \
               echo 'CREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT);'; \
               i=0; while [ $i -lt {txs} ]; do \
                 echo 'BEGIN;'; \
                 j=0; while [ $j -lt 50 ]; do echo \"INSERT INTO t(v) VALUES ('$i-$j');\"; j=$((j+1)); done; \
                 echo 'COMMIT;'; sleep {pause}; i=$((i+1)); \
               done ) | sqlite3 u/{db} > /dev/null"
        ),
        true,
    );
}

/// Every version of `u/<db>` in the branch's history, with its `-wal` and
/// `-journal` from the same commit, restored into a scratch directory and
/// checked with `PRAGMA integrity_check`. Returns the distinct row counts
/// seen, each a whole number of transactions.
fn check_every_version(v: &Vol, db: &str) -> std::collections::BTreeSet<u64> {
    let repo = git2::Repository::open_bare(v.state().join("git")).unwrap();
    let mut walk = repo.revwalk().unwrap();
    walk.push_ref("refs/heads/agentfs/demo").unwrap();
    let mut seen = std::collections::BTreeSet::new();
    let mut versions = std::collections::BTreeSet::new();
    for id in walk {
        let tree = repo.find_commit(id.unwrap()).unwrap().tree().unwrap();
        let mut files = Vec::new();
        for suffix in ["", "-wal", "-journal", "-shm"] {
            let path = format!("u/{db}{suffix}");
            if let Ok(entry) = tree.get_path(Path::new(&path)) {
                assert_ne!(suffix, "-shm", "{path} was recorded");
                files.push((
                    suffix,
                    repo.find_blob(entry.id()).unwrap().content().to_vec(),
                ));
            }
        }
        if files.first().is_none_or(|(s, _)| !s.is_empty()) {
            continue;
        };
        if !versions.insert(files.clone()) {
            continue;
        }
        let scratch = tempfile::tempdir().unwrap();
        for (suffix, content) in &files {
            fs::write(scratch.path().join(format!("{db}{suffix}")), content).unwrap();
        }
        let out = Command::new("sqlite3")
            .arg(scratch.path().join(db))
            .arg("PRAGMA integrity_check; SELECT count(*) FROM t;")
            .output()
            .unwrap();
        let stdout = text(&out.stdout);
        let mut lines = stdout.lines();
        assert_eq!(
            lines.next(),
            Some("ok"),
            "{db} at {id:?}: {stdout}{}",
            text(&out.stderr),
            id = tree.id()
        );
        // A brand new database may not have its table yet.
        if let Some(count) = lines.next().and_then(|c| c.parse::<u64>().ok()) {
            assert_eq!(count % 50, 0, "{db}: a torn transaction ({count} rows)");
            seen.insert(count);
        }
    }
    seen
}

#[test]
#[ignore = "needs /dev/fuse and root: ci/fuse-tests.sh"]
fn sqlite_in_the_fsync_class() {
    let v = Vol::new("sqlite-fsync");
    fs::write(v.state().join("git/info/attributes"), SQLITE_CLASS).unwrap();
    let m = v.mount(&[]);
    v.sh("mkdir u && chown 10000:10000 u");
    sqlite_slowly(&v, "wal.db", "WAL", 12, "0.3");
    sqlite_slowly(&v, "delete.db", "DELETE", 12, "0.3");
    check_sqlite_rows(&v, "wal.db", 600);
    check_sqlite_rows(&v, "delete.db", 600);
    let log = m.stop();
    assert!(!log.contains("alarm:"), "{log}");

    for db in ["wal.db", "delete.db"] {
        let counts = check_every_version(&v, db);
        // The final version is whole, and captures landed mid-run.
        assert_eq!(counts.last(), Some(&600), "{db}: {counts:?}");
        assert!(counts.len() >= 2, "{db}: only {counts:?}");
    }
    // -shm never reaches the history; the journal and WAL of a closed
    // database are gone from the head.
    let head = v.history();
    assert!(head.contains_key("u/wal.db") && head.contains_key("u/delete.db"));
    assert!(head.keys().all(|p| !p.ends_with("-shm")), "{head:?}");
    assert!(!head.contains_key("u/wal.db-wal"), "{head:?}");
    assert!(!head.contains_key("u/delete.db-journal"), "{head:?}");
}

/// With a coalesce longer than the run, fsyncs after the first capture
/// nothing, yet closing the database records its final state whole.
#[test]
#[ignore = "needs /dev/fuse and root: ci/fuse-tests.sh"]
fn sqlite_fsync_class_records_on_close() {
    let v = Vol::new("sqlite-fsync-close");
    fs::write(
        v.state().join("git/info/attributes"),
        "*.db* agentfs.commit=fsync agentfs.group=sqlite agentfs.coalesce=1h\n",
    )
    .unwrap();
    let m = v.mount(&[]);
    v.sh("mkdir u && chown 10000:10000 u");
    sqlite_slowly(&v, "wal.db", "WAL", 3, "0");
    m.stop();
    let counts = check_every_version(&v, "wal.db");
    assert_eq!(counts.last(), Some(&150), "{counts:?}");
    let m = v.mount(&[]);
    check_sqlite_rows(&v, "wal.db", 150);
    m.stop();
}

fn check_sqlite_rows(v: &Vol, db: &str, rows: u64) {
    let out = v.sh_as(
        Some(UID),
        &format!("sqlite3 u/{db} 'PRAGMA integrity_check; SELECT count(*) FROM t;'"),
        true,
    );
    assert_eq!(out, format!("ok\n{rows}\n"), "{db}");
}
#[test]
#[ignore = "needs /dev/fuse and root: ci/fuse-tests.sh"]
fn git_works_on_the_mount() {
    let v = Vol::new("git");
    let m = v.mount(&[]);
    let script = "set -e
        git init -q proj
        cd proj
        git config user.name t
        git config user.email t@example.com
        mkdir -p src
        for i in $(seq 1 50); do echo \"line $i\" > src/f$i.txt; done
        git add .
        git commit -q -m one
        for i in $(seq 1 50); do echo more >> src/f$i.txt; done
        git commit -q -am two
        git mv src/f1.txt src/g1.txt
        git commit -q -m three
        git gc -q
        git fsck --no-progress
        git status --porcelain
        git log --oneline | wc -l";
    let out = v.sh(script);
    assert!(out.trim_end().ends_with('3'), "{out}");
    // proj is a nested repository: nothing of it is recorded.
    v.sh("printf r > recorded");
    v.expect(&tree(&[("recorded", f("r"))]));
    m.stop();
}

// ---- the daemon ----

#[test]
#[ignore = "needs /dev/fuse and root: ci/fuse-tests.sh"]
fn sigterm_with_a_file_open_for_writing() {
    let v = Vol::new("sigterm_open");
    let mut m = v.mount(&[]);
    // A writer that never closes its file.
    let mut writer = Command::new("sh")
        .args(["-c", "exec 3>held; printf data >&3; exec sleep 60"])
        .current_dir(v.mnt())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(500));
    m.signal(rustix::process::Signal::TERM);
    let status = m.wait_exit(Duration::from_secs(30));
    let log = m.log();
    assert!(status.success(), "{log}");
    assert!(log.contains("detached lazily"), "{log}");
    assert!(
        log.contains("final sweep: 1 open file(s) recorded"),
        "{log}"
    );
    assert!(!is_mounted(&v.mnt()));
    let _ = writer.kill();
    let _ = writer.wait();
    v.expect(&tree(&[("held", f("data"))]));
    let status = text(&v.agentfs(&["status"]).stdout);
    assert!(status.contains("backlog:   0 record(s)"), "{status}");
}

#[test]
#[ignore = "needs /dev/fuse and root: ci/fuse-tests.sh"]
fn an_external_unmount_stops_the_daemon() {
    let v = Vol::new("external_umount");
    let mut m = v.mount(&[]);
    v.sh("printf a > a");
    v.expect(&tree(&[("a", f("a"))]));
    assert!(
        Command::new("umount")
            .arg(v.mnt())
            .status()
            .unwrap()
            .success()
    );
    let status = m.wait_exit(Duration::from_secs(30));
    assert!(status.success(), "{}", m.log());
    assert!(m.log().contains("unmounted from outside"), "{}", m.log());
}

#[test]
#[ignore = "needs /dev/fuse and root: ci/fuse-tests.sh"]
fn the_lock_holds_while_mounted() {
    let v = Vol::new("lock");
    let m = v.mount(&[]);
    let root = v.backing().to_str().unwrap().to_owned();
    for args in [&["run"][..], &["flush"], &["--root", &root, "record", "f"]] {
        let out = v.agentfs(args);
        assert_eq!(
            out.status.code(),
            Some(75),
            "{args:?}: {}",
            text(&out.stderr)
        );
    }
    // `push` is not refused: the mount pushes for it (design §2.3).
    let out = v.agentfs(&["push", "--timeout", "10s"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(
        text(&out.stdout).contains("through the running daemon"),
        "{}",
        text(&out.stdout)
    );
    let out = v.agentfs(&["status"]);
    assert!(out.status.success());
    assert!(
        text(&out.stdout).contains("command=mount"),
        "{}",
        text(&out.stdout)
    );
    m.stop();
}

#[test]
#[ignore = "needs /dev/fuse and root: ci/fuse-tests.sh"]
fn the_startup_line_reports_passthrough() {
    let v = Vol::new("passthrough");
    let m = v.mount(&[]);
    v.sh("printf data > f && cat f > /dev/null");
    v.expect(&tree(&[("f", f("data"))]));
    let log = m.stop();
    let line = log
        .lines()
        .find(|l| l.starts_with("mounted "))
        .unwrap()
        .to_owned();
    // The CI job's log shows what the runner's kernel negotiated.
    println!("startup ({}): {line}", passthrough());
    match passthrough().as_str() {
        "off" => assert!(
            line.contains("passthrough: off (disabled by --passthrough off)"),
            "{line}"
        ),
        "on" => assert!(line.contains("passthrough: on,"), "{line}"),
        _ => assert!(line.contains("passthrough: "), "{line}"),
    }
}

#[test]
#[ignore = "needs /dev/fuse and root: ci/fuse-tests.sh"]
fn an_unprivileged_daemon() {
    let Ok(user) = std::env::var("AGENTFS_TEST_UNPRIV_USER") else {
        println!("AGENTFS_TEST_UNPRIV_USER not set: skipped");
        return;
    };
    let id = |flag: &str| -> u32 {
        let out = Command::new("id").arg(flag).arg(&user).output().unwrap();
        text(&out.stdout).trim().parse().unwrap()
    };
    let (uid, gid) = (id("-u"), id("-g"));
    let v = Vol::new("unprivileged");
    let ok = Command::new("chown")
        .arg("-R")
        .arg(format!("{uid}:{gid}"))
        .arg(v.dir.path())
        .status()
        .unwrap()
        .success();
    assert!(ok);
    let log_path = v.log_path();
    let as_user = move |cmd: Command| {
        let mut wrapped = Command::new("setpriv");
        wrapped
            .arg(format!("--reuid={uid}"))
            .arg(format!("--regid={gid}"))
            .arg("--init-groups")
            .arg("--")
            .arg(cmd.get_program())
            .args(cmd.get_args());
        for (k, val) in cmd.get_envs() {
            match val {
                Some(val) => wrapped.env(k, val),
                None => wrapped.env_remove(k),
            };
        }
        let log = fs::OpenOptions::new().append(true).open(&log_path).unwrap();
        wrapped
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .stdin(Stdio::null());
        wrapped
    };
    if passthrough() == "on" {
        // Without CAP_SYS_ADMIN, `on` is refused before anything is
        // mounted.
        let status = as_user(v.mount_command(&[])).status().unwrap();
        let log = fs::read_to_string(v.log_path()).unwrap();
        assert_eq!(status.code(), Some(1), "{log}");
        assert!(
            log.contains("--passthrough on: the daemon lacks CAP_SYS_ADMIN"),
            "{log}"
        );
        assert!(!is_mounted(&v.mnt()));
        return;
    }
    let m = v.mount_with(&[], as_user);
    // Root writes through another user's allow_other mount.
    v.sh("printf r > by-root");
    v.expect(&tree(&[("by-root", f("r"))]));
    let meta = fs::metadata(v.backing().join("by-root")).unwrap();
    assert_eq!(meta.uid(), uid, "a non-root daemon creates files as itself");
    let log = m.stop();
    let line = log.lines().find(|l| l.starts_with("mounted ")).unwrap();
    println!("unprivileged startup: {line}");
    if passthrough() != "off" {
        assert!(
            line.contains("passthrough: off (the daemon lacks CAP_SYS_ADMIN"),
            "{line}"
        );
    }
}

// ---- extended attributes and stray .git directories ----

#[test]
#[ignore = "needs /dev/fuse and root: ci/fuse-tests.sh"]
fn xattrs_through_the_mount() {
    let v = Vol::new("xattrs");
    let m = v.mount(&[]);
    v.sh("printf x > f && mkdir d");
    for target in ["f", "d"] {
        v.sh(&format!("setfattr -n user.agentfs -v hello {target}"));
        let got = v.sh(&format!("getfattr --only-values -n user.agentfs {target}"));
        assert_eq!(got, "hello", "{target}");
        // It is the backing inode's attribute.
        let mut buf = [0u8; 64];
        let n =
            rustix::fs::getxattr(v.backing().join(target), "user.agentfs", &mut buf[..]).unwrap();
        assert_eq!(&buf[..n], b"hello", "{target}");
        let listed = v.sh(&format!("getfattr -d -m - {target}"));
        assert!(listed.contains("user.agentfs=\"hello\""), "{listed}");
        v.sh(&format!("setfattr -x user.agentfs {target}"));
        assert!(
            rustix::fs::getxattr(v.backing().join(target), "user.agentfs", &mut buf[..]).is_err()
        );
    }
    // Extended attributes are not content: nothing new is recorded.
    v.expect(&tree(&[("f", f("x"))]));
    m.stop();
}

#[test]
#[ignore = "needs /dev/fuse and root: ci/fuse-tests.sh"]
fn a_dot_git_that_is_no_repository_alarms() {
    let v = Vol::new("stray_dot_git");
    let m = v.mount(&[]);
    v.sh("mkdir -p notes/.git && printf r > notes/.git/report.txt && printf k > keep");
    v.expect(&tree(&[("keep", f("k"))]));
    let log = m.stop();
    let alarms: Vec<&str> = log.lines().filter(|l| l.starts_with("alarm:")).collect();
    assert_eq!(alarms.len(), 1, "{log}");
    assert!(alarms[0].contains("notes/.git/report.txt"), "{log}");
}

// ---- design §15, §18 test 11: recovery ----

#[test]
#[ignore = "needs /dev/fuse and root: ci/fuse-tests.sh"]
fn restore_through_the_mount() {
    let v = Vol::new("restore_through_the_mount");
    let m = v.mount(&[]);
    v.sh("chown 10000:10000 .");
    v.sh_as(
        Some(UID),
        "printf v1 > f && mkdir d && printf x1 > d/x",
        true,
    );
    v.expect(&tree(&[("d/x", f("x1")), ("f", f("v1"))]));
    let c1 = v.head_id();
    v.sh_as(Some(UID), "printf v2 > f && printf n > d/new", true);
    v.expect(&tree(&[
        ("d/new", f("n")),
        ("d/x", f("x1")),
        ("f", f("v2")),
    ]));
    let c2 = v.head_id();

    // A file, while mounted: the mount shows it at once, history has one
    // new commit, authored by the operator (root here).
    let out = v.ok(&["restore", "f", &c1]);
    assert!(out.contains("  write f\n"), "{out}");
    assert!(
        out.contains("restored 1 path(s), removed 0: commit "),
        "{out}"
    );
    assert_eq!(fs::read_to_string(v.mnt().join("f")).unwrap(), "v1");
    v.expect(&tree(&[
        ("d/new", f("n")),
        ("d/x", f("x1")),
        ("f", f("v1")),
    ]));
    assert_eq!(v.parent_id(), c2, "one commit");
    let (author, message) = v.head();
    assert_eq!(author, "uid-0");
    assert!(message.starts_with("restore f from "), "{message}");
    assert!(message.contains("Agentfs-Trigger: restore\n"), "{message}");
    assert!(
        message.contains(&format!("Agentfs-Restore: {c1}\n")),
        "{message}"
    );
    assert!(message.contains("Agentfs-Uid: 0\n"), "{message}");
    assert!(!message.contains("Agentfs-Uid: 10000"), "{message}");
    assert!(
        out.contains(&v.head_id()),
        "the reply names the commit: {out}"
    );
    // Rewritten in place: still the workload's file.
    assert_eq!(fs::metadata(v.mnt().join("f")).unwrap().uid(), UID);

    // A directory: a removal too. Then a dry run and a no-op.
    let out = v.ok(&["restore", "--dry-run", "d", &c1]);
    assert!(
        out.contains("remove d/new") && out.contains("dry run"),
        "{out}"
    );
    assert!(v.mnt().join("d/new").exists());
    v.ok(&["restore", "d", &c1]);
    assert!(!v.mnt().join("d/new").exists());
    v.expect(&tree(&[("d/x", f("x1")), ("f", f("v1"))]));
    let out = v.ok(&["restore", ".", &c1]);
    assert!(out.contains("already matches"), "{out}");
    // A refusal says why and exits 1 (an unrecorded write through the
    // mount: restore_refuses_an_unrecorded_write_through_the_mount).
    let out = v.agentfs(&["restore", "nothing-here", &c1]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    assert!(text(&out.stderr).contains("nothing at nothing-here"));
    m.stop();
}

#[test]
#[ignore = "needs /dev/fuse and root: ci/fuse-tests.sh"]
fn revert_through_the_mount() {
    let v = Vol::new("revert_through_the_mount");
    let m = v.mount(&[]);
    v.sh("chown 10000:10000 .");
    v.sh_as(
        Some(UID),
        "mkdir d && printf y > d/y && chmod 750 d/y && printf k > keep",
        true,
    );
    v.expect(&tree(&[("d/y", Exec("y".into())), ("keep", f("k"))]));
    v.sh_as(Some(UID), "printf x > x", true);
    v.expect(&tree(&[
        ("d/y", Exec("y".into())),
        ("keep", f("k")),
        ("x", f("x")),
    ]));
    let add = v.head_id();
    v.sh_as(Some(UID), "rm d/y", true);
    v.expect(&tree(&[("keep", f("k")), ("x", f("x"))]));
    let delete = v.head_id();

    // Revert the add: x is removed.
    let out = v.ok(&["revert", &add]);
    assert!(out.contains("  remove x\n"), "{out}");
    assert!(!v.mnt().join("x").exists());
    v.expect(&tree(&[("keep", f("k"))]));
    let (_, message) = v.head();
    assert!(
        message.contains(&format!("Agentfs-Revert: {add}\n")),
        "{message}"
    );
    assert!(message.contains("Agentfs-Trigger: restore\n"), "{message}");

    // Revert the delete: d/y comes back, owned like its directory, with
    // its exec bit.
    v.ok(&["revert", &delete]);
    v.expect(&tree(&[("d/y", Exec("y".into())), ("keep", f("k"))]));
    let meta = fs::metadata(v.mnt().join("d/y")).unwrap();
    assert_eq!((meta.uid(), meta.gid()), (UID, UID), "the parent's owner");
    assert_eq!(meta.mode() & 0o777, 0o755);
    assert_eq!(fs::read_to_string(v.mnt().join("d/y")).unwrap(), "y");
    m.stop();
}

#[test]
#[ignore = "needs /dev/fuse and root: ci/fuse-tests.sh"]
fn restore_refuses_a_file_open_for_writing() {
    let v = Vol::new("restore_open_for_writing");
    let m = v.mount(&[]);
    v.sh("printf v1 > f");
    v.expect(&tree(&[("f", f("v1"))]));
    let c1 = v.head_id();
    v.sh("printf v2 > f");
    v.expect(&tree(&[("f", f("v2"))]));

    // A writer holding f open (without writing).
    let mut holder = Command::new("sh")
        .arg("-c")
        .arg("exec 3>>f; echo ready; exec sleep 60")
        .current_dir(v.mnt())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    {
        use std::io::BufRead as _;
        let mut line = String::new();
        std::io::BufReader::new(holder.stdout.as_mut().unwrap())
            .read_line(&mut line)
            .unwrap();
        assert_eq!(line.trim(), "ready");
    }
    let out = v.agentfs(&["restore", "f", &c1]);
    let err = text(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(
        err.contains("open for writing through the mount: f"),
        "{err}"
    );
    assert!(err.contains("--force"), "{err}");
    assert_eq!(fs::read_to_string(v.mnt().join("f")).unwrap(), "v2");

    v.ok(&["restore", "--force", "f", &c1]);
    assert_eq!(fs::read_to_string(v.mnt().join("f")).unwrap(), "v1");
    v.expect(&tree(&[("f", f("v1"))]));
    let _ = holder.kill();
    let _ = holder.wait();
    m.stop();
}

#[test]
#[ignore = "needs /dev/fuse and root: ci/fuse-tests.sh"]
fn restore_with_the_workload_stopped() {
    let v = Vol::new("restore_stopped");
    let m = v.mount(&[]);
    v.sh("chown 10000:10000 .");
    v.sh_as(Some(UID), "printf v1 > f && printf o > other", true);
    v.expect(&tree(&[("f", f("v1")), ("other", f("o"))]));
    let c1 = v.head_id();
    v.sh_as(Some(UID), "printf v2 > f && rm other", true);
    v.expect(&tree(&[("f", f("v2"))]));
    m.stop();

    // No mount: direct mode writes the backing tree and records it.
    let backing = v.backing().to_str().unwrap().to_owned();
    let out = v.ok(&["--root", &backing, "restore", ".", &c1]);
    assert!(
        out.contains("restored 2 path(s), removed 0: commit "),
        "{out}"
    );
    v.expect(&tree(&[("f", f("v1")), ("other", f("o"))]));
    let (author, message) = v.head();
    assert_eq!(author, "uid-0");
    assert!(message.contains("Agentfs-Restore: "), "{message}");
    // The rewritten file keeps its owner; the recreated one gets its
    // directory's.
    assert_eq!(fs::metadata(v.backing().join("f")).unwrap().uid(), UID);
    assert_eq!(fs::metadata(v.backing().join("other")).unwrap().uid(), UID);

    // Remounted, the tree and history agree.
    let m = v.mount(&[]);
    assert_eq!(fs::read_to_string(v.mnt().join("f")).unwrap(), "v1");
    assert_eq!(fs::read_to_string(v.mnt().join("other")).unwrap(), "o");
    v.sh("printf v3 > f");
    v.expect(&tree(&[("f", f("v3")), ("other", f("o"))]));
    m.stop();
}

/// A file a workload has written through the mount and still holds open
/// is a change not recorded yet: refused even with --force, and kept. The
/// check's own read records it, so a retry finds it in history.
#[test]
#[ignore = "needs /dev/fuse and root: ci/fuse-tests.sh"]
fn restore_refuses_an_unrecorded_write_through_the_mount() {
    let v = Vol::new("restore_unrecorded_write");
    let m = v.mount(&[]);
    v.sh("printf v1 > f");
    v.expect(&tree(&[("f", f("v1"))]));
    let c1 = v.head_id();
    v.sh("printf v2 > f");
    v.expect(&tree(&[("f", f("v2"))]));
    let head = v.head_id();

    // A writer that has written f and holds it open: nothing recorded.
    // Every close of a descriptor for f is a change boundary the mount
    // records, including a copy closed by fork and exec (or by a shell
    // redirection), so the writer is a process that never forks.
    let mut holder = Command::new("perl")
        .arg("-e")
        .arg(
            r#"open(my $f, ">", "f") or die "$!"; syswrite($f, "dirty") == 5 or die "$!";
               $| = 1; print "ready\n"; sleep 60;"#,
        )
        .current_dir(v.mnt())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    {
        use std::io::BufRead as _;
        let mut line = String::new();
        std::io::BufReader::new(holder.stdout.as_mut().unwrap())
            .read_line(&mut line)
            .unwrap();
        assert_eq!(line.trim(), "ready");
    }
    let out = v.agentfs(&["restore", "--force", "f", &c1]);
    let err = text(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(err.contains("f differs from history"), "{err}");
    assert!(err.contains("--force does not override"), "{err}");
    assert_eq!(fs::read_to_string(v.mnt().join("f")).unwrap(), "dirty");
    // The check's own read of f through the mount was a change boundary
    // (its close is a FLUSH of a changed inode), so the write is in
    // history now: nothing unrecorded is left to lose, and only the
    // open handle remains, which --force overrides.
    v.expect(&tree(&[("f", f("dirty"))]));
    assert_ne!(v.head_id(), head);
    let out = v.agentfs(&["restore", "f", &c1]);
    let err = text(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(
        err.contains("open for writing through the mount: f"),
        "{err}"
    );
    v.ok(&["restore", "--force", "f", &c1]);
    assert_eq!(fs::read_to_string(v.mnt().join("f")).unwrap(), "v1");
    v.expect(&tree(&[("f", f("v1"))]));
    // The commit before the restore holds the workload's write.
    let repo = git2::Repository::open_bare(v.state().join("git")).unwrap();
    let parent = repo
        .find_commit(git2::Oid::from_str(&v.parent_id()).unwrap())
        .unwrap();
    let blob = parent.tree().unwrap().get_name("f").unwrap().id();
    assert_eq!(repo.find_blob(blob).unwrap().content(), b"dirty");
    let _ = holder.kill();
    let _ = holder.wait();
    m.stop();
}

/// Only root and the daemon's user may use the control socket: another
/// uid is refused by its peer credentials when it can reach the socket
/// (CAP_DAC_OVERRIDE), and cannot reach it otherwise.
#[test]
#[ignore = "needs /dev/fuse and root: ci/fuse-tests.sh"]
fn restore_by_another_user_is_refused() {
    let v = Vol::new("restore_other_user");
    let m = v.mount(&[]);
    v.sh("printf v1 > f");
    v.expect(&tree(&[("f", f("v1"))]));
    let c1 = v.head_id();
    v.sh("printf v2 > f");
    v.expect(&tree(&[("f", f("v2"))]));
    let head = v.head_id();
    // A copy of the binary uid 10000 can execute wherever the build is.
    let bin = v.p("agentfs-bin");
    fs::copy(env!("CARGO_BIN_EXE_agentfs"), &bin).unwrap();
    fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
    let as_other = |caps: &[&str]| {
        let mut cmd = Command::new("setpriv");
        cmd.args([
            format!("--reuid={UID}"),
            format!("--regid={UID}"),
            "--clear-groups".into(),
        ])
        .args(caps)
        .arg("--")
        .arg(&bin)
        .arg("--state")
        .arg(v.state())
        .args(["restore", "f", c1.as_str()])
        .env("AGENTFS_REPLICA", "test-0")
        .env_remove("AGENTFS_ROOT");
        cmd.output().unwrap()
    };
    // Through the socket: the peer's uid is refused.
    let out = as_other(&["--inh-caps=+dac_override", "--ambient-caps=+dac_override"]);
    let err = text(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(
        err.contains(&format!(
            "uid {UID} may not use this mount's control socket"
        )),
        "{err}"
    );
    // Without it: the socket's directory is closed to it.
    let out = as_other(&[]);
    let err = text(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(
        err.contains("cannot be used") || err.contains("ermission denied"),
        "{err}"
    );
    assert_eq!(fs::read_to_string(v.mnt().join("f")).unwrap(), "v2");
    assert_eq!(v.head_id(), head);
    m.stop();
}

// ---- design §17.1: probes, and a sidecar restarted after a crash ----

/// The mountinfo line for `mnt`, as the runner's kernel reports it.
fn mountinfo_line(mnt: &Path) -> Option<String> {
    let info = fs::read_to_string("/proc/self/mountinfo").unwrap();
    let mnt = mnt.to_str().unwrap();
    info.lines()
        .rfind(|l| l.split(' ').nth(4).is_some_and(|m| m == mnt))
        .map(str::to_owned)
}

#[test]
#[ignore = "needs /dev/fuse and root: ci/fuse-tests.sh"]
fn probe_hold_and_detach_after_a_crash() {
    let v = Vol::new("probe_hold");
    let mnt = v.mnt();
    let mnt_s = mnt.to_str().unwrap().to_owned();
    let mut m = v.mount(&[]);
    // The kernel's view, for the CI log.
    println!("mountinfo: {:?}", mountinfo_line(&mnt));
    let out = v.agentfs(&["probe", &mnt_s]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(text(&out.stdout), "live agentfs mount (agentfs:demo)\n");
    v.sh("printf before > before");
    v.expect(&tree(&[("before", f("before"))]));

    // The daemon dies: the mount point is stale.
    m.signal(rustix::process::Signal::KILL);
    m.wait_exit(Duration::from_secs(10));
    assert_eq!(
        rustix::fs::stat(&mnt).unwrap_err(),
        rustix::io::Errno::NOTCONN
    );
    let out = v.agentfs(&["probe", &mnt_s]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(text(&out.stdout), "stale agentfs mount (daemon gone)\n");
    let out = v.agentfs(&["probe", "--allow-stale", &mnt_s]);
    assert_eq!(out.status.code(), Some(0));

    // The default still refuses, with the advice.
    let out = v.agentfs(&["mount", v.backing().to_str().unwrap(), &mnt_s]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        text(&out.stderr).contains("stale FUSE mount"),
        "{}",
        text(&out.stderr)
    );

    // Hold: mounts nothing, takes no lock, keeps the mount point stale.
    let child = v
        .mount_command(&["--on-stale-mount", "hold"])
        .spawn()
        .unwrap();
    let mut held = Mounted {
        child: Some(child),
        mnt: mnt.clone(),
        log: v.log_path(),
    };
    let deadline = Instant::now() + Duration::from_secs(20);
    while !held.log().contains("Holding until SIGTERM") {
        assert!(
            Instant::now() < deadline,
            "never held; log:\n{}",
            held.log()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    std::thread::sleep(Duration::from_millis(300));
    assert!(is_mounted(&mnt), "{}", held.log());
    assert_eq!(
        rustix::fs::stat(&mnt).unwrap_err(),
        rustix::io::Errno::NOTCONN
    );
    let status = text(&v.agentfs(&["status"]).stdout);
    assert!(status.contains("lock:      free"), "{status}");
    let out = v.agentfs(&["probe", "--allow-stale", &mnt_s]);
    assert_eq!(out.status.code(), Some(0));

    // SIGTERM: the stale mount is detached and the hold exits 1.
    held.signal(rustix::process::Signal::TERM);
    let status = held.wait_exit(Duration::from_secs(20));
    let log = held.log();
    assert_eq!(status.code(), Some(1), "{log}");
    assert!(
        log.contains(&format!("detached stale mount {mnt_s}")),
        "{log}"
    );
    assert!(!is_mounted(&mnt), "{log}");
    let out = v.agentfs(&["probe", &mnt_s]);
    assert_eq!(text(&out.stdout), "not mounted\n");

    // The next mount recovers what was recorded before the kill.
    let m = v.mount(&["--on-stale-mount", "hold"]);
    v.expect(&tree(&[("before", f("before"))]));
    v.sh("printf after > after");
    v.expect(&tree(&[("after", f("after")), ("before", f("before"))]));
    m.stop();
}

/// Runs `cmd` under `setpriv` with CAP_SYS_ADMIN dropped from the bounding
/// set, so root's exec gets every capability but that one.
fn without_sys_admin(cmd: Command) -> Command {
    let mut wrapped = Command::new("setpriv");
    wrapped
        .args(["--bounding-set=-sys_admin", "--inh-caps=-sys_admin", "--"])
        .arg(cmd.get_program())
        .args(cmd.get_args());
    for (name, value) in cmd.get_envs() {
        match value {
            Some(value) => wrapped.env(name, value),
            None => wrapped.env_remove(name),
        };
    }
    wrapped
}

/// How many agentfs mounts the kernel lists at `mnt` (stacked layers).
fn layers(mnt: &Path) -> usize {
    let info = fs::read_to_string("/proc/self/mountinfo").unwrap();
    let mnt = mnt.to_str().unwrap();
    info.lines()
        .filter(|l| l.split(' ').nth(4).is_some_and(|m| m == mnt) && l.contains(" agentfs:"))
        .count()
}

/// Kills the daemon with SIGKILL and checks the mount point went stale.
fn crash(m: &mut Mounted) {
    m.signal(rustix::process::Signal::KILL);
    m.wait_exit(Duration::from_secs(10));
    assert_eq!(
        rustix::fs::stat(&m.mnt).unwrap_err(),
        rustix::io::Errno::NOTCONN
    );
}

/// `--on-stale-mount recover` (design §17.1): a daemon killed with -9 is
/// replaced by one stacked over its stale mount, which every copy of the
/// stale mount then reaches: a bind of it (as a kubelet subPath) and a
/// slave mount namespace (as a workload container). Refused without
/// CAP_SYS_ADMIN and for fronts; a hold detaches every stale layer.
#[test]
#[ignore = "needs /dev/fuse and root: ci/fuse-tests.sh"]
fn recover_stacks_over_a_crashed_mount() {
    let v = Vol::new("recover");
    let mnt = v.mnt();
    let mnt_s = mnt.to_str().unwrap().to_owned();
    let subpath = v.p("subpath");
    fs::create_dir(&subpath).unwrap();
    let mut first = v.mount(&[]);
    // Shared, as under a Bidirectional emptyDir, so copies are peers and
    // slaves of it.
    let ok = |cmd: &mut Command| {
        let out = cmd.output().unwrap();
        assert!(out.status.success(), "{cmd:?}: {}", text(&out.stderr));
    };
    ok(Command::new("mount").arg("--make-shared").arg(&mnt));
    ok(Command::new("mount").arg("--bind").arg(&mnt).arg(&subpath));
    let mut holder = Command::new("unshare")
        .args(["-m", "--propagation", "slave", "sleep", "600"])
        .spawn()
        .unwrap();
    let holder_pid = holder.id().to_string();
    v.sh("printf before > before");
    v.expect(&tree(&[("before", f("before"))]));
    crash(&mut first);
    assert_eq!(
        rustix::fs::stat(&subpath).unwrap_err(),
        rustix::io::Errno::NOTCONN
    );

    // Refused without CAP_SYS_ADMIN, and for fronts; the stale mount is
    // left as it is.
    let out = without_sys_admin(v.mount_command(&["--on-stale-mount", "recover"]))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    assert!(
        text(&out.stderr).contains("CAP_SYS_ADMIN"),
        "{}",
        text(&out.stderr)
    );
    let out = v
        .mount_command(&[
            "--on-stale-mount",
            "recover",
            "--history-service",
            "unix:/nonexistent.sock",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    assert!(
        text(&out.stderr).contains("fronts"),
        "{}",
        text(&out.stderr)
    );
    assert_eq!(layers(&mnt), 1);

    // Recover: stacked over the stale mount, it serves every copy.
    let mut second = v.mount(&["--on-stale-mount", "recover"]);
    let log = second.log();
    assert!(
        log.contains(&format!(
            "stacked over a stale mount left by a crashed daemon at {mnt_s} (1 stale layer(s) below)"
        )),
        "{log}"
    );
    assert_eq!(layers(&mnt), 2);
    let out = v.agentfs(&["probe", &mnt_s]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stdout));
    assert_eq!(fs::read_to_string(mnt.join("before")).unwrap(), "before");
    v.sh("printf after > after");
    v.expect(&tree(&[("after", f("after")), ("before", f("before"))]));
    assert_eq!(fs::read_to_string(subpath.join("after")).unwrap(), "after");
    fs::write(subpath.join("via-subpath"), "s").unwrap();
    let out = Command::new("nsenter")
        .args(["-m", "-t", &holder_pid, "cat"])
        .arg(mnt.join("after"))
        .output()
        .unwrap();
    assert_eq!(text(&out.stdout), "after", "{}", text(&out.stderr));
    let out = Command::new("nsenter")
        .args(["-m", "-t", &holder_pid, "sh", "-c"])
        .arg(format!("printf ns > {mnt_s}/via-namespace"))
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", text(&out.stderr));
    v.expect(&tree(&[
        ("after", f("after")),
        ("before", f("before")),
        ("via-namespace", f("ns")),
        ("via-subpath", f("s")),
    ]));

    // A second crash leaves two stale layers; a hold detaches both.
    crash(&mut second);
    assert_eq!(layers(&mnt), 2);
    let child = v
        .mount_command(&["--on-stale-mount", "hold"])
        .spawn()
        .unwrap();
    let mut held = Mounted {
        child: Some(child),
        mnt: mnt.clone(),
        log: v.log_path(),
    };
    let deadline = Instant::now() + Duration::from_secs(20);
    while !held.log().contains("Holding until SIGTERM") {
        assert!(
            Instant::now() < deadline,
            "never held; log:\n{}",
            held.log()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    held.signal(rustix::process::Signal::TERM);
    assert_eq!(held.wait_exit(Duration::from_secs(20)).code(), Some(1));
    assert!(
        held.log()
            .contains(&format!("detached stale mount {mnt_s} (2 layer(s))")),
        "{}",
        held.log()
    );
    assert_eq!(layers(&mnt), 0, "{}", held.log());

    // Recover again, stop: the stop detaches the stale layer under it.
    let mut third = v.mount(&[]);
    crash(&mut third);
    let fourth = v.mount(&["--on-stale-mount", "recover"]);
    v.sh("printf last > last");
    let log = fourth.stop();
    assert!(
        log.contains(&format!("detached 1 stale layer(s) at {mnt_s}")),
        "{log}"
    );
    assert_eq!(layers(&mnt), 0, "{log}");

    let _ = holder.kill();
    let _ = holder.wait();
    let _ = Command::new("umount").arg("-l").arg(&subpath).status();
    let _ = Command::new("umount").arg("-l").arg(&subpath).status();
}

/// With nothing stale, `recover` is a normal mount; files left in the bare
/// mount point are warned about (design §17.1).
#[test]
#[ignore = "needs /dev/fuse and root: ci/fuse-tests.sh"]
fn recover_on_a_healthy_mount_point_is_a_normal_mount() {
    let v = Vol::new("recover_healthy");
    fs::write(v.mnt().join("left-behind"), "x").unwrap();
    let m = v.mount(&["--on-stale-mount", "recover"]);
    let log = m.log();
    assert!(!log.contains("stacked over"), "{log}");
    assert!(log.contains("is not empty"), "{log}");
    assert_eq!(layers(&v.mnt()), 1);
    m.stop();
    fs::remove_file(v.mnt().join("left-behind")).unwrap();
}

#[test]
#[ignore = "needs /dev/fuse and root: ci/fuse-tests.sh"]
fn mount_creates_a_missing_mount_point() {
    let v = Vol::new("create_mnt");
    fs::remove_dir(v.mnt()).unwrap();
    let m = v.mount(&[]);
    assert!(m.log().contains("created the mount point"), "{}", m.log());
    v.sh("printf x > x");
    v.expect(&tree(&[("x", f("x"))]));
    m.stop();
    assert!(v.mnt().is_dir());
}

/// `--seal-mountpoint` (design §17.1): the bare mount point is root's with
/// mode 0000, so between mounts a non-root writer is refused; the mount on
/// top is unaffected, and a sealed mount point mounts again.
#[test]
#[ignore = "needs /dev/fuse and root: ci/fuse-tests.sh"]
fn a_sealed_mount_point_refuses_writes_between_mounts() {
    use std::os::unix::fs::MetadataExt as _;

    let v = Vol::new("seal");
    let m = v.mount(&["--seal-mountpoint"]);
    v.sh("printf x > x");
    v.expect(&tree(&[("x", f("x"))]));
    m.stop();
    let meta = fs::metadata(v.mnt()).unwrap();
    assert_eq!((meta.mode() & 0o7777, meta.uid()), (0, 0));
    let probe = "if printf y > y; then echo wrote; else echo refused; fi 2>/dev/null";
    assert_eq!(v.sh_as(Some(10000), probe, false), "refused\n");
    assert!(!v.mnt().join("y").exists());
    let m = v.mount(&["--seal-mountpoint"]);
    assert_eq!(v.sh("cat x"), "x");
    m.stop();
}

// ---- design §11.2: the final push at shutdown ----

/// Stopping `agentfs mount` pushes what the push interval had not yet sent
/// (design §11.2), here a whole hour of it.
#[test]
#[ignore = "needs /dev/fuse and root: ci/fuse-tests.sh"]
fn mount_pushes_its_final_commit_at_shutdown() {
    let v = Vol::new("final_push");
    let remote = v.p("remote.git");
    git2::Repository::init_bare(&remote).unwrap();
    v.config("remote.origin.url", &format!("file://{}", remote.display()));
    v.config("agentfs.pushInterval", "1h");
    let m = v.mount(&["--push-grace", "30s"]);
    v.sh("printf late > late.txt");
    v.expect(&tree(&[("late.txt", f("late"))]));
    let remote_head = || {
        git2::Repository::open_bare(&remote)
            .unwrap()
            .refname_to_id("refs/heads/agentfs/demo")
            .ok()
            .map(|id| id.to_string())
    };
    // Committed, but the next push is an hour away.
    assert_eq!(remote_head(), None);
    let log = m.stop();
    let head = v.head_id();
    assert!(
        log.contains(&format!("final push: origin: pushed none..{head}")),
        "{log}"
    );
    assert_eq!(remote_head(), Some(head));
}

// ---- design §18 test 15: the patch queue at mount (design §12) ----

/// Queues patch `n` in the volume's own repository: `parent` with `path`
/// set to `content`.
fn queue_patch(v: &Vol, n: u64, parent: &str, path: &str, content: &str) {
    let repo = git2::Repository::open_bare(v.state().join("git")).unwrap();
    let parent = repo
        .find_commit(git2::Oid::from_str(parent).unwrap())
        .unwrap();
    let mut index = git2::Index::new().unwrap();
    index.read_tree(&parent.tree().unwrap()).unwrap();
    let blob = repo.blob(content.as_bytes()).unwrap();
    index
        .add(&git2::IndexEntry {
            ctime: git2::IndexTime::new(0, 0),
            mtime: git2::IndexTime::new(0, 0),
            dev: 0,
            ino: 0,
            mode: 0o100644,
            uid: 0,
            gid: 0,
            file_size: 0,
            id: blob,
            flags: 0,
            flags_extended: 0,
            path: path.as_bytes().to_vec(),
        })
        .unwrap();
    let tree = repo.find_tree(index.write_tree_to(&repo).unwrap()).unwrap();
    let sig = git2::Signature::now("Ada", "ada@example.com").unwrap();
    let oid = repo
        .commit(None, &sig, &sig, &format!("patch {n}"), &tree, &[&parent])
        .unwrap();
    repo.reference(
        &format!("refs/agentfs/demo/inbox/{n:010}"),
        oid,
        true,
        "test",
    )
    .unwrap();
}

#[test]
#[ignore = "needs /dev/fuse and root: ci/fuse-tests.sh"]
fn patches_apply_before_the_mount_serves() {
    let v = Vol::new("patches_apply_before_the_mount_serves");
    let m = v.mount(&[]);
    v.sh("printf a0 > a.txt && printf b0 > b.txt");
    v.expect(&tree(&[("a.txt", f("a0")), ("b.txt", f("b0"))]));
    let base = v.head_id();
    v.sh("printf workload > b.txt");
    v.expect(&tree(&[("a.txt", f("a0")), ("b.txt", f("workload"))]));
    m.stop();
    queue_patch(&v, 1, &base, "a.txt", "a1");
    queue_patch(&v, 2, &base, "b.txt", "iac");
    queue_patch(&v, 3, &base, "c.txt", "c");

    // Patch 1 is there from the first read; 2 conflicts and blocks 3; the
    // volume mounts anyway.
    let m = v.mount(&[]);
    assert_eq!(fs::read_to_string(v.mnt().join("a.txt")).unwrap(), "a1");
    assert_eq!(
        fs::read_to_string(v.mnt().join("b.txt")).unwrap(),
        "workload"
    );
    assert!(!v.mnt().join("c.txt").exists());
    let (author, message) = v.head();
    assert_eq!(author, "Ada");
    assert!(message.contains("Agentfs-Inbox: 1\n"), "{message}");
    let log = m.log();
    assert!(log.contains("patches: blocked at 2 (conflict)"), "{log}");
    assert!(log.contains("alarm: "), "{log}");
    let status = v.ok(&["patch", "status", "--json"]);
    let doc: serde_json::Value = serde_json::from_str(&status).unwrap();
    assert_eq!(doc["status"]["blocked"]["reason"], "conflict");
    assert_eq!(doc["status"]["queue"].as_array().unwrap().len(), 2);
    // The workload goes on.
    v.sh("printf more > d.txt");
    v.expect(&tree(&[
        ("a.txt", f("a1")),
        ("b.txt", f("workload")),
        ("d.txt", f("more")),
    ]));
    m.stop();
}

// ---- design §14: two fronts, one history service ----

/// Starts `agentfs serve-history` for `v`, logging to its own log, and waits
/// for it to serve.
fn start_history_service(v: &Vol) -> Mounted {
    // A log of its own, apart from the fronts'.
    let path = log_dir().join(format!("{}-service.log", v.name));
    let mut log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .unwrap();
    writeln!(log, "---- serve-history").unwrap();
    let before = fs::read_to_string(&path)
        .unwrap_or_default()
        .matches("serving history for volume")
        .count();
    let child = v
        .command()
        .arg("--root")
        .arg(v.backing())
        .arg("serve-history")
        .env("AGENTFS_REPLICA", "history-0")
        .stdout(log.try_clone().unwrap())
        .stderr(log)
        .stdin(Stdio::null())
        .spawn()
        .unwrap();
    // Not a mount: `mnt` is never mounted, so Drop unmounts nothing.
    let service = Mounted {
        child: Some(child),
        mnt: v.p("no-mount"),
        log: path,
    };
    let deadline = Instant::now() + Duration::from_secs(20);
    while service.log().matches("serving history for volume").count() <= before {
        assert!(
            Instant::now() < deadline,
            "never served; log:\n{}",
            service.log()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    service
}

/// Starts a front mount of `v`'s backing tree at `mnt` as pod `pod`.
fn start_front(v: &Vol, mnt: &Path, pod: &str) -> Mounted {
    // A log of its own: its alarms are its own.
    let path = log_dir().join(format!("{}-{pod}.log", v.name));
    let socket = v.state().join("meta/history/history.sock");
    let mut log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .unwrap();
    writeln!(log, "---- front {pod}").unwrap();
    let before = startup_lines(&fs::read_to_string(&path).unwrap_or_default());
    let child = v
        .command()
        .args([
            "mount",
            "--passthrough",
            &passthrough(),
            "--unmount-grace",
            "3s",
        ])
        .arg("--history-service")
        .arg(&socket)
        .args(["--history-timeout", "3s", "--history-wait", "10s"])
        .arg(v.backing())
        .arg(mnt)
        .env("AGENTFS_REPLICA", pod)
        .env("AGENTFS_POD", pod)
        .env("AGENTFS_POD_UID", format!("{pod}-uid"))
        .env("AGENTFS_NAMESPACE", "agents")
        .stdout(log.try_clone().unwrap())
        .stderr(log)
        .stdin(Stdio::null())
        .spawn()
        .unwrap();
    let front = Mounted {
        child: Some(child),
        mnt: mnt.to_owned(),
        log: path,
    };
    front.wait_mounted(before);
    front
}

/// Writes `content` to `path` and returns what close(2) said.
fn write_and_close(path: &Path, content: &str) -> std::io::Result<()> {
    use std::os::fd::IntoRawFd as _;
    let mut file = fs::File::create(path)?;
    file.write_all(content.as_bytes())?;
    let fd = file.into_raw_fd();
    // SAFETY: `fd` came from `into_raw_fd`, so this is its only owner.
    if unsafe { libc::close(fd) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Every commit with a pod trailer, oldest first: committer and pod.
fn pod_commits(v: &Vol) -> Vec<(String, String)> {
    let repo = git2::Repository::open_bare(v.state().join("git")).unwrap();
    let mut walk = repo.revwalk().unwrap();
    walk.push_ref("refs/heads/agentfs/demo").unwrap();
    walk.set_sorting(git2::Sort::TOPOLOGICAL | git2::Sort::REVERSE)
        .unwrap();
    walk.filter_map(|id| {
        let commit = repo.find_commit(id.unwrap()).unwrap();
        let pod = commit
            .message()
            .unwrap_or("")
            .lines()
            .find_map(|l| l.strip_prefix("Agentfs-Pod: "))?
            .to_owned();
        Some((commit.committer().name().unwrap_or("").to_owned(), pod))
    })
    .collect()
}

#[test]
#[ignore = "needs /dev/fuse and root: ci/fuse-tests.sh"]
fn two_fronts_one_service() {
    let v = Vol::new("two-fronts");
    // The fronts' repairs are retried at each sweep.
    v.config("agentfs.sweepInterval", "1s");
    let mnt2 = v.p("mnt2");
    fs::create_dir_all(&mnt2).unwrap();
    fs::set_permissions(&mnt2, fs::Permissions::from_mode(0o755)).unwrap();

    // No service: a front exits without mounting.
    let out = v
        .command()
        .args(["mount", "--history-service"])
        .arg(v.state().join("meta/history/history.sock"))
        .args(["--history-wait", "1s"])
        .arg(v.backing())
        .arg(v.mnt())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    assert!(!is_mounted(&v.mnt()));

    let service = start_history_service(&v);
    let a = start_front(&v, &v.mnt(), "pod-a");
    let b = start_front(&v, &mnt2, "pod-b");
    write_and_close(&v.mnt().join("from-a.txt"), "a").unwrap();
    write_and_close(&mnt2.join("from-b.txt"), "b").unwrap();
    // One backing tree: each front sees the other's file.
    assert_eq!(fs::read_to_string(mnt2.join("from-a.txt")).unwrap(), "a");
    assert_eq!(fs::read_to_string(v.mnt().join("from-b.txt")).unwrap(), "b");
    v.expect(&tree(&[("from-a.txt", f("a")), ("from-b.txt", f("b"))]));
    let status = v.ok(&["status"]);
    assert!(status.contains("history:   serving fronts at"), "{status}");
    assert!(status.contains("command=mount pod=pod-a"), "{status}");
    assert!(status.contains("command=mount pod=pod-b"), "{status}");

    // A file open in A that B renames, or removes: A's close records it
    // under its current name, or nowhere (design §14).
    {
        write_and_close(&v.mnt().join("app.log"), "1").unwrap();
        write_and_close(&v.mnt().join("gone.log"), "1").unwrap();
        let mut moved = fs::OpenOptions::new()
            .append(true)
            .open(v.mnt().join("app.log"))
            .unwrap();
        let mut removed = fs::OpenOptions::new()
            .append(true)
            .open(v.mnt().join("gone.log"))
            .unwrap();
        fs::rename(mnt2.join("app.log"), mnt2.join("app.log.1")).unwrap();
        fs::remove_file(mnt2.join("gone.log")).unwrap();
        moved.write_all(b"2").unwrap();
        removed.write_all(b"2").unwrap();
        drop(moved);
        drop(removed);
        v.expect(&tree(&[
            ("from-a.txt", f("a")),
            ("from-b.txt", f("b")),
            ("app.log.1", f("12")),
        ]));
        fs::remove_file(mnt2.join("app.log.1")).unwrap();
    }

    // The service down: the boundary fails with EIO and an alarm.
    let mut service = service;
    service.signal(rustix::process::Signal::KILL);
    service.wait_exit(Duration::from_secs(10));
    let err = write_and_close(&v.mnt().join("lost.txt"), "x").unwrap_err();
    assert_eq!(err.raw_os_error(), Some(libc::EIO), "{err}");
    // A rename is a boundary too: EIO, though the tree has the new name.
    let err = fs::rename(v.mnt().join("from-a.txt"), v.mnt().join("moved.txt")).unwrap_err();
    assert_eq!(err.raw_os_error(), Some(libc::EIO), "{err}");
    assert!(v.backing().join("moved.txt").exists());
    // Front A's own log: the alarm is A's, not the service's or B's.
    let log = a.log();
    assert!(log.contains("alarm: "), "{log}");
    assert!(log.contains("is unavailable"), "{log}");

    // Back up: the fronts carry on without a restart.
    let service = start_history_service(&v);
    write_and_close(&mnt2.join("after.txt"), "after").unwrap();
    // A's repairs (the failed close and rename) reach history at its next
    // sweep: HEAD follows the tree again.
    let want = tree(&[
        ("from-b.txt", f("b")),
        ("lost.txt", f("x")),
        ("moved.txt", f("a")),
        ("after.txt", f("after")),
    ]);
    let deadline = Instant::now() + Duration::from_secs(30);
    while v.history() != want {
        assert!(
            Instant::now() < deadline,
            "history never caught up: {:?}\nfront a:\n{}",
            v.history(),
            a.log()
        );
        std::thread::sleep(Duration::from_millis(100));
    }

    a.stop();
    b.stop();
    service.stop();
    let commits = pod_commits(&v);
    assert!(
        commits.contains(&("pod-a".to_owned(), "pod-a".to_owned())),
        "{commits:?}"
    );
    assert!(
        commits.contains(&("pod-b".to_owned(), "pod-b".to_owned())),
        "{commits:?}"
    );
    assert!(
        commits.iter().all(|(c, p)| c == p),
        "a commit mixes pods: {commits:?}"
    );
}
